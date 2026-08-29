//! Filesystem and filter logic for the FileChooser dialog, kept pure so
//! it is testable without a GPU or a window.

use std::path::{Path, PathBuf};

use aegis_portal_prompter::{FileFilter, FilterRuleKind};

/// One row in the directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    /// Display name (lossy; the path bytes stay exact).
    pub name: String,
    pub is_dir: bool,
}

/// Read `dir` into dialog rows: directories first, then files, each group
/// sorted case-insensitively. Dotfiles are hidden unless `show_hidden`;
/// files must pass `filter` (directories always stay visible for
/// navigation).
pub fn list_dir(
    dir: &Path,
    show_hidden: bool,
    filter: Option<&FileFilter>,
) -> Result<Vec<Entry>, String> {
    let read = std::fs::read_dir(dir)
        .map_err(|error| format!("could not open {}: {error}", dir.display()))?;
    let mut entries = Vec::new();
    for item in read {
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                log::warn!(
                    "prompter: skipping unreadable entry in {}: {error}",
                    dir.display()
                );
                continue;
            }
        };
        let name = item.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = item.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir && filter.is_some_and(|filter| !filter_allows(filter, &item.path())) {
            continue;
        }
        entries.push(Entry {
            path: item.path(),
            name,
            is_dir,
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(entries)
}

/// Whether a file passes a portal filter: any single rule matches (the
/// portal's rules within one filter are OR-ed, matching GTK).
pub fn filter_allows(filter: &FileFilter, path: &Path) -> bool {
    filter.rules.iter().any(|rule| match rule.kind {
        FilterRuleKind::Glob => path
            .file_name()
            .is_some_and(|name| glob_match(&rule.value, &name.to_string_lossy())),
        FilterRuleKind::Mime => mime_matches(&rule.value, path),
    })
}

/// A mime rule is the full essence (`image/png`) or a type prefix
/// (`image/*`). Types come from the filename's extension, the same source
/// GTK's mime filtering resolves through gio.
fn mime_matches(rule: &str, path: &Path) -> bool {
    let Some(guessed) = mime_guess::from_path(path).first() else {
        return false;
    };
    let essence = guessed.essence_str();
    if let Some(prefix) = rule.strip_suffix('*') {
        essence.starts_with(prefix)
    } else {
        essence == rule
    }
}

/// Match a filename against a glob pattern with `*` (any run), `?` (one
/// character), and `[...]` classes (ranges and `!` negation). Anything
/// else matches literally; a malformed class degrades to a literal `[`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    glob(&pattern, &name)
}

fn glob(pattern: &[char], name: &[char]) -> bool {
    match pattern.first() {
        None => name.is_empty(),
        Some('*') => glob(&pattern[1..], name) || (!name.is_empty() && glob(pattern, &name[1..])),
        Some('?') => !name.is_empty() && glob(&pattern[1..], &name[1..]),
        Some('[') => match parse_class(pattern) {
            Some((matches, rest)) if !name.is_empty() => matches(name[0]) && glob(rest, &name[1..]),
            _ => name.first() == Some(&'[') && glob(&pattern[1..], &name[1..]),
        },
        Some(&literal) => name.first() == Some(&literal) && glob(&pattern[1..], &name[1..]),
    }
}

/// Parse `[...]` at the head of `pattern` into a predicate and the rest of
/// the pattern. Returns `None` when the class is unterminated or empty.
fn parse_class(pattern: &[char]) -> Option<(impl Fn(char) -> bool + use<>, &[char])> {
    debug_assert_eq!(pattern.first(), Some(&'['));
    let mut items: Vec<(char, char)> = Vec::new();
    let mut index = 1;
    let negated = pattern.get(index) == Some(&'!');
    if negated {
        index += 1;
    }
    while index < pattern.len() && pattern[index] != ']' {
        let low = pattern[index];
        let high = if pattern.get(index + 1) == Some(&'-')
            && pattern.get(index + 2).is_some_and(|&c| c != ']')
        {
            index += 2;
            pattern[index]
        } else {
            low
        };
        items.push((low, high));
        index += 1;
    }
    if index >= pattern.len() || items.is_empty() {
        return None;
    }
    Some((
        move |c: char| items.iter().any(|&(low, high)| (low..=high).contains(&c)) != negated,
        &pattern[index + 1..],
    ))
}

