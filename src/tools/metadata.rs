//! Note metadata inspection and frontmatter manipulation tools.

use std::path::{Path, PathBuf};

use rmcp::model::{CallToolResult, Content, ErrorCode};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::VaultError;
use crate::models::{FileStat, Heading, WikiLink};
use crate::vault::Vault;

// ── note_metadata / note_patch_targets ─────────────────────────────────

/// Parameters for the `note_metadata` tool.
#[derive(Deserialize, JsonSchema, Default)]
pub struct NoteMetadataParams {
    /// Path to the note, relative to vault root.
    pub path: String,
}

/// Parameters for the `note_patch_targets` tool.
#[derive(Deserialize, JsonSchema, Default)]
pub struct NotePatchTargetsParams {
    /// Path to the note, relative to vault root.
    pub path: String,
}

#[derive(Serialize, JsonSchema)]
struct NoteMetadataOutput {
    path: PathBuf,
    title: String,
    tags: Vec<String>,
    frontmatter: Option<serde_json::Value>,
    headings: Vec<Heading>,
    outgoing_links: Vec<WikiLink>,
    block_refs: Vec<String>,
    backlinks_count: usize,
    stat: FileStat,
}

/// A note's metadata: tags, headings, links, frontmatter and file stats.
pub async fn note_metadata(
    vault: &Vault,
    params: NoteMetadataParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    note_inspect_metadata(vault, &params.path).await
}

/// The addressable targets in a note, for use before `note_patch`.
pub async fn note_patch_targets(
    vault: &Vault,
    params: NotePatchTargetsParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    note_inspect_targets(vault, &params.path).await
}

async fn note_inspect_metadata(
    vault: &Vault,
    note_path: &str,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let path = Path::new(note_path);
    let meta = vault.get_note_metadata(path)?;
    let backlinks = vault.backlinks(path)?;

    let output = NoteMetadataOutput {
        path: meta.path,
        title: meta.title,
        tags: meta.tags,
        frontmatter: meta.frontmatter,
        headings: meta.headings,
        outgoing_links: meta.links,
        block_refs: meta.block_refs,
        backlinks_count: backlinks.len(),
        stat: meta.stat,
    };

    let value = serde_json::to_value(output)
        .map_err(|e| VaultError::Other(format!("serialization error: {e}")))?;
    Ok(CallToolResult::structured(value))
}

async fn note_inspect_targets(
    vault: &Vault,
    note_path: &str,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let path = Path::new(note_path);
    let map = vault.get_document_map(path)?;

    let value = serde_json::to_value(map)
        .map_err(|e| VaultError::Other(format!("serialization error: {e}")))?;
    Ok(CallToolResult::structured(value))
}

// ── frontmatter ────────────────────────────────────────────────────────

/// Parameters for the `note_frontmatter` tool.
#[derive(Deserialize, JsonSchema, Default)]
pub struct NoteFrontmatterParams {
    /// Path to the note, relative to vault root.
    pub path: String,
}

/// Parameters for the `note_frontmatter_edit` tool.
///
/// Reading frontmatter lives in a separate tool on purpose. `OBSIDIAN_TOOLS`
/// filters by tool *name*, so a single tool multiplexing read and write actions
/// cannot be filtered: including it for its read action grants its write action
/// too. Splitting them is what makes the `read` profile actually read-only.
#[derive(Deserialize, JsonSchema, Default)]
pub struct NoteFrontmatterEditParams {
    /// Path to the note, relative to vault root.
    pub path: String,
    /// Edit to apply: `"set"` (upsert a field) or `"remove"` (delete a field).
    pub action: String,
    /// Frontmatter key to set or remove.
    pub key: String,
    /// JSON value to assign. Required for `"set"`, ignored for `"remove"`. Pass
    /// arrays and objects directly; a JSON-encoded string is stored as a
    /// literal string.
    #[serde(
        default,
        deserialize_with = "crate::tools::deserialize_optional_json_value"
    )]
    #[schemars(schema_with = "crate::tools::json_value_schema")]
    pub value: Option<serde_json::Value>,
}

