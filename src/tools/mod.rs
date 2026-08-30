//! MCP tool handlers — thin wrappers that translate MCP requests into vault operations.

pub mod graph;
pub mod metadata;
pub mod navigation;
pub mod notes;
pub mod periodic;
pub mod related;
pub mod search;
pub mod utility;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, ErrorData, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::client::semantic_daemon::SemanticDaemonClient;
use crate::config::SemanticMode;
use crate::vault::Vault;

/// JSON Schema for a value that may be any JSON type.
///
/// `serde_json::Value` deliberately emits an unconstrained schema in Schemars,
/// so tool inputs need this explicit schema to prevent clients from guessing
/// that structured values should be JSON-encoded strings.
pub(crate) fn json_value_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["array", "boolean", "null", "number", "object", "string"]
    })
}

/// JSON Schema for an optional object-valued tool input.
pub(crate) fn json_object_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["object", "null"],
        "additionalProperties": true
    })
}

/// Deserialize an optional dynamic JSON field while preserving explicit null.
///
/// Serde normally maps both a missing `Option<Value>` and an explicit JSON
/// `null` to `None`. The tool handlers need to distinguish those cases because
/// null is a valid frontmatter value.
pub(crate) fn deserialize_optional_json_value<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <serde_json::Value as serde::Deserialize>::deserialize(deserializer).map(Some)
}

/// Deserialize an optional object-valued input and reject every other JSON type.
pub(crate) fn deserialize_optional_json_object<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<serde_json::Value> as serde::Deserialize>::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::Object(_)) | None => Ok(value),
        Some(_) => Err(<D::Error as serde::de::Error>::custom(
            "expected a JSON object or null",
        )),
    }
}

#[derive(Clone)]
pub struct SemanticRuntime {
    pub mode: SemanticMode,
    pub daemon_client: Option<SemanticDaemonClient>,
    pub daemon_unavailable_reason: Option<String>,
    pub prefetch_count: usize,
    pub vault_ensured: Arc<AtomicBool>,
}

pub struct ObsidianMcp {
    vault: Vault,
    hybrid_alpha: f32,
    semantic_runtime: SemanticRuntime,
    /// The router the handler dispatches against. Disabled routes are applied
    /// here in `new`, so this must stay the router `#[tool_handler]` is pointed
    /// at — see the comment on that attribute.
    pub tool_router: ToolRouter<Self>,
}

/// The tools a server carrying this `disabled` set actually advertises.
///
/// Built from the same router the request path dispatches against, so a status
/// page cannot drift from what clients really see when a filter is active.
pub fn tool_manifest(disabled: &HashSet<String>) -> serde_json::Value {
    let mut router = ObsidianMcp::tool_router();
    for name in disabled {
        router.disable_route(name.clone());
    }
    serde_json::to_value(router.list_all()).unwrap_or_else(|_| serde_json::json!([]))
}

#[tool_router]
impl ObsidianMcp {
    pub fn new(
        vault: Vault,
        hybrid_alpha: f32,
        semantic_runtime: SemanticRuntime,
        disabled_tools: HashSet<String>,
    ) -> Self {
        let mut tool_router = Self::tool_router();
        if !disabled_tools.is_empty() {
            tracing::info!(
                count = disabled_tools.len(),
                "disabling tools per filter config"
            );
            for name in disabled_tools {
                tool_router.disable_route(name);
            }
        }
        Self {
            tool_router,
            vault,
            hybrid_alpha,
            semantic_runtime,
        }
    }

    // ── Search: finding notes you cannot name ───────────────────────
    //
    // Every description below follows one shape — what the tool does, when to
    // reach for it *instead of its siblings*, and what it hands back. The
    // middle clause is the one that matters: an agent choosing between five
    // search tools has nothing else to go on, and a wrong choice looks like an
    // empty vault rather than a mistake.

