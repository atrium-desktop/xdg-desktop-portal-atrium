//! freedesktop application and content-type resolution for the AppChooser
//! portal.
//!
//! Everything here is hand-rolled against the Desktop Entry and MIME Apps
//! specifications so the portal stays free of GLib. The module is split by
//! spec area: [`desktop`] scans `.desktop` files and the
//! `mimeinfo.cache` globs2 index, [`mimeapps`] applies the
//! `mimeapps.list` Added/Removed/Default associations, and [`exec`]
//! expands the `Exec` field codes and launches detached. This file owns
//! the [`AppDirs`] XDG directory set that stitches them together and the
//! resolution order across the three sources.
//!
//! `.desktop` files are scanned from `$XDG_DATA_HOME/applications` and
//! each `$XDG_DATA_DIRS/applications` (nearer directories shadow farther
//! ones by desktop id), with `MimeType=` keys as the cache-miss fallback.
//! Writes are limited to `set_default_app`, which edits only
//! `$XDG_CONFIG_HOME/mimeapps.list` and preserves every unrelated line.
//!
//! All filesystem access goes through [`AppDirs`] so tests drive the logic
//! with fixture directories instead of the host system. Inputs are bounded:
//! desktop files and `mimeapps.list` past a fixed byte cap are skipped,
//! candidate lists are truncated to a screenful, and desktop ids must be
//! plain file names.
//!
//! Launching expands the Desktop Entry `Exec` field codes and spawns
//! detached. Entries with `Terminal=true` are refused by the caller (this
//! module cannot know which terminal emulator to prefer, and guessing
//! `$TERMINAL` silently would strand the user on systems without one); the
//! metadata codes `%i`/`%c`/`%k` need entry context the launch surface
//! deliberately does not take, so they expand to nothing.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

/// Desktop files and association files past this size are skipped; real
/// entries are a few KiB.
const MAX_ENTRY_BYTES: u64 = 256 * 1024;
/// Enumeration result cap: one screenful of candidates.
const MAX_LISTED_APPS: usize = 64;
/// Number of `.desktop` files parsed per `applications` directory.
const MAX_ENTRIES_PER_DIR: usize = 1024;
/// `globs2` databases past this size are ignored; the system database is
/// a few hundred KiB.
const MAX_GLOBS_BYTES: u64 = 1024 * 1024;
/// Glob rules read per database.
const MAX_GLOBS: usize = 16 * 1024;
/// Exec line and id length caps.
const MAX_EXEC_BYTES: usize = 8 * 1024;
const MAX_ID_BYTES: usize = 256;
/// A single launch carries at most this many URIs.
const MAX_LAUNCH_URIS: usize = 256;

/// One resolved desktop entry.
mod desktop;
mod exec;
mod mimeapps;

use desktop::{
    glob_matches, parse_desktop_file, parse_globs2, parse_grouped_lists, valid_content_type,
    valid_desktop_id,
};
use mimeapps::update_default_applications;

