//! YAML frontmatter parsing, serialization, and field-level manipulation.

use crate::error::{VaultError, VaultResult};

/// Extract raw frontmatter string from markdown content.
/// Returns `None` if no frontmatter block exists.
/// Returns `(yaml_str, body_start_byte_offset)`.
pub fn extract_raw_frontmatter(content: &str) -> Option<(&str, usize)> {
    let after_open = if content.starts_with("---\n") {
        4
    } else if content.starts_with("---\r\n") {
        5
    } else {
        return None;
    };

    let rest = &content[after_open..];
    let mut pos = 0;

    loop {
        let remaining = &rest[pos..];
        let line_end = remaining.find('\n');

        let line = match line_end {
            Some(end) => &remaining[..end],
            None => remaining,
        };

        if line.trim_end_matches('\r') == "---" {
            let yaml_str = &content[after_open..after_open + pos];
            let body_start = match line_end {
                Some(end) => after_open + pos + end + 1,
                None => content.len(),
            };
            return Some((yaml_str, body_start));
        }

        pos += line_end? + 1;
    }
}

/// Get the body content (everything after frontmatter).
pub fn get_body(content: &str) -> &str {
    match extract_raw_frontmatter(content) {
        Some((_, body_start)) => &content[body_start..],
        None => content,
    }
}

/// Parse frontmatter YAML into a `serde_json::Value` (JSON object).
/// Returns `None` if no frontmatter exists, `Err` on malformed YAML.
///
/// Callers with file-path context should map `VaultError::Other` into
/// `VaultError::FrontmatterParse` via `.map_err()`.
pub fn parse_frontmatter(content: &str) -> VaultResult<Option<serde_json::Value>> {
    let (yaml_str, _) = match extract_raw_frontmatter(content) {
        Some(v) => v,
        None => return Ok(None),
    };

    if yaml_str.trim().is_empty() {
        return Ok(Some(serde_json::Value::Object(serde_json::Map::new())));
    }

    let value: serde_json::Value = serde_yaml::from_str(yaml_str)
        .map_err(|e| VaultError::Other(format!("Frontmatter YAML parse error: {e}")))?;

    // Degrade instead of dropping the note. A block like "---\n\n## title: x"
    // is a YAML comment, so it parses to null; upstream turned that into a hard
    // error and the ENTIRE note vanished from both the BM25 and semantic
    // indexes. 10 of 416 notes in a real vault hit this. Treat any non-mapping
    // frontmatter as empty and keep the note searchable.
    if !matches!(&value, serde_json::Value::Object(_)) {
        tracing::warn!("frontmatter is not a YAML mapping; treating as empty");
        return Ok(Some(serde_json::Value::Object(serde_json::Map::new())));
    }

    match &value {
        serde_json::Value::Object(_) => Ok(Some(value)),
        other => Err(VaultError::Other(format!(
            "Frontmatter must be a YAML mapping, got {}",
            json_type_name(other),
        ))),
    }
}

pub(crate) fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Array(_) => "array",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Null => "null",
        serde_json::Value::Object(_) => "object",
    }
}

/// Shared helper: extract a string list from a frontmatter field.
/// Handles YAML arrays, single strings (comma-separated), and other scalars.
fn extract_string_list(frontmatter: &serde_json::Value, key: &str) -> Vec<String> {
    let Some(val) = frontmatter.get(key) else {
        return Vec::new();
    };

    match val {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Null => None,
                other => Some(other.to_string()),
            })
            .collect(),
        serde_json::Value::String(s) => s
            .split(',')
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Extract tags from frontmatter. Handles both:
///   `tags: [a, b]`  and  `tags:\n  - a\n  - b`
pub fn extract_frontmatter_tags(frontmatter: &serde_json::Value) -> Vec<String> {
    extract_string_list(frontmatter, "tags")
}

/// Extract aliases from frontmatter.
pub fn extract_aliases(frontmatter: &serde_json::Value) -> Vec<String> {
    extract_string_list(frontmatter, "aliases")
}