/// The path's ancestor chain from the root down to itself, for the
/// location breadcrumb.
pub fn breadcrumbs(dir: &Path) -> Vec<PathBuf> {
    let mut chain: Vec<PathBuf> = dir.ancestors().map(Path::to_path_buf).collect();
    chain.reverse();
    chain
}

/// Which well-known folder a sidebar place points at (the icon choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceIcon {
    Home,
    Desktop,
    Documents,
    Downloads,
    Music,
    Pictures,
    Videos,
    Computer,
    Bookmark,
}

/// The sidebar section a place belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceSection {
    Standard,
    Pinned,
}

/// One sidebar shortcut to a well-known folder or pinned bookmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub name: String,
    pub path: PathBuf,
    pub icon: PlaceIcon,
    pub section: PlaceSection,
    pub custom: bool,
}

/// The XDG user dirs offered in the sidebar, in display order.
const USER_DIR_KEYS: &[(&str, &str, PlaceIcon)] = &[
    ("XDG_DESKTOP_DIR", "Desktop", PlaceIcon::Desktop),
    ("XDG_DOCUMENTS_DIR", "Documents", PlaceIcon::Documents),
    ("XDG_DOWNLOAD_DIR", "Downloads", PlaceIcon::Downloads),
    ("XDG_MUSIC_DIR", "Music", PlaceIcon::Music),
    ("XDG_PICTURES_DIR", "Pictures", PlaceIcon::Pictures),
    ("XDG_VIDEOS_DIR", "Videos", PlaceIcon::Videos),
];

/// The sidebar shortcuts: Home, the configured XDG user dirs that exist on
/// disk, the filesystem root, and user bookmarks from `~/.config/gtk-3.0/bookmarks`.
pub fn places() -> Vec<Place> {
    let mut result = Vec::new();
    if let Some(home) = std::env::home_dir() {
        result.push(Place {
            name: "Home".to_owned(),
            path: home.clone(),
            icon: PlaceIcon::Home,
            section: PlaceSection::Standard,
            custom: false,
        });
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let content = std::fs::read_to_string(config.join("user-dirs.dirs")).unwrap_or_default();
        let mut dirs = parse_user_dirs(&content, &home);
        if dirs.is_empty() {
            dirs = USER_DIR_KEYS
                .iter()
                .map(|(_, name, icon)| Place {
                    name: (*name).to_owned(),
                    path: home.join(name),
                    icon: *icon,
                    section: PlaceSection::Standard,
                    custom: false,
                })
                .collect();
        }
        for place in dirs {
            if place.path.is_dir() && !result.iter().any(|seen| seen.path == place.path) {
                result.push(place);
            }
        }
    }
    result.push(Place {
        name: "Computer".to_owned(),
        path: PathBuf::from("/"),
        icon: PlaceIcon::Computer,
        section: PlaceSection::Standard,
        custom: false,
    });

    let bookmarks = load_bookmarks(None);
    for b in bookmarks {
        if !result.iter().any(|seen| seen.path == b.path) {
            result.push(b);
        }
    }

    result
}

/// Parse `user-dirs.dirs` content into sidebar places in canonical order.
/// Values may be double-quoted and start with `$HOME`; relative tails are
/// resolved against `home`. Unknown keys and malformed lines are skipped.
fn parse_user_dirs(content: &str, home: &Path) -> Vec<Place> {
    let mut configured: Vec<(&str, PathBuf)> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        let path = if let Some(rest) = value.strip_prefix("$HOME") {
            home.join(rest.trim_start_matches('/'))
        } else if value.starts_with('/') {
            PathBuf::from(value)
        } else if value.is_empty() {
            continue;
        } else {
            home.join(value)
        };
        configured.push((key.trim(), path));
    }
    USER_DIR_KEYS
        .iter()
        .filter_map(|(key, name, icon)| {
            let path = configured.iter().find(|(found, _)| found == key)?.1.clone();
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| (*name).to_owned());
            Some(Place {
                name: label,
                path,
                icon: *icon,
                section: PlaceSection::Standard,
                custom: false,
            })
        })
        .collect()
}

