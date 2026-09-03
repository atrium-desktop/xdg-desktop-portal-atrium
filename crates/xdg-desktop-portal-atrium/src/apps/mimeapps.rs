//! `mimeapps.list` association handling: the Added/Removed/Default groups
//! and the config-file edit that records a new default.

/// Minimal INI edit: set `content_type`'s `[Default Applications]` value to
/// `desktop_id` (keeping any other listed ids as fallbacks behind it),
/// inserting the key or the whole group when absent. Every unrelated line
/// is preserved verbatim.
pub(super) fn update_default_applications(
    text: &str,
    content_type: &str,
    desktop_id: &str,
) -> String {
    let key_line = |id: &str| format!("{content_type}={id};\n");
    let mut out: Vec<String> = Vec::new();
    let mut in_defaults = false;
    let mut seen_group = false;
    let mut wrote_key = false;

    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let header = body
            .trim_end()
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .map(str::trim);
        if let Some(header) = header {
            // Leaving the group without having seen the key: append it.
            if in_defaults && !wrote_key {
                out.push(key_line(desktop_id));
                wrote_key = true;
            }
            in_defaults = header == "Default Applications";
            seen_group |= in_defaults;
            out.push(line.to_owned());
            continue;
        }
        let is_target = in_defaults
            && body
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == content_type);
        if is_target {
            let existing = body.split_once('=').map_or("", |(_, value)| value);
            let mut ids: Vec<&str> = vec![desktop_id];
            ids.extend(
                existing
                    .split(';')
                    .map(str::trim)
                    .filter(|id| !id.is_empty() && *id != desktop_id),
            );
            out.push(key_line(&ids.join(";")));
            wrote_key = true;
            continue;
        }
        out.push(line.to_owned());
    }
    // The file ended inside the group without the key.
    if in_defaults && !wrote_key {
        out.push(key_line(desktop_id));
    }

    if !seen_group {
        if let Some(last) = out.last_mut()
            && !last.ends_with('\n')
        {
            last.push('\n');
        }
        if !out.is_empty() {
            out.push("\n".to_owned());
        }
        out.push("[Default Applications]\n".to_owned());
        out.push(key_line(desktop_id));
    }

    let mut result = out.concat();
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    result
}
