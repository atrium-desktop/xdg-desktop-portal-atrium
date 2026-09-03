//! Desktop-entry discovery: id validation, INI list parsing, the
//! `mimeinfo.cache` globs2 index, glob matching, and `.desktop` parsing.

use std::collections::HashMap;
use std::path::Path;

use super::{AppInfo, MAX_ENTRY_BYTES, MAX_EXEC_BYTES, MAX_GLOBS, MAX_GLOBS_BYTES, MAX_ID_BYTES};

/// Desktop ids handled here are plain UTF-8-ish file names: no separators,
/// no NUL, bounded length.
pub(super) fn valid_desktop_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0')
        && !id.contains('\n')
}

/// A content type is `type/subtype`-shaped text, bounded and NUL-free.
pub(super) fn valid_content_type(content_type: &str) -> bool {
    let mut parts = content_type.split('/');
    let (Some(kind), Some(subtype), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    content_type.len() <= MAX_ID_BYTES
        && !content_type.contains('\0')
        && !content_type.contains('\n')
        && !kind.trim().is_empty()
        && !subtype.trim().is_empty()
}

/// Read and parse one INI-ish file into `group -> key -> id list` form.
/// Missing or oversized files yield an empty map. Only `;`-separated list
/// values are kept, which covers both `mimeinfo.cache` and `mimeapps.list`.
pub(super) fn parse_grouped_lists(path: &Path) -> HashMap<String, HashMap<String, Vec<String>>> {
    let mut groups: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    let Ok(file) = std::fs::File::open(path) else {
        return groups;
    };
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_ENTRY_BYTES + 1),
        &mut bytes,
    )
    .is_err()
        || bytes.len() as u64 > MAX_ENTRY_BYTES
    {
        return groups;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut group = String::new();
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
        {
            group = name.trim().to_owned();
            continue;
        }
        if group.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.contains('[') {
            continue;
        }
        let ids: Vec<String> = value
            .split(';')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        groups
            .entry(group.clone())
            .or_default()
            .insert(key.to_owned(), ids);
    }
    groups
}

/// One `globs2` line: `priority:glob:mimetype[:flags]`. Only the `cs`
/// flag is honoured (case-sensitive matching); character classes inside
/// globs are not expanded — real-world `globs2` files use `*` and `?`.
/// Lines that do not fit the shape are skipped.
pub(super) fn parse_globs2(path: &Path) -> Vec<(u32, String, String, bool)> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_GLOBS_BYTES + 1),
        &mut bytes,
    )
    .is_err()
        || bytes.len() as u64 > MAX_GLOBS_BYTES
    {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut globs = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((priority, rest)) = line.split_once(':') else {
            continue;
        };
        let Ok(priority) = priority.trim().parse::<u32>() else {
            continue;
        };
        let Some((glob, rest)) = rest.split_once(':') else {
            continue;
        };
        let (mime, flags) = rest.split_once(':').unwrap_or((rest, ""));
        if glob.is_empty() || mime.is_empty() || globs.len() >= MAX_GLOBS {
            continue;
        }
        let case_sensitive = flags.split(',').any(|flag| flag.trim() == "cs");
        globs.push((priority, glob.to_owned(), mime.to_owned(), case_sensitive));
    }
    globs
}

/// Match a `globs2` pattern against a file name. Supports `*` (any run)
/// and `?` (one character); everything else is literal. Case-insensitive
/// unless `case_sensitive` (`cs` flag).
pub(super) fn glob_matches(glob: &str, name: &str, case_sensitive: bool) -> bool {
    let fold = |text: &str| {
        if case_sensitive {
            text.chars().collect::<Vec<char>>()
        } else {
            text.to_lowercase().chars().collect()
        }
    };
    let pattern = fold(glob);
    let text = fold(name);
    let (mut p, mut t) = (0, 0);
    let (mut star_p, mut star_t) = (usize::MAX, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star_p = p;
            star_t = t;
            p += 1;
        } else if star_p != usize::MAX {
            p = star_p + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Parse one desktop file. Only the `[Desktop Entry]` group is read;
/// `Hidden=true` deletes the entry from view (returns `None`).
pub(super) fn parse_desktop_file(path: &Path, id: &str) -> Option<AppInfo> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_ENTRY_BYTES + 1),
        &mut bytes,
    )
    .is_err()
        || bytes.len() as u64 > MAX_ENTRY_BYTES
    {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);

    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut no_display = false;
    let mut terminal = false;
    let mut mime_types = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(group) = line
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
        {
            in_entry = group.trim() == "Desktop Entry";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Name" => name = Some(value.to_owned()),
            "Exec" if value.len() <= MAX_EXEC_BYTES => exec = Some(value.to_owned()),
            "Icon" if !value.is_empty() => icon = Some(value.to_owned()),
            "NoDisplay" => no_display = value == "true",
            "Hidden" if value == "true" => return None,
            "Terminal" => terminal = value == "true",
            "MimeType" => {
                mime_types = value
                    .split(';')
                    .map(str::trim)
                    .filter(|mime| !mime.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            _ => {}
        }
    }

    Some(AppInfo {
        id: id.to_owned(),
        name: name?.chars().take(256).collect(),
        exec: exec?,
        icon,
        terminal,
        no_display,
        mime_types,
    })
}