/// Set a field in frontmatter. If frontmatter doesn't exist, creates it.
/// Returns the modified full file content.
pub fn set_frontmatter_field(
    content: &str,
    key: &str,
    value: serde_json::Value,
) -> VaultResult<String> {
    let body = get_body(content);

    let mut fm = parse_frontmatter(content)?
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    fm.as_object_mut()
        .ok_or_else(|| VaultError::Other("Frontmatter is not a YAML mapping".into()))?
        .insert(key.to_string(), value);

    Ok(rebuild_content(Some(&fm), body))
}

/// Remove a field from frontmatter. Returns the modified full file content.
/// Returns content unchanged if field doesn't exist or no frontmatter.
pub fn remove_frontmatter_field(content: &str, key: &str) -> VaultResult<String> {
    let mut fm = match parse_frontmatter(content)? {
        Some(fm) => fm,
        None => return Ok(content.to_string()),
    };

    let map = fm
        .as_object_mut()
        .ok_or_else(|| VaultError::Other("Frontmatter is not a YAML mapping".into()))?;

    if !map.contains_key(key) {
        return Ok(content.to_string());
    }

    map.remove(key);

    let body = get_body(content);

    if map.is_empty() {
        Ok(body.to_string())
    } else {
        Ok(rebuild_content(Some(&fm), body))
    }
}