/// Load user bookmarks from `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` (or `~/.config/gtk-3.0/bookmarks`).
pub fn load_bookmarks(home: Option<&Path>) -> Vec<Place> {
    let home_buf = std::env::home_dir();
    let Some(home) = home.or(home_buf.as_deref()) else {
        return Vec::new();
    };
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let bookmarks_file = config.join("gtk-3.0/bookmarks");
    let Ok(content) = std::fs::read_to_string(&bookmarks_file) else {
        return Vec::new();
    };
    parse_bookmarks(&content)
}

/// Parse the contents of a GTK bookmarks file.
/// Format: `file:///absolute/path/to/dir [optional custom label]`
pub fn parse_bookmarks(content: &str) -> Vec<Place> {
    let mut places = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (uri_part, label_part) = match line.split_once(' ') {
            Some((uri, label)) => (uri.trim(), Some(label.trim())),
            None => (line, None),
        };
        let Some(path_str) = uri_part.strip_prefix("file://") else {
            continue;
        };
        let decoded = url_decode(path_str);
        let path = PathBuf::from(decoded);
        if !path.is_dir() {
            continue;
        }
        let name = label_part
            .filter(|l| !l.is_empty())
            .map(|l| l.to_owned())
            .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| path.display().to_string());

        places.push(Place {
            name,
            path,
            icon: PlaceIcon::Bookmark,
            section: PlaceSection::Pinned,
            custom: true,
        });
    }
    places
}

/// Save user bookmarks to `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`.
pub fn save_bookmarks(home: Option<&Path>, places: &[Place]) -> std::io::Result<()> {
    let home_buf = std::env::home_dir();
    let Some(home) = home.or(home_buf.as_deref()) else {
        return Ok(());
    };
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let dir = config.join("gtk-3.0");
    std::fs::create_dir_all(&dir)?;
    let bookmarks_file = dir.join("bookmarks");
    let mut out = String::new();
    for place in places.iter().filter(|p| p.custom) {
        let encoded_path = url_encode(&place.path.to_string_lossy());
        let default_name = place.path.file_name().map(|n| n.to_string_lossy());
        if let Some(def) = default_name
            && place.name != def
        {
            out.push_str(&format!("file://{encoded_path} {}\n", place.name));
        } else {
            out.push_str(&format!("file://{encoded_path}\n"));
        }
    }
    std::fs::write(&bookmarks_file, out)
}

fn url_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2)
                && let Ok(val) =
                    u8::from_str_radix(std::str::from_utf8(&[h1, h2]).unwrap_or(""), 16)
            {
                bytes.push(val);
                continue;
            }
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}

/// Expand a leading `~` or `~/` to `home`; anything else passes through
/// unchanged (`~user` forms are not resolved).
pub fn expand_tilde(input: &str, home: Option<&Path>) -> PathBuf {
    match home {
        Some(home) if input == "~" => home.to_path_buf(),
        Some(home) if input.starts_with("~/") => home.join(&input[2..]),
        _ => PathBuf::from(input),
    }
}

/// Resolve `.` and `..` components lexically, without touching the
/// filesystem (symlinks are not resolved). A `..` at the root is dropped.
pub fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Whether `name` is usable as a new file or folder name inside one
/// directory (no separators, no special dot entries).
pub fn valid_filename(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\0') && name != "." && name != ".."
}

/// Back/forward navigation stacks, GTK-style: every explicit move pushes
/// the folder left behind onto `back` and clears `forward`.
#[derive(Debug, Default)]
pub struct History {
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
}