    #[tool(
        name = "search_semantic",
        description = "Find notes by meaning rather than wording. Use this when you have a question or an idea and do not know which words the note actually uses; use search_text instead when you know the terms that appear in it. Returns `{ results: [...] }`, ranked most similar first. Each hit carries a snippet plus its provenance: `match_type` says WHY the note ranked — \"chunk\" (one specific passage matched) or \"summary\" (the note's overall gist matched) — and `best_chunk` gives the closest passage with its `heading_path` and its own score. On a \"summary\" match that passage did NOT cause the ranking, so cite it as context, never as the reason the note was found. Provenance is omitted only when you pass lexical_prefetch:true, where a blended rank is not attributable to one representation: treat missing `match_type` as \"unknown\", never as \"chunk\". Requires semantic search to be enabled; while the index is still building this returns an explicit \"warming\" error with progress rather than degrading silently."
    )]
    async fn search_semantic(
        &self,
        Parameters(params): Parameters<search::SearchSemanticParams>,
    ) -> Result<Json<search::SemanticSearchOutput>, ErrorData> {
        search::search_semantic(
            &self.vault,
            params,
            self.hybrid_alpha,
            &self.semantic_runtime,
        )
        .await
    }

    #[tool(
        name = "search_text",
        description = "Find notes containing particular words, ranked by BM25. Use when you know terms that literally appear in the note; use search_semantic when you only know the idea. Stems words, so \"program\" matches \"programming\", and can tolerate typos when fuzzy matching is enabled. Returns ranked note paths with a relevance score and the matching context snippet."
    )]
    async fn search_text(
        &self,
        Parameters(params): Parameters<search::SearchTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        search::search_text(&self.vault, params).await
    }

    #[tool(
        name = "search_regex",
        description = "Find notes whose text matches a regular expression. Use for structural patterns that word search cannot express — dates, identifiers, TODO markers, code shapes. Prefer search_text for ordinary words: regex does no stemming and no relevance ranking. Returns matching note paths with context snippets."
    )]
    async fn search_regex(
        &self,
        Parameters(params): Parameters<search::SearchRegexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        search::search_regex(&self.vault, params).await
    }

    #[tool(
        name = "search_tags",
        description = "Find notes carrying a tag, matching both inline #tags and frontmatter tags. Use when filtering by an explicit label the user applied, rather than by what a note says. `include_nested` (default true) also matches children, so \"inbox\" matches \"inbox/read\". Returns the matching note paths."
    )]
    async fn search_tags(
        &self,
        Parameters(params): Parameters<search::SearchTagsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        search::search_tags(&self.vault, params).await
    }

    #[tool(
        name = "search_frontmatter",
        description = "Find notes by the value of a frontmatter field. Use for structured properties kept in YAML — status, type, author, date — rather than prose; use search_tags for tags specifically. `operator` is \"eq\" (default), \"contains\" (substring for strings, membership for arrays), or \"exists\" (value ignored). Returns the matching note paths."
    )]
    async fn search_frontmatter(
        &self,
        Parameters(params): Parameters<search::SearchFrontmatterParams>,
    ) -> Result<CallToolResult, ErrorData> {
        search::search_frontmatter(&self.vault, params).await
    }

    // ── Relate: starting from a note you already have ───────────────

    #[tool(
        name = "note_related",
        description = "Find notes about the same subject as a note you already have, seeded from that note's own embedding — no query string needed. Use when you have a note and want its neighbours; use search_semantic when you have a question instead of a note. Each result carries `linked`: false means no wikilink joins them yet, which is usually the interesting case — a connection the vault has not recorded. Also returns the note's existing outgoing links and backlinks for comparison. Results carry the same `match_type` / `best_chunk` provenance as search_semantic, with the same caveat about summary matches. Requires semantic search to be enabled."
    )]
    async fn note_related(
        &self,
        Parameters(params): Parameters<related::NoteRelatedParams>,
    ) -> Result<Json<related::NoteRelatedResult>, ErrorData> {
        related::note_related(&self.vault, params).await
    }

    #[tool(
        name = "note_links",
        description = "Get both link directions for one note in a single call. Use to see where a note sits in the graph as actually recorded; use note_related for connections by meaning that the graph does not yet capture. Returns `{ path, backlinks, outgoing }` — backlinks being the notes that link TO it with the specific wikilinks involved, outgoing being the links FROM it, each with its resolution status (`resolved_path` is null when the link is broken)."
    )]
    async fn note_links(
        &self,
        Parameters(params): Parameters<graph::NoteLinksParams>,
    ) -> Result<CallToolResult, ErrorData> {
        graph::note_links(&self.vault, params).await
    }

    #[tool(
        name = "vault_broken_links",
        description = "List wikilinks that point at nothing — vault-wide, or within one note if `path` is given. Use to find typos and links left dangling by a rename. Returns each broken link with its source note, the raw link text, and the unresolved target."
    )]
    async fn vault_broken_links(
        &self,
        Parameters(params): Parameters<graph::VaultBrokenLinksParams>,
    ) -> Result<CallToolResult, ErrorData> {
        graph::vault_broken_links(&self.vault, params).await
    }

    #[tool(
        name = "vault_orphans",
        description = "List notes disconnected from the resolvable link graph. Use to find notes nothing points to. Returns each note with a `status` distinguishing \"no_links\" (nothing in either direction) from \"broken_outgoing_only\" (it links out, but every one of those links is broken), plus the broken targets involved."
    )]
    async fn vault_orphans(
        &self,
        Parameters(params): Parameters<graph::VaultOrphansParams>,
    ) -> Result<CallToolResult, ErrorData> {
        graph::vault_orphans(&self.vault, params).await
    }

    // ── Read ────────────────────────────────────────────────────────

    #[tool(
        name = "note_read",
        description = "Read one note's full content as raw markdown, frontmatter included. Use when you know the path and need the text. Use note_read_many for several notes at once, or note_metadata when you only need its tags, headings and links rather than its body."
    )]
    async fn note_read(
        &self,
        Parameters(params): Parameters<notes::NoteReadParams>,
    ) -> Result<String, ErrorData> {
        notes::note_read(&self.vault, params).await
    }

    #[tool(
        name = "note_read_many",
        description = "Read several notes in one bounded call. Use instead of repeated note_read calls. Provide exactly one of `paths` or `dir`; directory reads are non-recursive unless asked otherwise. Inspects at most 100 files and returns at most 262144 combined content bytes — anything left out is listed in `skipped` with a reason, so check that field rather than assuming you received everything. Fall back to note_read for a single deliberately oversized note."
    )]
    async fn note_read_many(
        &self,
        Parameters(params): Parameters<notes::NoteReadManyParams>,
    ) -> Result<Json<notes::NoteReadManyOutput>, ErrorData> {
        notes::note_read_many(&self.vault, params).await
    }

    #[tool(
        name = "note_metadata",
        description = "Get one note's metadata without its body. Use to judge a note cheaply before deciding whether to read it, or to count its backlinks. Returns title, tags, frontmatter, headings, outgoing links, block references, backlinks count and file stats. Use note_read for the text itself."
    )]
    async fn note_metadata(
        &self,
        Parameters(params): Parameters<metadata::NoteMetadataParams>,
    ) -> Result<CallToolResult, ErrorData> {
        metadata::note_metadata(&self.vault, params).await
    }

    #[tool(
        name = "note_frontmatter",
        description = "Read a note's frontmatter as JSON, or null if it has none. Read-only — use note_frontmatter_edit to change a field."
    )]
    async fn note_frontmatter(
        &self,
        Parameters(params): Parameters<metadata::NoteFrontmatterParams>,
    ) -> Result<CallToolResult, ErrorData> {
        metadata::note_frontmatter(&self.vault, params).await
    }

    #[tool(
        name = "note_patch_targets",
        description = "List the addressable targets in a note: headings with their Markdown level markers (\"## Log\"), block references, and frontmatter field names. Call this before note_patch to learn exactly which `target` values that note will accept, rather than guessing. Returns `{ headings, block_refs, frontmatter_fields }`."
    )]
    async fn note_patch_targets(
        &self,
        Parameters(params): Parameters<metadata::NotePatchTargetsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        metadata::note_patch_targets(&self.vault, params).await
    }

    // ── Write: these modify the vault ───────────────────────────────

    #[tool(
        name = "note_create",
        description = "Create a new note, with optional content and YAML frontmatter. Parent directories are created automatically. Fails if the note already exists — use note_write to replace one that does."
    )]
    async fn note_create(
        &self,
        Parameters(params): Parameters<notes::NoteCreateParams>,
    ) -> Result<String, ErrorData> {
        notes::note_create(&self.vault, params).await
    }

    #[tool(
        name = "note_write",
        description = "Replace a note's entire content. The note must already exist; use note_create for a new one. This discards whatever is currently there — prefer note_insert to add to a note, or note_patch to change one section of it."
    )]
    async fn note_write(
        &self,
        Parameters(params): Parameters<notes::NoteWriteParams>,
    ) -> Result<String, ErrorData> {
        notes::note_write(&self.vault, params).await
    }

    #[tool(
        name = "note_insert",
        description = "Add content to an existing note without replacing what is there. `position` \"end\" (default) appends; \"beginning\" inserts after the frontmatter, or at the very start if the note has none. Use note_patch instead when you need to land the content inside a specific section."
    )]
    async fn note_insert(
        &self,
        Parameters(params): Parameters<notes::NoteInsertParams>,
    ) -> Result<String, ErrorData> {
        notes::note_insert(&self.vault, params).await
    }

    #[tool(
        name = "note_patch",
        description = "Modify one section of a note, addressed by heading, block reference, or frontmatter field, with `operation` append, prepend or replace. Call note_patch_targets first to learn the valid `target` values for that note; heading targets accept either bare text (\"Log\") or the marker-prefixed form (\"## Log\")."
    )]
    async fn note_patch(
        &self,
        Parameters(params): Parameters<notes::NotePatchParams>,
    ) -> Result<String, ErrorData> {
        notes::note_patch(&self.vault, params).await
    }

    #[tool(
        name = "note_frontmatter_edit",
        description = "Set or remove a single frontmatter field. `action` is \"set\" (upsert, requires `value`) or \"remove\". Pass arrays and objects as real JSON, not as encoded strings. Reading frontmatter is a separate tool, note_frontmatter."
    )]
    async fn note_frontmatter_edit(
        &self,
        Parameters(params): Parameters<metadata::NoteFrontmatterEditParams>,
    ) -> Result<CallToolResult, ErrorData> {
        metadata::note_frontmatter_edit(&self.vault, params).await
    }

    #[tool(
        name = "note_move",
        description = "Move or rename a note. Destination parent directories are created automatically. Wikilinks in other notes are NOT rewritten, so a rename can leave links pointing at the old name — run vault_broken_links afterwards to find any this breaks."
    )]
    async fn note_move(
        &self,
        Parameters(params): Parameters<notes::NoteMoveParams>,
    ) -> Result<String, ErrorData> {
        notes::note_move(&self.vault, params).await
    }

    #[tool(
        name = "note_delete",
        description = "Delete a note from the vault. Requires `confirm: true`, so that a call made by mistake fails instead of destroying a note. This is irreversible and there is no undo."
    )]
    async fn note_delete(
        &self,
        Parameters(params): Parameters<notes::NoteDeleteParams>,
    ) -> Result<String, ErrorData> {
        notes::note_delete(&self.vault, params).await
    }

    // ── Periodic notes ──────────────────────────────────────────────

    #[tool(
        name = "periodic_get",
        description = "Read the periodic note for a date — daily, weekly, monthly, quarterly or yearly. `date` defaults to today. Read-only: if the note does not exist this returns an error rather than creating it. Use periodic_create to make one."
    )]
    async fn periodic_get(
        &self,
        Parameters(params): Parameters<periodic::PeriodicGetParams>,
    ) -> Result<String, ErrorData> {
        periodic::periodic_get(&self.vault, params).await
    }

    #[tool(
        name = "periodic_list",
        description = "List recent periodic notes of one period, newest first. Use to find which dates actually have notes before reading them. `limit` defaults to 10. Returns a path and date for each."
    )]
    async fn periodic_list(
        &self,
        Parameters(params): Parameters<periodic::PeriodicListParams>,
    ) -> Result<String, ErrorData> {
        periodic::periodic_list(&self.vault, params).await
    }

    #[tool(
        name = "periodic_create",
        description = "Create the periodic note for a date, expanded from its configured template or from `content` if you supply it. `date` defaults to today. This writes to the vault."
    )]
    async fn periodic_create(
        &self,
        Parameters(params): Parameters<periodic::PeriodicCreateParams>,
    ) -> Result<String, ErrorData> {
        periodic::periodic_create(&self.vault, params).await
    }

    // ── Vault ───────────────────────────────────────────────────────

    #[tool(
        name = "vault_list",
        description = "List files and directories. Use to explore how the vault is organised when you do not yet know what exists; use the search tools once you know what you are looking for. Supports recursive listing, glob filtering, and a tree view. Returns an array of paths, or objects with title, tags, size and timestamps when `include_metadata` is true; `format: \"tree\"` returns a formatted string instead."
    )]
    async fn vault_list(
        &self,
        Parameters(params): Parameters<navigation::VaultListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        navigation::vault_list(&self.vault, params)
    }

    #[tool(
        name = "vault_info",
        description = "Aggregate statistics for the whole vault: total notes, files, tags, links and size in bytes. Use to gauge scale before a broad operation, or to confirm which vault you are connected to."
    )]
    async fn vault_info(
        &self,
        Parameters(params): Parameters<utility::VaultInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        utility::vault_info(&self.vault, params).await
    }

    // ── Utility ─────────────────────────────────────────────────────

    #[tool(
        name = "open_in_obsidian",
        description = "Open a note in the Obsidian desktop app via the obsidian:// URI scheme. This acts on the user's machine rather than reading the vault, and requires Obsidian to be installed."
    )]
    async fn open_in_obsidian(
        &self,
        Parameters(params): Parameters<utility::OpenInObsidianParams>,
    ) -> Result<CallToolResult, ErrorData> {
        utility::open_in_obsidian(&self.vault, params).await
    }
}