/// Read a note's frontmatter.
pub async fn note_frontmatter(
    vault: &Vault,
    params: NoteFrontmatterParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let path = Path::new(&params.path);

    match vault.get_frontmatter(path)? {
        Some(value) => Ok(CallToolResult::structured(value)),
        None => Ok(CallToolResult::success(vec![Content::text("null")])),
    }
}

/// Set or remove a single frontmatter field.
pub async fn note_frontmatter_edit(
    vault: &Vault,
    params: NoteFrontmatterEditParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let path = Path::new(&params.path);
    let key = params.key.as_str();

    if params.action.eq_ignore_ascii_case("set") {
        let value = params.value.ok_or_else(|| {
            rmcp::ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "'value' is required for action 'set'",
                None::<serde_json::Value>,
            )
        })?;
        vault.set_frontmatter_field(path, key, value)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Set frontmatter field '{key}' on '{}'",
            params.path
        ))]))
    } else if params.action.eq_ignore_ascii_case("remove") {
        vault.remove_frontmatter_field(path, key)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Removed frontmatter field '{key}' from '{}'",
            params.path
        ))]))
    } else {
        Err(rmcp::ErrorData::new(
            ErrorCode::INVALID_PARAMS,
            format!(
                "Unknown action '{}'. Valid values: \"set\", \"remove\"",
                params.action
            ),
            None::<serde_json::Value>,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{PatchOperation, PatchTargetType};
    use crate::test_helpers::{create_test_vault, test_config};
    use crate::tools::notes::{NotePatchParams, note_patch};

    #[tokio::test]
    async fn note_inspect_metadata_returns_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        vault
            .write_note(
                Path::new("test.md"),
                "---\ntags: [rust]\nstatus: draft\n---\n# Heading\n## Sub\n[[other]] #inline\n^block1\n",
            )
            .unwrap();
        vault
            .write_note(Path::new("other.md"), "# Other\n[[test]]\n")
            .unwrap();

        let result = note_metadata(
            &vault,
            NoteMetadataParams {
                path: "test.md".into(),
            },
        )
        .await
        .unwrap();

        let v = result.structured_content.unwrap();
        assert_eq!(v["title"], "test");
        assert!(
            v["tags"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("rust"))
        );
        assert!(v["frontmatter"].is_object());
        assert!(!v["headings"].as_array().unwrap().is_empty());
        assert!(!v["outgoing_links"].as_array().unwrap().is_empty());
        assert!(
            v["block_refs"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("block1"))
        );
        assert_eq!(v["backlinks_count"], 1);
        assert!(v["stat"].is_object());
    }

    #[tokio::test]
    async fn note_inspect_not_found() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        let result = note_metadata(
            &vault,
            NoteMetadataParams {
                path: "nonexistent.md".into(),
            },
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn note_inspect_targets_lists_targets() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        vault
            .write_note(
                Path::new("mapped.md"),
                "---\ntags: [rust]\ndate: 2026-01-01\n---\n# Heading\n## Sub\nText ^block1\n",
            )
            .unwrap();

        let result = note_patch_targets(
            &vault,
            NotePatchTargetsParams {
                path: "mapped.md".into(),
            },
        )
        .await
        .unwrap();

        let v = result.structured_content.unwrap();
        let headings = v["headings"].as_array().unwrap();
        assert!(
            headings
                .iter()
                .any(|h| h.as_str().unwrap().contains("Heading"))
        );
        assert!(headings.iter().any(|h| h.as_str().unwrap().contains("Sub")));
        assert!(
            v["block_refs"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("block1"))
        );
        assert!(
            v["frontmatter_fields"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("tags"))
        );
        assert!(
            v["frontmatter_fields"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("date"))
        );
    }

    #[tokio::test]
    async fn note_inspect_targets_heading_can_be_used_for_note_patch() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        vault
            .write_note(
                Path::new("scratch.md"),
                "# Scratch\n\n## Log\n\n| Date | Update |\n| ---- | ------ |\n",
            )
            .unwrap();

        let result = note_patch_targets(
            &vault,
            NotePatchTargetsParams {
                path: "scratch.md".into(),
            },
        )
        .await
        .unwrap();

        let v = result.structured_content.unwrap();
        let headings = v["headings"].as_array().unwrap();
        let target = headings
            .iter()
            .find_map(|h| {
                let heading = h.as_str().unwrap();
                (heading == "## Log").then(|| heading.to_string())
            })
            .expect("targets view should return marker-prefixed heading");

        note_patch(
            &vault,
            NotePatchParams {
                path: "scratch.md".into(),
                operation: PatchOperation::Append,
                target_type: PatchTargetType::Heading,
                target,
                content: "| 2026-02-02 | x |".into(),
            },
        )
        .await
        .unwrap();

        let content = vault.read_note(Path::new("scratch.md")).unwrap();
        let log_idx = content.find("## Log").unwrap();
        let appended_idx = content.find("| 2026-02-02 | x |").unwrap();
        assert!(appended_idx > log_idx);
    }

    #[tokio::test]
    async fn frontmatter_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        vault
            .write_note(Path::new("fm.md"), "# Note\nBody\n")
            .unwrap();

        let result = note_frontmatter(
            &vault,
            NoteFrontmatterParams {
                path: "fm.md".into(),
            },
        )
        .await
        .unwrap();
        assert!(result.structured_content.is_none());
        let text = result.content[0].as_text().expect("expected text content");
        assert_eq!(text.text, "null");

        note_frontmatter_edit(
            &vault,
            NoteFrontmatterEditParams {
                action: "set".into(),
                path: "fm.md".into(),
                key: "status".into(),
                value: Some(serde_json::json!("draft")),
            },
        )
        .await
        .unwrap();

        let result = note_frontmatter(
            &vault,
            NoteFrontmatterParams {
                path: "fm.md".into(),
            },
        )
        .await
        .unwrap();
        let fm = result.structured_content.unwrap();
        assert_eq!(fm["status"], "draft");

        note_frontmatter_edit(
            &vault,
            NoteFrontmatterEditParams {
                action: "set".into(),
                path: "fm.md".into(),
                key: "tags".into(),
                value: Some(serde_json::json!(["rust", "mcp"])),
            },
        )
        .await
        .unwrap();

        let result = note_frontmatter(
            &vault,
            NoteFrontmatterParams {
                path: "fm.md".into(),
            },
        )
        .await
        .unwrap();
        let fm = result.structured_content.unwrap();
        assert_eq!(fm["status"], "draft");
        assert_eq!(fm["tags"], serde_json::json!(["rust", "mcp"]));

        for (key, value) in [
            ("empty", serde_json::Value::Null),
            ("literal_json", serde_json::json!("[\"rust\",\"mcp\"]")),
        ] {
            note_frontmatter_edit(
                &vault,
                NoteFrontmatterEditParams {
                    action: "set".into(),
                    path: "fm.md".into(),
                    key: key.into(),
                    value: Some(value),
                },
            )
            .await
            .unwrap();
        }

        let result = note_frontmatter(
            &vault,
            NoteFrontmatterParams {
                path: "fm.md".into(),
            },
        )
        .await
        .unwrap();
        let fm = result.structured_content.unwrap();
        assert_eq!(fm["empty"], serde_json::Value::Null);
        assert_eq!(fm["literal_json"], "[\"rust\",\"mcp\"]");

        note_frontmatter_edit(
            &vault,
            NoteFrontmatterEditParams {
                action: "remove".into(),
                path: "fm.md".into(),
                key: "status".into(),
                value: None,
            },
        )
        .await
        .unwrap();

        let result = note_frontmatter(
            &vault,
            NoteFrontmatterParams {
                path: "fm.md".into(),
            },
        )
        .await
        .unwrap();
        let fm = result.structured_content.unwrap();
        assert!(fm.get("status").is_none());
        assert_eq!(fm["tags"], serde_json::json!(["rust", "mcp"]));
    }

    #[test]
    fn frontmatter_edit_params_preserve_missing_null_and_literal_strings() {
        let missing: NoteFrontmatterEditParams = serde_json::from_value(serde_json::json!({
            "action": "set",
            "path": "fm.md",
            "key": "value"
        }))
        .unwrap();
        assert!(missing.value.is_none());

        let explicit_null: NoteFrontmatterEditParams = serde_json::from_value(serde_json::json!({
            "action": "set",
            "path": "fm.md",
            "key": "value",
            "value": null
        }))
        .unwrap();
        assert_eq!(explicit_null.value, Some(serde_json::Value::Null));

        let array: NoteFrontmatterEditParams = serde_json::from_value(serde_json::json!({
            "action": "set",
            "path": "fm.md",
            "key": "value",
            "value": ["rust", "mcp"]
        }))
        .unwrap();
        assert_eq!(array.value, Some(serde_json::json!(["rust", "mcp"])));

        let literal: NoteFrontmatterEditParams = serde_json::from_value(serde_json::json!({
            "action": "set",
            "path": "fm.md",
            "key": "value",
            "value": "[\"rust\",\"mcp\"]"
        }))
        .unwrap();
        assert_eq!(literal.value, Some(serde_json::json!("[\"rust\",\"mcp\"]")));
    }

    #[tokio::test]
    async fn frontmatter_edit_invalid_action() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();
        vault.write_note(Path::new("fm.md"), "# Note\n").unwrap();

        // "get" is deliberately not accepted here: reading lives in
        // `note_frontmatter`, which is what makes name-based filtering work.
        for action in ["invalid", "get"] {
            let result = note_frontmatter_edit(
                &vault,
                NoteFrontmatterEditParams {
                    action: action.into(),
                    path: "fm.md".into(),
                    key: "k".into(),
                    value: Some(serde_json::json!("v")),
                },
            )
            .await;
            assert!(result.is_err(), "action '{action}' should be rejected");
        }
    }

    #[tokio::test]
    async fn frontmatter_set_missing_value() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();
        vault.write_note(Path::new("fm.md"), "# Note\n").unwrap();

        let result = note_frontmatter_edit(
            &vault,
            NoteFrontmatterEditParams {
                action: "set".into(),
                path: "fm.md".into(),
                key: "k".into(),
                value: None,
            },
        )
        .await;
        assert!(result.is_err());
    }

    /// The split is the whole point of this pair of tools, so assert it: the
    /// read tool has no way to express a write, and the write tool has no way
    /// to express a read. That is what lets a name-based filter be trusted.
    #[tokio::test]
    async fn reading_and_editing_frontmatter_are_separate_tools() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();
        vault.write_note(Path::new("fm.md"), "# Note\n").unwrap();

        note_frontmatter_edit(
            &vault,
            NoteFrontmatterEditParams {
                action: "set".into(),
                path: "fm.md".into(),
                key: "status".into(),
                value: Some(serde_json::json!("draft")),
            },
        )
        .await
        .unwrap();

        let before = vault.read_note(Path::new("fm.md")).unwrap();

        // The read tool takes only a path: no action, no key, no value.
        let result = note_frontmatter(
            &vault,
            NoteFrontmatterParams {
                path: "fm.md".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(result.structured_content.unwrap()["status"], "draft");

        assert_eq!(
            before,
            vault.read_note(Path::new("fm.md")).unwrap(),
            "reading frontmatter must not modify the note"
        );
    }
}