impl History {
    /// Record a move from `from` to `to`; a no-op when they are equal.
    pub fn push(&mut self, from: &Path, to: &Path) {
        if from != to {
            self.back.push(from.to_path_buf());
            self.forward.clear();
        }
    }

    /// The folder the Back button would show, if any.
    pub fn back(&self) -> Option<&Path> {
        self.back.last().map(PathBuf::as_path)
    }

    /// The folder the Forward button would show, if any.
    pub fn forward(&self) -> Option<&Path> {
        self.forward.last().map(PathBuf::as_path)
    }

    /// Move back from `current`, rotating the stacks. Returns the target.
    pub fn go_back(&mut self, current: &Path) -> Option<PathBuf> {
        let target = self.back.pop()?;
        self.forward.push(current.to_path_buf());
        Some(target)
    }

    /// Move forward from `current`, rotating the stacks. Returns the target.
    pub fn go_forward(&mut self, current: &Path) -> Option<PathBuf> {
        let target = self.forward.pop()?;
        self.back.push(current.to_path_buf());
        Some(target)
    }
}

/// The first entry whose name starts with `prefix`, ignoring case (the
/// GTK typeahead gesture). Folders and files match alike.
pub fn typeahead_index(entries: &[Entry], prefix: &str) -> Option<usize> {
    if prefix.is_empty() {
        return None;
    }
    let prefix = prefix.to_lowercase();
    entries
        .iter()
        .position(|entry| entry.name.to_lowercase().starts_with(&prefix))
}

/// Split a typed path into the directory part (through the last `/`) and
/// the tail being edited: `"~/Doc/x"` → `("~/Doc/", "x")`, `"x"` → `("", "x")`.
pub fn split_dir_tail(input: &str) -> (&str, &str) {
    match input.rfind('/') {
        Some(index) => input.split_at(index + 1),
        None => ("", input),
    }
}