// `router = self.tool_router` is load-bearing. Without it the macro defaults to
// `Self::tool_router()`, which builds a *fresh* router per request and so ignores
// the per-instance disabled set applied in `new`. That made `OBSIDIAN_TOOLS`
// silently ineffective on both transports: the filter was parsed, logged and
// stored on the field, yet `list_tools` still advertised every tool and
// `call_tool` executed write tools on a read-only server.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for ObsidianMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Obsidian vault over direct filesystem access, with chunk-level \
                 semantic retrieval alongside the wikilink graph.\n\
                 \n\
                 Choosing a search tool is the decision that matters most:\n\
                 - You know words that appear in the note -> search_text\n\
                 - You know the idea but not the wording -> search_semantic\n\
                 - You need a structural pattern -> search_regex\n\
                 - You are filtering by a label or YAML property -> search_tags, \
                 search_frontmatter\n\
                 - You already have a note and want its neighbours -> note_related \
                 (by meaning) or note_links (by recorded wikilink)\n\
                 \n\
                 Semantic results carry their own provenance. `match_type` says \
                 why a note ranked: \"chunk\" means one passage matched, \"summary\" \
                 means the note's overall gist did. `best_chunk` is the closest \
                 passage either way, but on a summary match it did not cause the \
                 ranking - report it as context, not as the reason. The fields \
                 are omitted only on the hybrid path (lexical_prefetch:true), \
                 where a blended rank is not attributable to one representation; \
                 absent means unknown, never \"chunk\".\n\
                 \n\
                 Not every tool listed here is always available: the server can be \
                 started with a restricted tool set, in which case the hidden tools \
                 are absent from tools/list rather than failing when called. Work \
                 from the list you were given.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ALL_TOOL_NAMES;
    use crate::test_helpers::{create_test_vault, test_config};
    use crate::vault::Vault;
    use rmcp::ServiceExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn test_runtime() -> SemanticRuntime {
        SemanticRuntime {
            mode: SemanticMode::Local,
            daemon_client: None,
            daemon_unavailable_reason: None,
            prefetch_count: 50,
            vault_ensured: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Drive a real JSON-RPC session against `server` over an in-memory duplex
    /// transport, returning the response to `method`.
    ///
    /// Asserting on `server.tool_router` only proves the field was mutated — it
    /// cannot catch a handler that consults a *different* router, which is
    /// exactly how `OBSIDIAN_TOOLS` came to be inert while its own tests passed.
    /// Anything about tool visibility or dispatch has to go through here.
    async fn rpc_raw(
        server: ObsidianMcp,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let (server_transport, client_transport) = tokio::io::duplex(1024 * 1024);
        let server_handle = tokio::spawn(async move {
            server
                .serve(server_transport)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let (client_read, mut client_write) = tokio::io::split(client_transport);
        let mut client_lines = BufReader::new(client_read).lines();

        let mut initialize = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0.0.1"}
            }
        }))
        .unwrap();
        initialize.push(b'\n');
        client_write.write_all(&initialize).await.unwrap();
        let _initialize_response = client_lines.next_line().await.unwrap().unwrap();

        let mut initialized = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .unwrap();
        initialized.push(b'\n');
        client_write.write_all(&initialized).await.unwrap();

        let mut call = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": method,
            "params": params
        }))
        .unwrap();
        call.push(b'\n');
        client_write.write_all(&call).await.unwrap();

        let response =
            serde_json::from_str(&client_lines.next_line().await.unwrap().unwrap()).unwrap();
        client_write.shutdown().await.unwrap();
        drop(client_lines);
        server_handle.await.unwrap();
        response
    }

    async fn call_tool_raw(
        server: ObsidianMcp,
        name: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        rpc_raw(
            server,
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    /// Tool names actually advertised to a client by `tools/list`.
    async fn advertised_tools(server: ObsidianMcp) -> HashSet<String> {
        let response = rpc_raw(server, "tools/list", serde_json::json!({})).await;
        response["result"]["tools"]
            .as_array()
            .expect("tools/list returns an array")
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn no_disabled_tools_exposes_all() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), HashSet::new());

        for name in ALL_TOOL_NAMES {
            assert!(
                server.tool_router.has_route(name),
                "expected tool '{name}' to be enabled"
            );
        }
    }

    #[tokio::test]
    async fn disabled_tools_are_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();

        let disabled: HashSet<String> = ["open_in_obsidian", "wikilinks", "periodic"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), disabled);

        assert!(!server.tool_router.has_route("open_in_obsidian"));
        assert!(!server.tool_router.has_route("wikilinks"));
        assert!(!server.tool_router.has_route("periodic"));

        assert!(server.tool_router.has_route("note_read"));
        assert!(server.tool_router.has_route("vault_list"));
        assert!(server.tool_router.has_route("search_text"));
    }

    #[tokio::test]
    async fn disable_all_tools_hides_everything() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();

        let disabled: HashSet<String> = ALL_TOOL_NAMES.iter().map(|s| s.to_string()).collect();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), disabled);

        for name in ALL_TOOL_NAMES {
            assert!(
                !server.tool_router.has_route(name),
                "expected tool '{name}' to be disabled"
            );
        }
    }

    /// The three tests above assert on the router *field*. That is necessary but
    /// not sufficient: the handler macro can be pointed at a different router, in
    /// which case the field is disabled and the server still serves everything.
    /// These drive the wire protocol instead.
    #[tokio::test]
    async fn disabled_tools_are_absent_from_tools_list() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();

        let disabled: HashSet<String> = ["note_delete", "note_write", "note_move"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), disabled.clone());

        let advertised = advertised_tools(server).await;

        for name in &disabled {
            assert!(
                !advertised.contains(name),
                "tools/list advertised '{name}', which is disabled"
            );
        }
        assert!(
            advertised.contains("note_read"),
            "tools/list dropped an enabled tool"
        );
        assert_eq!(advertised.len(), ALL_TOOL_NAMES.len() - disabled.len());
    }

    #[tokio::test]
    async fn calling_a_disabled_tool_is_rejected_before_it_runs() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();

        let disabled: HashSet<String> = ["note_delete"].iter().map(|s| s.to_string()).collect();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), disabled);

        let response = call_tool_raw(
            server,
            "note_delete",
            serde_json::json!({ "path": "note.md", "confirm": true }),
        )
        .await;

        let error = response
            .get("error")
            .unwrap_or_else(|| panic!("disabled tool returned a result, not an error: {response}"));
        let message = error["message"].as_str().unwrap_or_default();

        // A vault-layer answer ("Note not found", "deleted") would mean the call
        // reached the tool. The rejection has to happen before dispatch.
        assert!(
            message.contains("tool not found"),
            "expected a routing rejection, got: {message}"
        );
    }

    #[tokio::test]
    async fn enabled_tools_still_work_while_a_filter_is_active() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();

        let disabled: HashSet<String> = ["note_delete"].iter().map(|s| s.to_string()).collect();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), disabled);

        let response = call_tool_raw(server, "vault_info", serde_json::json!({})).await;

        assert!(
            response.get("error").is_none(),
            "an enabled tool was rejected while a filter was active: {response}"
        );
    }

    #[tokio::test]
    async fn frontmatter_tool_inputs_publish_explicit_json_types() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), HashSet::new());

        let input_schema = |name: &str| {
            let tool = server
                .tool_router
                .get(name)
                .unwrap_or_else(|| panic!("missing tool '{name}'"));
            serde_json::Value::Object(tool.input_schema.as_ref().clone())
        };

        let note_create = input_schema("note_create");
        assert_eq!(
            note_create.pointer("/properties/frontmatter/type"),
            Some(&serde_json::json!(["object", "null"]))
        );
        assert_eq!(
            note_create.pointer("/properties/frontmatter/additionalProperties"),
            Some(&serde_json::json!(true))
        );

        let dynamic_types =
            serde_json::json!(["array", "boolean", "null", "number", "object", "string"]);
        for tool_name in ["note_frontmatter_edit", "search_frontmatter"] {
            let schema = input_schema(tool_name);
            assert_eq!(
                schema.pointer("/properties/value/type"),
                Some(&dynamic_types),
                "unexpected value schema for '{tool_name}'"
            );
            assert!(
                !schema["required"]
                    .as_array()
                    .is_some_and(|required| required.contains(&serde_json::json!("value"))),
                "'value' must remain optional for '{tool_name}'"
            );
        }
    }

    /// The retrieval tools publish an output schema so a client can learn what
    /// `match_type` and `best_chunk` mean without being told out of band.
    ///
    /// This also pins the shape: MCP requires an object-rooted `outputSchema`,
    /// and returning the ranked list bare produced an array root, which the
    /// macro rejects at runtime — a panic on first call rather than a compile
    /// error, so nothing but a test catches it.
    #[tokio::test]
    async fn retrieval_tools_publish_object_rooted_output_schemas() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), HashSet::new());

        for name in ["search_semantic", "note_related"] {
            let tool = server
                .tool_router
                .get(name)
                .unwrap_or_else(|| panic!("missing tool '{name}'"));
            let schema = tool
                .output_schema
                .as_ref()
                .unwrap_or_else(|| panic!("'{name}' publishes no output schema"));
            let schema = serde_json::Value::Object(schema.as_ref().clone());

            assert_eq!(
                schema.pointer("/type"),
                Some(&serde_json::json!("object")),
                "'{name}' output schema must be rooted at an object"
            );

            let rendered = schema.to_string();
            for field in ["match_type", "best_chunk"] {
                assert!(
                    rendered.contains(field),
                    "'{name}' output schema does not describe '{field}'"
                );
            }
        }
    }

    #[tokio::test]
    async fn note_read_many_publishes_typed_input_and_output_schemas() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), HashSet::new());
        let tool = server
            .tool_router
            .get("note_read_many")
            .expect("missing note_read_many tool");

        let input_schema = serde_json::Value::Object(tool.input_schema.as_ref().clone());
        assert_eq!(
            input_schema.pointer("/properties/paths/maxItems"),
            Some(&serde_json::json!(100))
        );
        assert_eq!(
            input_schema.pointer("/properties/max_files/maximum"),
            Some(&serde_json::json!(100))
        );
        assert_eq!(
            input_schema.pointer("/properties/max_bytes/maximum"),
            Some(&serde_json::json!(262144))
        );

        let output_schema = serde_json::Value::Object(
            tool.output_schema
                .as_ref()
                .expect("note_read_many must advertise outputSchema")
                .as_ref()
                .clone(),
        );
        assert_eq!(output_schema["type"], "object");
        for field in ["notes", "skipped", "skipped_count", "content_bytes"] {
            assert!(
                output_schema["required"]
                    .as_array()
                    .is_some_and(|required| required.contains(&serde_json::json!(field))),
                "output schema must require '{field}'"
            );
        }
    }

    #[tokio::test]
    async fn note_read_many_raw_mcp_returns_matching_text_and_structured_content() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        std::fs::write(tmp.path().join("one.md"), "one").unwrap();
        std::fs::write(tmp.path().join("two.md"), "two").unwrap();
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), HashSet::new());

        let response = call_tool_raw(
            server,
            "note_read_many",
            serde_json::json!({"paths": ["two.md", "one.md"]}),
        )
        .await;

        let text = response
            .pointer("/result/content/0/text")
            .and_then(serde_json::Value::as_str)
            .expect("missing compatibility text content");
        let text_value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(text_value, response["result"]["structuredContent"]);
        assert_eq!(
            text_value.pointer("/notes/0/path"),
            Some(&serde_json::json!("two.md"))
        );
        assert_eq!(
            text_value.pointer("/notes/1/path"),
            Some(&serde_json::json!("one.md"))
        );
        assert_eq!(text_value["skipped_count"], 0);
    }

    #[tokio::test]
    async fn note_create_rejects_stringified_frontmatter_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        create_test_vault(tmp.path());
        let vault = Vault::open(&test_config(tmp.path())).await.unwrap();
        let server = ObsidianMcp::new(vault, 0.25, test_runtime(), HashSet::new());

        let response = call_tool_raw(
            server,
            "note_create",
            serde_json::json!({
                "path": "invalid.md",
                "content": "body",
                "frontmatter": "{\"tags\":[\"rust\",\"mcp\"]}"
            }),
        )
        .await;
        assert_eq!(response["error"]["code"], -32602);
        assert!(!tmp.path().join("invalid.md").exists());
    }
}