pub(crate) use exec::launch;
#[cfg(test)]
use exec::{expand_exec, split_exec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppInfo {
    /// The desktop file id, e.g. `org.foo.Bar.desktop`.
    pub(crate) id: String,
    pub(crate) name: String,
    /// The raw `Exec=` value; field codes are expanded only at launch.
    pub(crate) exec: String,
    pub(crate) icon: Option<String>,
    /// `Terminal=true`: the entry needs a terminal emulator the portal
    /// does not pick; the caller refuses to launch it.
    pub(crate) terminal: bool,
    /// `NoDisplay=true`: not enumerated, but still resolved when the
    /// portal frontend names the id explicitly.
    pub(crate) no_display: bool,
    /// The content types the entry's `MimeType=` declares.
    pub(crate) mime_types: Vec<String>,
}

/// The XDG directory set every lookup reads. Constructed from the process
/// environment in production and from fixture roots in tests.
#[derive(Debug, Clone)]
pub(crate) struct AppDirs {
    data_home: PathBuf,
    data_dirs: Vec<PathBuf>,
    config_home: PathBuf,
    config_dirs: Vec<PathBuf>,
}

impl AppDirs {
    /// The environment's directory set, with the spec's defaults for unset
    /// variables.
    pub(crate) fn from_env() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("/"));
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".config")))
            .unwrap_or_else(|| PathBuf::from("/"));
        let split = |variable: &str, default: &str| {
            std::env::var_os(variable)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    std::env::split_paths(&value)
                        .filter(|dir| !dir.as_os_str().is_empty())
                        .collect()
                })
                .unwrap_or_else(|| {
                    default
                        .split(':')
                        .map(PathBuf::from)
                        .collect::<Vec<PathBuf>>()
                })
        };
        Self {
            data_home,
            data_dirs: split("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
            config_home,
            config_dirs: split("XDG_CONFIG_DIRS", "/etc/xdg"),
        }
    }

    /// An explicit directory set for fixture-driven tests (and for the
    /// app_chooser module's tests, which live outside this module).
    #[cfg(test)]
    pub(crate) fn fixture(
        data_home: PathBuf,
        data_dirs: Vec<PathBuf>,
        config_home: PathBuf,
        config_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            data_home,
            data_dirs,
            config_home,
            config_dirs,
        }
    }

    /// `applications/` directories in shadowing order (nearest first).
    fn applications_dirs(&self) -> Vec<PathBuf> {
        std::iter::once(&self.data_home)
            .chain(&self.data_dirs)
            .map(|root| root.join("applications"))
            .collect()
    }

    /// `mimeapps.list` files in spec precedence order (most preferred
    /// first): config home, config dirs, data home, data dirs.
    fn mimeapps_files(&self) -> Vec<PathBuf> {
        std::iter::once(self.config_home.join("mimeapps.list"))
            .chain(
                self.config_dirs
                    .iter()
                    .map(|root| root.join("mimeapps.list")),
            )
            .chain(
                self.applications_dirs()
                    .iter()
                    .map(|dir| dir.join("mimeapps.list")),
            )
            .collect()
    }

    /// Parse one desktop file relative to an `applications/` root. Only
    /// plain file names are valid desktop ids here (no subdirectory ids).
    fn entry_at(&self, applications: &Path, id: &str) -> Option<AppInfo> {
        parse_desktop_file(&applications.join(id), id)
    }

    /// Look up one desktop id with shadowing applied. Ids with path
    /// separators or NUL never resolve.
    pub(crate) fn app_by_id(&self, id: &str) -> Option<AppInfo> {
        if !valid_desktop_id(id) {
            return None;
        }
        self.applications_dirs()
            .iter()
            .find_map(|dir| self.entry_at(dir, id))
    }

    /// Every desktop entry, nearer directories shadowing farther ones by
    /// id, in scan order. `Hidden=true` entries are dropped entirely.
    fn load_all(&self) -> Vec<AppInfo> {
        let mut seen = HashSet::new();
        let mut apps = Vec::new();
        for dir in self.applications_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name.ends_with(".desktop") && valid_desktop_id(name))
                .take(MAX_ENTRIES_PER_DIR)
                .collect();
            names.sort_unstable();
            for id in names {
                if !seen.insert(id.clone()) {
                    continue;
                }
                if let Some(app) = self.entry_at(&dir, &id) {
                    apps.push(app);
                }
            }
        }
        apps
    }

    /// The applications registered for `content_type`, best first, capped
    /// at [`MAX_LISTED_APPS`]. The list is the mimeapps.list Added
    /// associations (precedence order), then the `mimeinfo.cache` hits,
    /// then entries declaring the `MimeType=` key, minus every Removed
    /// association. `NoDisplay` entries are not enumerated; entries
    /// without an `Exec=` line are useless to the chooser and dropped.
    pub(crate) fn apps_for_content_type(&self, content_type: &str) -> Vec<AppInfo> {
        if !valid_content_type(content_type) {
            return Vec::new();
        }
        let apps = self.load_all();
        let by_id: HashMap<&str, &AppInfo> =
            apps.iter().map(|app| (app.id.as_str(), app)).collect();

        let mut added: Vec<String> = Vec::new();
        let mut removed: HashSet<String> = HashSet::new();
        for file in self.mimeapps_files() {
            let groups = parse_grouped_lists(&file);
            for id in groups
                .get("Added Associations")
                .and_then(|group| group.get(content_type))
                .into_iter()
                .flatten()
            {
                if !added.contains(id) {
                    added.push(id.clone());
                }
            }
            if let Some(ids) = groups
                .get("Removed Associations")
                .and_then(|group| group.get(content_type))
            {
                removed.extend(ids.iter().cloned());
            }
        }

        let mut ordered: Vec<String> = added;
        for dir in self.applications_dirs() {
            let cache = parse_grouped_lists(&dir.join("mimeinfo.cache"));
            if let Some(ids) = cache
                .get("MIME Cache")
                .and_then(|group| group.get(content_type))
            {
                for id in ids {
                    if !ordered.contains(id) {
                        ordered.push(id.clone());
                    }
                }
            }
        }
        for app in &apps {
            if app.mime_types.iter().any(|mime| mime == content_type) && !ordered.contains(&app.id)
            {
                ordered.push(app.id.clone());
            }
        }

        ordered
            .iter()
            .filter(|id| !removed.contains(*id))
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .filter(|app| !app.no_display && !app.exec.is_empty())
            .take(MAX_LISTED_APPS)
            .cloned()
            .collect()
    }

    /// The configured default for `content_type`: the first resolvable id
    /// in the first `[Default Applications]` entry, following mimeapps.list
    /// precedence.
    pub(crate) fn default_app(&self, content_type: &str) -> Option<AppInfo> {
        if !valid_content_type(content_type) {
            return None;
        }
        for file in self.mimeapps_files() {
            let groups = parse_grouped_lists(&file);
            let Some(ids) = groups
                .get("Default Applications")
                .and_then(|group| group.get(content_type))
            else {
                continue;
            };
            for id in ids {
                if let Some(app) = self.app_by_id(id) {
                    return Some(app);
                }
            }
        }
        None
    }

    /// The content type for a file *name* per the shared-mime-info glob
    /// databases (`mime/globs2` under each data root). All roots'
    /// databases apply; the highest priority wins, ties go to the nearer
    /// root, and matching is case-insensitive unless the glob carries the
    /// `cs` flag. `None` means no database matched — the caller falls back
    /// to `application/octet-stream`.
    ///
    /// The files are parsed per call: `globs2` is small, OpenURI requests
    /// are rare, and a process-global cache would pin the host's database
    /// into fixture-driven tests.
    pub(crate) fn content_type_for_filename(&self, name: &str) -> Option<String> {
        let roots = std::iter::once(&self.data_home).chain(&self.data_dirs);
        let mut best: Option<(u32, String)> = None;
        for root in roots {
            for (priority, glob, mime, case_sensitive) in parse_globs2(&root.join("mime/globs2")) {
                let better = best
                    .as_ref()
                    .is_none_or(|(best_priority, _)| priority > *best_priority);
                if better && glob_matches(&glob, name, case_sensitive) {
                    best = Some((priority, mime));
                }
            }
        }
        best.map(|(_, mime)| mime)
    }

    /// Record `desktop_id` as the default for `content_type` in
    /// `$XDG_CONFIG_HOME/mimeapps.list`, preserving every unrelated line
    /// and any previously listed fallback ids. The write is atomic
    /// (temp file plus rename) so a crash never leaves a torn file.
    pub(crate) fn set_default_app(&self, content_type: &str, desktop_id: &str) -> io::Result<()> {
        if !valid_content_type(content_type) || !valid_desktop_id(desktop_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid content type or desktop id",
            ));
        }
        let path = self.config_home.join("mimeapps.list");
        let existing = match std::fs::read(&path) {
            Ok(bytes) if bytes.len() as u64 <= MAX_ENTRY_BYTES => {
                String::from_utf8_lossy(&bytes).into_owned()
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "mimeapps.list exceeds the size cap",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let updated = update_default_applications(&existing, content_type, desktop_id);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self
            .config_home
            .join(format!(".mimeapps.list.tessera-{}", std::process::id()));
        std::fs::write(&temp, updated)?;
        std::fs::rename(&temp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture XDG tree under a unique temp root.
    struct Fixture {
        root: PathBuf,
        dirs: AppDirs,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "tessera-apps-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let dirs = AppDirs {
                data_home: root.join("data-home"),
                data_dirs: vec![root.join("data-a"), root.join("data-b")],
                config_home: root.join("config-home"),
                config_dirs: vec![root.join("config-a")],
            };
            Self { root, dirs }
        }

        /// Write a desktop file under one of the data roots.
        fn desktop(&self, root: &str, id: &str, body: &str) {
            let dir = self.root.join(root).join("applications");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(id), body).unwrap();
        }

        fn write(&self, relative: &str, body: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const EDITOR: &str = "[Desktop Entry]\nName=Foo Editor\nExec=foo-edit %U\nIcon=foo-edit\nMimeType=text/plain;text/markdown;\n";
    const VIEWER: &str = "[Desktop Entry]\nName=Bar Viewer\nExec=bar-view %u\nNoDisplay=true\nMimeType=text/plain;\n";

    #[test]
    fn nearer_directories_shadow_farther_ones() {
        let fixture = Fixture::new("shadow");
        fixture.desktop("data-a", "foo.desktop", EDITOR);
        fixture.desktop(
            "data-b",
            "foo.desktop",
            "[Desktop Entry]\nName=Wrong\nExec=wrong %u\n",
        );
        fixture.desktop(
            "data-b",
            "baz.desktop",
            "[Desktop Entry]\nName=Baz\nExec=baz\n",
        );

        let app = fixture.dirs.app_by_id("foo.desktop").unwrap();
        assert_eq!(app.name, "Foo Editor");
        assert!(fixture.dirs.app_by_id("baz.desktop").is_some());
        assert!(fixture.dirs.app_by_id("../escape.desktop").is_none());
        assert!(fixture.dirs.app_by_id("missing.desktop").is_none());
    }

    #[test]
    fn hidden_entries_are_dropped_and_terminal_is_recorded() {
        let fixture = Fixture::new("hidden");
        fixture.desktop(
            "data-home",
            "gone.desktop",
            "[Desktop Entry]\nName=Gone\nExec=gone\nHidden=true\n",
        );
        fixture.desktop(
            "data-home",
            "term.desktop",
            "[Desktop Entry]\nName=Term\nExec=term %f\nTerminal=true\n",
        );
        assert!(fixture.dirs.app_by_id("gone.desktop").is_none());
        assert!(fixture.dirs.app_by_id("term.desktop").unwrap().terminal);
    }

    #[test]
    fn associations_merge_cache_mimeapps_and_declared_types() {
        let fixture = Fixture::new("assoc");
        fixture.desktop("data-home", "editor.desktop", EDITOR);
        fixture.desktop(
            "data-a",
            "viewer.desktop",
            "[Desktop Entry]\nName=Bar Viewer\nExec=bar-view %u\nMimeType=text/plain;\n",
        );
        fixture.desktop(
            "data-a",
            "extra.desktop",
            "[Desktop Entry]\nName=Extra\nExec=extra %u\n",
        );
        fixture.write(
            "data-a/applications/mimeinfo.cache",
            "[MIME Cache]\ntext/plain=viewer.desktop;extra.desktop;\n",
        );
        // Added wins over the cache; a farther Removed still filters it.
        fixture.write(
            "config-home/mimeapps.list",
            "[Added Associations]\ntext/plain=editor.desktop;\n",
        );
        fixture.write(
            "data-b/applications/mimeapps.list",
            "[Removed Associations]\ntext/plain=extra.desktop;\n",
        );

        let apps = fixture.dirs.apps_for_content_type("text/plain");
        let ids: Vec<&str> = apps.iter().map(|app| app.id.as_str()).collect();
        // Added first, cache next (minus the removed), then MimeType=.
        assert_eq!(ids, ["editor.desktop", "viewer.desktop"]);
        assert!(fixture.dirs.apps_for_content_type("not-a-type").is_empty());
    }

    #[test]
    fn no_display_entries_resolve_only_when_explicit() {
        let fixture = Fixture::new("nodisplay");
        fixture.desktop("data-home", "viewer.desktop", VIEWER);
        assert!(fixture.dirs.apps_for_content_type("text/plain").is_empty());
        let explicit = fixture.dirs.app_by_id("viewer.desktop").unwrap();
        assert_eq!(explicit.name, "Bar Viewer");
        assert!(explicit.no_display);
    }

    #[test]
    fn default_app_follows_precedence_and_skips_missing_ids() {
        let fixture = Fixture::new("default");
        fixture.desktop("data-home", "editor.desktop", EDITOR);
        fixture.desktop("data-a", "viewer.desktop", VIEWER);
        fixture.write(
            "config-a/mimeapps.list",
            "[Default Applications]\ntext/plain=missing.desktop;viewer.desktop;\n",
        );
        fixture.write(
            "data-a/applications/mimeapps.list",
            "[Default Applications]\ntext/plain=editor.desktop;\n",
        );
        // The config dir beats the data dir; the missing id falls through.
        let default = fixture.dirs.default_app("text/plain").unwrap();
        assert_eq!(default.id, "viewer.desktop");
        assert!(fixture.dirs.default_app("image/png").is_none());
    }

    #[test]
    fn set_default_app_preserves_unrelated_content() {
        let fixture = Fixture::new("setdefault");
        fixture.write(
            "config-home/mimeapps.list",
            "[Default Applications]\ntext/html=browser.desktop;\n\n[Added Associations]\ntext/plain=editor.desktop;\n",
        );
        fixture
            .dirs
            .set_default_app("text/plain", "writer.desktop")
            .unwrap();
        fixture
            .dirs
            .set_default_app("text/plain", "editor.desktop")
            .unwrap();
        let text = std::fs::read_to_string(fixture.root.join("config-home/mimeapps.list")).unwrap();
        assert!(text.contains("text/html=browser.desktop;"));
        assert!(text.contains("[Added Associations]\ntext/plain=editor.desktop;"));
        // The second write keeps the first id as a fallback behind the new
        // default.
        assert!(text.contains("text/plain=editor.desktop;writer.desktop;"));
    }

    #[test]
    fn set_default_app_creates_the_file_and_group() {
        let fixture = Fixture::new("setdefault-new");
        fixture
            .dirs
            .set_default_app("image/png", "viewer.desktop")
            .unwrap();
        let text = std::fs::read_to_string(fixture.root.join("config-home/mimeapps.list")).unwrap();
        assert_eq!(text, "[Default Applications]\nimage/png=viewer.desktop;\n");
        assert!(
            fixture
                .dirs
                .set_default_app("image png", "viewer.desktop")
                .is_err()
        );
    }

    #[test]
    fn exec_splitting_honours_quoting_and_escapes() {
        assert_eq!(split_exec("foo bar baz"), ["foo", "bar", "baz"]);
        assert_eq!(split_exec("foo \"a b\" c"), ["foo", "a b", "c"]);
        assert_eq!(split_exec("foo a\\ b"), ["foo", "a b"]);
        assert_eq!(split_exec("foo \"a\\\"b\""), ["foo", "a\"b"]);
    }

    #[test]
    fn exec_expansion_covers_the_field_codes() {
        let uris = vec![
            "file:///tmp/a%20b.txt".to_owned(),
            "https://x.test/".to_owned(),
        ];
        assert_eq!(
            expand_exec("foo %U", &uris),
            ["foo", "file:///tmp/a%20b.txt", "https://x.test/"]
        );
        assert_eq!(
            expand_exec("foo %u", &uris),
            ["foo", "file:///tmp/a%20b.txt"]
        );
        assert_eq!(expand_exec("foo %F", &uris), ["foo", "/tmp/a b.txt"]);
        // The metadata codes %c/%i/%k expand to nothing, and with no URI
        // code present the URIs are appended.
        assert_eq!(
            expand_exec("foo --name=%c %% %i %k", &uris),
            [
                "foo",
                "--name=",
                "%",
                "file:///tmp/a%20b.txt",
                "https://x.test/"
            ]
        );
        // No URI code: the URIs are appended.
        assert_eq!(
            expand_exec("foo --flag", &uris),
            ["foo", "--flag", "file:///tmp/a%20b.txt", "https://x.test/"]
        );
        // An empty URI list still launches the bare program.
        assert_eq!(expand_exec("foo %u", &[]), ["foo"]);
    }

    #[test]
    fn launch_rejects_empty_exec_and_runs_true() {
        assert!(launch("", &[]).is_err());
        launch("true %u", &["file:///tmp/x".to_owned()]).unwrap();
    }

    #[test]
    fn globs2_resolve_by_priority_with_nearer_roots_winning_ties() {
        let fixture = Fixture::new("globs");
        fixture.write(
            "data-a/mime/globs2",
            "# comment\n50:*.txt:text/plain\n80:*.txt:text/x-special\n",
        );
        fixture.write(
            "data-home/mime/globs2",
            "80:*.txt:text/x-nearer\n60:*.md:text/markdown\n",
        );

        // Higher priority beats the nearer root's lower one.
        assert_eq!(
            fixture.dirs.content_type_for_filename("notes.TXT"),
            Some("text/x-nearer".to_owned()),
            "matching is case-insensitive without the cs flag"
        );
        assert_eq!(
            fixture.dirs.content_type_for_filename("readme.md"),
            Some("text/markdown".to_owned())
        );
        assert_eq!(fixture.dirs.content_type_for_filename("data.bin"), None);
    }

    #[test]
    fn globs2_honour_the_case_sensitive_flag_and_wildcards() {
        let fixture = Fixture::new("globs-cs");
        fixture.write(
            "data-home/mime/globs2",
            "50:*.CS:text/x-upper:cs\n50:makefile:text/x-makefile\n50:photo.???:image/x-any3\n",
        );
        let dirs = &fixture.dirs;
        assert_eq!(
            dirs.content_type_for_filename("FLAGS.CS"),
            Some("text/x-upper".to_owned())
        );
        assert_eq!(dirs.content_type_for_filename("flags.cs"), None);
        assert_eq!(
            dirs.content_type_for_filename("Makefile"),
            Some("text/x-makefile".to_owned())
        );
        assert_eq!(
            dirs.content_type_for_filename("photo.jpg"),
            Some("image/x-any3".to_owned())
        );
        assert_eq!(dirs.content_type_for_filename("photo.jpeg"), None);
    }
}