/// The longest common leading substring of `candidates`, char-wise.
pub fn common_prefix<'a>(candidates: impl IntoIterator<Item = &'a str>) -> String {
    let mut iter = candidates.into_iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for candidate in iter {
        let candidate: Vec<char> = candidate.chars().collect();
        let shared = prefix
            .iter()
            .zip(&candidate)
            .take_while(|(left, right)| left == right)
            .count();
        prefix.truncate(shared);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_portal_prompter::FilterRule;

    fn filter(label: &str, rules: &[(&str, FilterRuleKind)]) -> FileFilter {
        FileFilter {
            label: label.into(),
            rules: rules
                .iter()
                .map(|(value, kind)| FilterRule {
                    kind: *kind,
                    value: (*value).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn glob_matches_stars_questions_and_classes() {
        assert!(glob_match("*.png", "shot.png"));
        assert!(!glob_match("*.png", "shot.jpg"));
        assert!(glob_match("shot.p?g", "shot.png"));
        assert!(glob_match("*.tar.*", "a.tar.gz"));
        assert!(glob_match("[a-z].txt", "b.txt"));
        assert!(!glob_match("[a-z].txt", "B.txt"));
        assert!(glob_match("[!a-z].txt", "B.txt"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("*.png", "png"));
        assert!(!glob_match("[.txt", "x.txt"));
        assert!(glob_match("[.txt", "[.txt"));
    }

    #[test]
    fn glob_respects_case_and_full_length() {
        assert!(!glob_match("*.PNG", "shot.png"));
        assert!(!glob_match("shot.pn", "shot.png"));
        assert!(!glob_match("shot.pngg", "shot.png"));
    }

    #[test]
    fn filter_rules_or_glob_and_mime() {
        let images = filter(
            "Images",
            &[
                ("*.png", FilterRuleKind::Glob),
                ("*.jpg", FilterRuleKind::Glob),
            ],
        );
        assert!(filter_allows(&images, Path::new("/tmp/a.png")));
        assert!(filter_allows(&images, Path::new("/tmp/b.jpg")));
        assert!(!filter_allows(&images, Path::new("/tmp/c.txt")));

        let mime = filter("Text", &[("text/plain", FilterRuleKind::Mime)]);
        assert!(mime_matches("text/*", Path::new("/tmp/a.txt")));
        assert!(filter_allows(&mime, Path::new("/tmp/a.txt")));
        assert!(!filter_allows(&mime, Path::new("/tmp/a.png")));
        assert!(!mime_matches("image/*", Path::new("/tmp/no-extension")));
    }

    #[test]
    fn breadcrumbs_walk_from_the_root() {
        assert_eq!(
            breadcrumbs(Path::new("/tmp")),
            vec![PathBuf::from("/"), PathBuf::from("/tmp")]
        );
        assert_eq!(breadcrumbs(Path::new("/")), vec![PathBuf::from("/")]);
    }

    #[test]
    fn user_dirs_parse_in_canonical_order() {
        let home = Path::new("/home/ming");
        let content = "\
# a comment
XDG_MUSIC_DIR=\"$HOME/music\"
XDG_DESKTOP_DIR=\"$HOME/Desktop\"
not a setting
XDG_DOWNLOAD_DIR=/mnt/data/downloads
XDG_UNKNOWN_DIR=\"$HOME/ignored\"
";
        let dirs = parse_user_dirs(content, home);
        let names: Vec<&str> = dirs.iter().map(|place| place.name.as_str()).collect();
        assert_eq!(names, ["Desktop", "downloads", "music"]);
        assert_eq!(dirs[0].path, PathBuf::from("/home/ming/Desktop"));
        assert_eq!(dirs[0].icon, PlaceIcon::Desktop);
        assert_eq!(dirs[1].path, PathBuf::from("/mnt/data/downloads"));
        assert_eq!(dirs[2].icon, PlaceIcon::Music);
    }

    #[test]
    fn user_dirs_resolve_relative_values_against_home() {
        let home = Path::new("/home/ming");
        let dirs = parse_user_dirs("XDG_DOCUMENTS_DIR=docs\n", home);
        assert_eq!(
            dirs,
            vec![Place {
                name: "docs".to_owned(),
                path: PathBuf::from("/home/ming/docs"),
                icon: PlaceIcon::Documents,
                section: PlaceSection::Standard,
                custom: false,
            }]
        );
    }

    #[test]
    fn tilde_expands_only_at_the_start() {
        let home = Path::new("/home/ming");
        assert_eq!(expand_tilde("~", Some(home)), PathBuf::from("/home/ming"));
        assert_eq!(
            expand_tilde("~/docs", Some(home)),
            PathBuf::from("/home/ming/docs")
        );
        assert_eq!(expand_tilde("/abs", Some(home)), PathBuf::from("/abs"));
        assert_eq!(expand_tilde("rel", Some(home)), PathBuf::from("rel"));
        assert_eq!(expand_tilde("~root", Some(home)), PathBuf::from("~root"));
        assert_eq!(expand_tilde("~/docs", None), PathBuf::from("~/docs"));
    }

    #[test]
    fn normalize_resolves_dots_lexically() {
        assert_eq!(
            normalize_lexical(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(normalize_lexical(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(normalize_lexical(Path::new("a/../b")), PathBuf::from("b"));
        assert_eq!(normalize_lexical(Path::new("/a/b/")), PathBuf::from("/a/b"));
    }

    #[test]
    fn filenames_reject_separators_and_dot_entries() {
        assert!(valid_filename("out.txt"));
        assert!(!valid_filename(""));
        assert!(!valid_filename("a/b"));
        assert!(!valid_filename("."));
        assert!(!valid_filename(".."));
        assert!(!valid_filename("a\0b"));
    }

    #[test]
    fn history_rotates_back_and_forward() {
        let mut history = History::default();
        history.push(Path::new("/a"), Path::new("/b"));
        history.push(Path::new("/b"), Path::new("/c"));
        assert_eq!(history.back(), Some(Path::new("/b")));
        assert_eq!(history.forward(), None);

        assert_eq!(history.go_back(Path::new("/c")), Some(PathBuf::from("/b")));
        assert_eq!(history.forward(), Some(Path::new("/c")));
        assert_eq!(
            history.go_forward(Path::new("/b")),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(history.back(), Some(Path::new("/b")));

        // A fresh move clears the forward stack.
        history.go_back(Path::new("/c"));
        history.go_back(Path::new("/b"));
        history.push(Path::new("/a"), Path::new("/z"));
        assert_eq!(history.forward(), None);
    }

    #[test]
    fn history_ignores_noop_moves() {
        let mut history = History::default();
        history.push(Path::new("/a"), Path::new("/a"));
        assert_eq!(history.back(), None);
        assert_eq!(history.go_back(Path::new("/a")), None);
    }

    #[test]
    fn typeahead_matches_case_insensitively() {
        let entries = vec![
            Entry {
                path: PathBuf::from("/tmp/Docs"),
                name: "Docs".to_owned(),
                is_dir: true,
            },
            Entry {
                path: PathBuf::from("/tmp/beta.txt"),
                name: "beta.txt".to_owned(),
                is_dir: false,
            },
        ];
        assert_eq!(typeahead_index(&entries, "be"), Some(1));
        assert_eq!(typeahead_index(&entries, "DO"), Some(0));
        assert_eq!(typeahead_index(&entries, "x"), None);
        assert_eq!(typeahead_index(&entries, ""), None);
    }

    #[test]
    fn split_takes_the_tail_after_the_last_slash() {
        assert_eq!(split_dir_tail("~/Doc/x"), ("~/Doc/", "x"));
        assert_eq!(split_dir_tail("x"), ("", "x"));
        assert_eq!(split_dir_tail("/tmp/"), ("/tmp/", ""));
        assert_eq!(split_dir_tail("/"), ("/", ""));
    }

    #[test]
    fn common_prefix_shrinks_to_the_shared_leading_run() {
        assert_eq!(common_prefix(["download", "downpour", "down"]), "down");
        assert_eq!(common_prefix(["a", "b"]), "");
        assert_eq!(common_prefix(["same", "same"]), "same");
        assert_eq!(common_prefix(Vec::<&str>::new()), "");
    }

    #[test]
    fn listing_sorts_dirs_first_and_hides_dotfiles() {
        let root = std::env::temp_dir().join(format!("aegis-chooser-{}", std::process::id()));
        std::fs::create_dir_all(root.join("zeta")).unwrap();
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::write(root.join("beta.txt"), b"x").unwrap();
        std::fs::write(root.join(".hidden"), b"x").unwrap();

        let entries = list_dir(&root, false, None).unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta", "beta.txt"]);

        let entries = list_dir(&root, true, None).unwrap();
        assert_eq!(entries.len(), 4);

        let only_txt = filter("Text", &[("*.txt", FilterRuleKind::Glob)]);
        let entries = list_dir(&root, false, Some(&only_txt)).unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        // Directories stay visible for navigation; files are filtered.
        assert_eq!(names, ["alpha", "zeta", "beta.txt"]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bookmarks_parse_and_roundtrip() {
        let root = std::env::temp_dir().join(format!("aegis-bookmarks-{}", std::process::id()));
        let dir1 = root.join("Projects");
        let dir2 = root.join("Notes");
        std::fs::create_dir_all(&dir1).unwrap();
        std::fs::create_dir_all(&dir2).unwrap();

        let raw = format!(
            "file://{} My Projects\nfile://{}\n",
            dir1.to_str().unwrap(),
            dir2.to_str().unwrap()
        );
        let parsed = parse_bookmarks(&raw);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "My Projects");
        assert_eq!(parsed[0].path, dir1);
        assert_eq!(parsed[0].section, PlaceSection::Pinned);
        assert!(parsed[0].custom);
        assert_eq!(parsed[1].name, "Notes");
        assert_eq!(parsed[1].path, dir2);

        let home = root.clone();
        save_bookmarks(Some(&home), &parsed).unwrap();
        let loaded = load_bookmarks(Some(&home));
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "My Projects");
        assert_eq!(loaded[1].name, "Notes");

        std::fs::remove_dir_all(root).unwrap();
    }
}