/// Rebuild full file content from frontmatter value + body.
pub fn rebuild_content(frontmatter: Option<&serde_json::Value>, body: &str) -> String {
    match frontmatter {
        Some(fm) if fm.as_object().is_some_and(|m| !m.is_empty()) => {
            match serde_yaml::to_string(fm) {
                Ok(yaml) => format!("---\n{yaml}---\n{body}"),
                Err(e) => {
                    tracing::warn!("Failed to serialize frontmatter, dropping it: {e}");
                    body.to_string()
                }
            }
        }
        _ => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── extract_raw_frontmatter ──────────────────────────────────────

    #[test]
    fn extract_no_frontmatter() {
        assert!(extract_raw_frontmatter("# Just a heading\n").is_none());
    }

    #[test]
    fn extract_empty_frontmatter() {
        let content = "---\n---\n";
        let (yaml, body_start) = extract_raw_frontmatter(content).unwrap();
        assert_eq!(yaml, "");
        assert_eq!(&content[body_start..], "");
    }

    #[test]
    fn extract_empty_frontmatter_with_body() {
        let content = "---\n---\nbody";
        let (yaml, body_start) = extract_raw_frontmatter(content).unwrap();
        assert_eq!(yaml, "");
        assert_eq!(&content[body_start..], "body");
    }

    #[test]
    fn extract_standard_frontmatter() {
        let content = "---\ntags: [a, b]\ntitle: Hello\n---\n# Body\n";
        let (yaml, body_start) = extract_raw_frontmatter(content).unwrap();
        assert_eq!(yaml, "tags: [a, b]\ntitle: Hello\n");
        assert_eq!(&content[body_start..], "# Body\n");
    }

    #[test]
    fn extract_frontmatter_only_no_trailing_newline() {
        let content = "---\ntags: [a]\n---";
        let (yaml, body_start) = extract_raw_frontmatter(content).unwrap();
        assert_eq!(yaml, "tags: [a]\n");
        assert_eq!(body_start, content.len());
        assert_eq!(&content[body_start..], "");
    }

    #[test]
    fn extract_crlf_frontmatter() {
        let content = "---\r\ntags: [a]\r\n---\r\nbody";
        let (yaml, body_start) = extract_raw_frontmatter(content).unwrap();
        assert_eq!(yaml, "tags: [a]\r\n");
        assert_eq!(&content[body_start..], "body");
    }

    #[test]
    fn extract_no_closing_delimiter() {
        assert!(extract_raw_frontmatter("---\ntags: [a]\n").is_none());
    }

    #[test]
    fn extract_four_dashes_not_frontmatter() {
        assert!(extract_raw_frontmatter("----\ntags: [a]\n---\n").is_none());
    }

    #[test]
    fn extract_delimiter_with_trailing_space_not_matched() {
        assert!(extract_raw_frontmatter("--- \ntags: [a]\n---\n").is_none());
    }

    #[test]
    fn extract_hr_in_body_not_confused() {
        let content = "---\ntitle: Test\n---\nSome text\n\n---\n\nMore text\n";
        let (yaml, body_start) = extract_raw_frontmatter(content).unwrap();
        assert_eq!(yaml, "title: Test\n");
        assert_eq!(&content[body_start..], "Some text\n\n---\n\nMore text\n");
    }

    // ── get_body ─────────────────────────────────────────────────────

    #[test]
    fn body_with_frontmatter() {
        assert_eq!(get_body("---\na: 1\n---\nbody"), "body");
    }

    #[test]
    fn body_without_frontmatter() {
        assert_eq!(get_body("just text"), "just text");
    }

    #[test]
    fn body_empty_after_frontmatter() {
        assert_eq!(get_body("---\na: 1\n---\n"), "");
    }

    // ── parse_frontmatter ────────────────────────────────────────────

    #[test]
    fn parse_no_frontmatter() {
        assert_eq!(parse_frontmatter("# Title").unwrap(), None);
    }

    #[test]
    fn parse_empty_frontmatter() {
        let result = parse_frontmatter("---\n---\n").unwrap().unwrap();
        assert_eq!(result, json!({}));
    }

    #[test]
    fn parse_standard_frontmatter() {
        let content = "---\ntags:\n  - rust\n  - mcp\ntitle: Hello\n---\nbody";
        let fm = parse_frontmatter(content).unwrap().unwrap();
        assert_eq!(fm["tags"], json!(["rust", "mcp"]));
        assert_eq!(fm["title"], json!("Hello"));
    }

    #[test]
    fn parse_various_value_types() {
        let content = "---\ncount: 42\nactive: true\ntags: [a, b]\n---\n";
        let fm = parse_frontmatter(content).unwrap().unwrap();
        assert_eq!(fm["count"], json!(42));
        assert_eq!(fm["active"], json!(true));
        assert_eq!(fm["tags"], json!(["a", "b"]));
    }

    #[test]
    fn parse_malformed_yaml() {
        let content = "---\n[invalid yaml\n---\nbody";
        assert!(parse_frontmatter(content).is_err());
    }

    #[test]
    fn parse_non_mapping_yaml_array() {
        let content = "---\n- a\n- b\n---\nbody";
        // Non-mapping frontmatter degrades to an empty mapping so the note
        // stays indexed, instead of erroring and dropping the note entirely.
        let value = parse_frontmatter(content).expect("array frontmatter should degrade");
        assert_eq!(value, Some(serde_json::Value::Object(serde_json::Map::new())));
    }

    #[test]
    fn parse_non_mapping_yaml_scalar() {
        let content = "---\njust a string\n---\nbody";
        let value = parse_frontmatter(content).expect("scalar frontmatter should degrade");
        assert_eq!(value, Some(serde_json::Value::Object(serde_json::Map::new())));
    }

    // ── extract_frontmatter_tags ─────────────────────────────────────

    #[test]
    fn tags_from_array() {
        let fm = json!({"tags": ["rust", "mcp"]});
        assert_eq!(extract_frontmatter_tags(&fm), vec!["rust", "mcp"]);
    }

    #[test]
    fn tags_from_comma_string() {
        let fm = json!({"tags": "rust, mcp, tools"});
        assert_eq!(extract_frontmatter_tags(&fm), vec!["rust", "mcp", "tools"]);
    }

    #[test]
    fn tags_missing_key() {
        let fm = json!({"title": "Hello"});
        assert!(extract_frontmatter_tags(&fm).is_empty());
    }

    #[test]
    fn tags_with_non_string_elements() {
        let fm = json!({"tags": [42, "rust", true, null]});
        let tags = extract_frontmatter_tags(&fm);
        assert_eq!(tags, vec!["42", "rust", "true"]);
    }

    // ── extract_aliases ──────────────────────────────────────────────

    #[test]
    fn aliases_from_array() {
        let fm = json!({"aliases": ["server", "mcp-server"]});
        assert_eq!(extract_aliases(&fm), vec!["server", "mcp-server"]);
    }

    #[test]
    fn aliases_from_comma_string() {
        let fm = json!({"aliases": "server, mcp-server"});
        assert_eq!(extract_aliases(&fm), vec!["server", "mcp-server"]);
    }

    #[test]
    fn aliases_missing() {
        assert!(extract_aliases(&json!({})).is_empty());
    }

    // ── set_frontmatter_field ────────────────────────────────────────

    #[test]
    fn set_field_in_existing_frontmatter() {
        let content = "---\ntags:\n- a\n---\nbody";
        let result = set_frontmatter_field(content, "title", json!("Hello")).unwrap();
        let fm = parse_frontmatter(&result).unwrap().unwrap();
        assert_eq!(fm["title"], json!("Hello"));
        assert_eq!(fm["tags"], json!(["a"]));
        assert_eq!(get_body(&result), "body");
    }

    #[test]
    fn set_field_creates_frontmatter() {
        let content = "just body";
        let result = set_frontmatter_field(content, "title", json!("Hello")).unwrap();
        let fm = parse_frontmatter(&result).unwrap().unwrap();
        assert_eq!(fm["title"], json!("Hello"));
        assert_eq!(get_body(&result), "just body");
    }

    #[test]
    fn set_field_overwrites_existing() {
        let content = "---\ntitle: Old\n---\nbody";
        let result = set_frontmatter_field(content, "title", json!("New")).unwrap();
        let fm = parse_frontmatter(&result).unwrap().unwrap();
        assert_eq!(fm["title"], json!("New"));
    }

    // ── remove_frontmatter_field ─────────────────────────────────────

    #[test]
    fn remove_existing_field() {
        let content = "---\ntags:\n- a\ntitle: Hello\n---\nbody";
        let result = remove_frontmatter_field(content, "tags").unwrap();
        let fm = parse_frontmatter(&result).unwrap().unwrap();
        assert!(fm.get("tags").is_none());
        assert_eq!(fm["title"], json!("Hello"));
        assert_eq!(get_body(&result), "body");
    }

    #[test]
    fn remove_nonexistent_field() {
        let content = "---\ntags:\n- a\n---\nbody";
        let result = remove_frontmatter_field(content, "missing").unwrap();
        assert_eq!(result, content);
    }

    #[test]
    fn remove_last_field_strips_frontmatter() {
        let content = "---\ntags:\n- a\n---\nbody";
        let result = remove_frontmatter_field(content, "tags").unwrap();
        assert_eq!(result, "body");
        assert!(parse_frontmatter(&result).unwrap().is_none());
    }

    #[test]
    fn remove_from_no_frontmatter() {
        let content = "just body";
        let result = remove_frontmatter_field(content, "tags").unwrap();
        assert_eq!(result, "just body");
    }

    // ── rebuild_content ──────────────────────────────────────────────

    #[test]
    fn rebuild_with_frontmatter() {
        let fm = json!({"title": "Hello"});
        let result = rebuild_content(Some(&fm), "body\n");
        assert!(result.starts_with("---\n"));
        assert!(result.ends_with("---\nbody\n"));
        assert!(result.contains("title:"));
    }

    #[test]
    fn rebuild_no_frontmatter() {
        assert_eq!(rebuild_content(None, "body"), "body");
    }

    #[test]
    fn rebuild_empty_frontmatter_returns_body() {
        assert_eq!(rebuild_content(Some(&json!({})), "body"), "body");
    }

    // ── roundtrip ────────────────────────────────────────────────────

    #[test]
    fn roundtrip_preserves_body_exactly() {
        let body = "\n# Heading\n\nSome text with **bold** and `code`.\n\n- list item\n";
        let content = format!("---\ntags:\n- rust\n---\n{body}");

        let fm = parse_frontmatter(&content).unwrap().unwrap();
        let rebuilt = rebuild_content(Some(&fm), get_body(&content));

        assert_eq!(get_body(&rebuilt), body);
    }

    #[test]
    fn roundtrip_set_then_remove_restores_body() {
        let original = "just the body\nwith multiple lines\n";
        let with_field = set_frontmatter_field(original, "title", json!("Test")).unwrap();
        let restored = remove_frontmatter_field(&with_field, "title").unwrap();
        assert_eq!(restored, original);
    }
}
