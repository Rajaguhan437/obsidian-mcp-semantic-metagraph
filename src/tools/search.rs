//! Text, regex, tag, and frontmatter search tools across vault notes.

use rmcp::model::{CallToolResult, Content, ErrorCode};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::SemanticMode;
use crate::daemon::protocol;
use crate::error::VaultError;
use crate::models::SearchField;
use crate::vault::Vault;

use super::SemanticRuntime;

const MAX_RESULTS_CAP: usize = 200;
const MAX_CONTEXT_LEN_CAP: usize = 2000;
const SEMANTIC_FILTER_OVERFETCH_FACTOR: usize = 4;
const SEMANTIC_FILTER_OVERFETCH_MIN_EXTRA: usize = 20;

// ── search_text ─────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Default)]
pub struct SearchTextParams {
    /// Natural-language search query. Supports stemming (e.g. "program"
    /// matches "programming"). Results are ranked by BM25 relevance.
    pub query: String,
    /// Characters of context around each match (default: 100).
    #[serde(default)]
    pub context_length: Option<usize>,
    /// Maximum number of file results to return (default: 20).
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Enable fuzzy matching with edit distance 1 (tolerates typos). Default: false.
    #[serde(default)]
    pub fuzzy: Option<bool>,
    /// Restrict search to specific note fields. Default: all fields.
    /// Allowed values: `title`, `headings`, `tags`, `body`, `frontmatter`.
    #[serde(default)]
    pub fields: Option<Vec<SearchField>>,
}

pub async fn search_text(
    vault: &Vault,
    params: SearchTextParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let context_length = params
        .context_length
        .unwrap_or(100)
        .min(MAX_CONTEXT_LEN_CAP);
    let max_results = params.max_results.unwrap_or(20).min(MAX_RESULTS_CAP);
    let fuzzy = params.fuzzy.unwrap_or(false);

    let results = if fuzzy || params.fields.is_some() {
        let fields_slice = params.fields.as_deref();
        vault.search_text_with_options(
            &params.query,
            context_length,
            max_results,
            fuzzy,
            fields_slice,
        )?
    } else {
        let all = vault.search_text(&params.query, context_length)?;
        all.into_iter().take(max_results).collect()
    };

    let json = serde_json::to_string_pretty(&results)
        .map_err(|e| VaultError::Other(format!("JSON serialization failed: {e}")))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ── search_regex ────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Default)]
pub struct SearchRegexParams {
    /// Regular expression pattern to search for.
    pub pattern: String,
    /// Characters of context around each match (default: 100).
    #[serde(default)]
    pub context_length: Option<usize>,
    /// Maximum number of file results to return (default: 20).
    #[serde(default)]
    pub max_results: Option<usize>,
}

pub async fn search_regex(
    vault: &Vault,
    params: SearchRegexParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let context_length = params
        .context_length
        .unwrap_or(100)
        .min(MAX_CONTEXT_LEN_CAP);
    let max_results = params.max_results.unwrap_or(20).min(MAX_RESULTS_CAP);

    let results = vault.search_regex(&params.pattern, context_length)?;
    let limited: Vec<_> = results.into_iter().take(max_results).collect();

    let json = serde_json::to_string_pretty(&limited)
        .map_err(|e| VaultError::Other(format!("JSON serialization failed: {e}")))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ── search_metadata ─────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum FrontmatterOperator {
    /// Exact equality (or array-contains for list fields).
    #[default]
    Eq,
    /// Substring match for strings; element membership for arrays.
    Contains,
    /// Field exists regardless of value.
    Exists,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct SearchMetadataParams {
    /// Type of metadata search: `"tag"` to find notes by tag, or `"frontmatter"` to query by frontmatter field.
    #[serde(rename = "type")]
    pub search_type: String,
    /// Tag to search for (without the `#` prefix). Required when type is `"tag"`.
    #[serde(default)]
    pub tag: Option<String>,
    /// If true, also match nested tags (e.g. `inbox` matches `inbox/read`). Default: true. Only used when type is `"tag"`.
    #[serde(default)]
    pub include_nested: Option<bool>,
    /// Frontmatter field name to query. Required when type is `"frontmatter"`.
    #[serde(default)]
    pub field: Option<String>,
    /// Value to compare against. Required for `eq` and `contains` operators;
    /// ignored for `exists`. Pass arrays and objects directly; a JSON-encoded
    /// string is compared as a literal string. Only used when type is `"frontmatter"`.
    #[serde(
        default,
        deserialize_with = "crate::tools::deserialize_optional_json_value"
    )]
    #[schemars(schema_with = "crate::tools::json_value_schema")]
    pub value: Option<serde_json::Value>,
    /// Comparison operator (default: `eq`). Only used when type is `"frontmatter"`.
    #[serde(default)]
    pub operator: Option<FrontmatterOperator>,
}

pub async fn search_metadata(
    vault: &Vault,
    params: SearchMetadataParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let search_type = params.search_type.as_str();

    if search_type.eq_ignore_ascii_case("tag") {
        search_metadata_tag(vault, &params)
    } else if search_type.eq_ignore_ascii_case("frontmatter") {
        search_metadata_frontmatter(vault, &params)
    } else {
        Err(rmcp::ErrorData::new(
            ErrorCode::INVALID_PARAMS,
            format!("Unknown type '{search_type}'. Valid values: \"tag\", \"frontmatter\""),
            None::<serde_json::Value>,
        ))
    }
}

fn search_metadata_tag(
    vault: &Vault,
    params: &SearchMetadataParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let tag = params.tag.as_deref().ok_or_else(|| {
        rmcp::ErrorData::new(
            ErrorCode::INVALID_PARAMS,
            "'tag' is required when type is \"tag\"",
            None::<serde_json::Value>,
        )
    })?;
    let tag = tag.strip_prefix('#').unwrap_or(tag);
    let include_nested = params.include_nested.unwrap_or(true);

    let results = if include_nested {
        vault.search_by_tag_prefix(tag)?
    } else {
        vault.search_by_tag(tag)?
    };

    let json = serde_json::to_string_pretty(&results)
        .map_err(|e| VaultError::Other(format!("JSON serialization failed: {e}")))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn search_metadata_frontmatter(
    vault: &Vault,
    params: &SearchMetadataParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let field = params.field.as_deref().ok_or_else(|| {
        rmcp::ErrorData::new(
            ErrorCode::INVALID_PARAMS,
            "'field' is required when type is \"frontmatter\"",
            None::<serde_json::Value>,
        )
    })?;
    let operator = params.operator.clone().unwrap_or_default();

    let results = match operator {
        FrontmatterOperator::Exists => vault.search_frontmatter_exists(field)?,
        FrontmatterOperator::Eq => {
            let value = params.value.as_ref().ok_or_else(|| {
                rmcp::ErrorData::new(
                    ErrorCode::INVALID_PARAMS,
                    "'value' is required for 'eq' operator",
                    None::<serde_json::Value>,
                )
            })?;
            vault.search_frontmatter(field, value)?
        }
        FrontmatterOperator::Contains => {
            let value = params.value.as_ref().ok_or_else(|| {
                rmcp::ErrorData::new(
                    ErrorCode::INVALID_PARAMS,
                    "'value' is required for 'contains' operator",
                    None::<serde_json::Value>,
                )
            })?;
            vault.search_frontmatter_contains(field, value)?
        }
    };

    let json = serde_json::to_string_pretty(&results)
        .map_err(|e| VaultError::Other(format!("JSON serialization failed: {e}")))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

// ── search_semantic ──────────────────────────────────────────────────

#[cfg(has_embeddings)]
const DEFAULT_PREFETCH_COUNT: usize = 50;
#[cfg(has_embeddings)]
const SNIPPET_CONTEXT_LEN: usize = 150;
#[cfg(has_embeddings)]
const SNIPPET_FALLBACK_CHARS: usize = 300;

#[derive(Deserialize, JsonSchema, Default)]
pub struct SearchSemanticParams {
    /// Natural-language query for semantic search. Does not require exact
    /// keyword matches — conceptually similar notes are returned.
    pub query: String,
    /// Number of results to return (default: 10).
    #[serde(default)]
    pub top_k: Option<usize>,
    /// If true, include the full note content in each result. Default: false.
    #[serde(default)]
    pub include_content: Option<bool>,
    /// When true, first retrieves top candidates via BM25 lexical search,
    /// then re-ranks by combining lexical and semantic scores. Produces
    /// higher-quality results than either approach alone. Requires both
    /// Tantivy and embeddings to be enabled. Default: false.
    #[serde(default)]
    pub lexical_prefetch: Option<bool>,
    /// Blending weight for hybrid re-ranking: `alpha * BM25 + (1-alpha) * semantic`.
    /// Only used when `lexical_prefetch` is true. Lower values favor semantic similarity.
    /// Overrides the `OBSIDIAN_HYBRID_ALPHA` env var for this query. Range: 0.0–1.0, default: 0.25.
    #[serde(default)]
    pub alpha: Option<f32>,
}

#[derive(serde::Serialize, JsonSchema)]
struct SemanticSearchResult {
    path: std::path::PathBuf,
    title: String,
    pub(crate) score: f32,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    /// Which representation produced `score`:
    ///
    /// - `"chunk"` — one specific passage caused this ranking. `best_chunk` is
    ///   that passage.
    /// - `"summary"` — the whole-note vector caused it (title + every heading +
    ///   the first 400 words). The note matched *as a whole*; `best_chunk` is
    ///   still supplied as its most relevant passage, but it did **not** cause
    ///   the ranking. Because that arm is weighted, a summary win is also why
    ///   `score` can exceed 1.0.
    /// - `"note"` — a legacy whole-note entry from a pre-chunking cache.
    ///
    /// Absent on the experimental hybrid path, where a blended rank is not
    /// attributable to one representation.
    #[serde(skip_serializing_if = "Option::is_none")]
    match_type: Option<&'static str>,
    /// The note's best-matching passage, supplied whenever the note has chunks
    /// — **including when `match_type` is `"summary"`**.
    ///
    /// Evidence and attribution are separate: this is always the most relevant
    /// passage, and `match_type` says whether it is also the reason the note
    /// ranked. Compare `best_chunk.score` against `summary_score` to see how
    /// close the two arms were.
    #[serde(skip_serializing_if = "Option::is_none")]
    best_chunk: Option<MatchedChunk>,
    /// The summary arm's weighted score, when the note has a summary vector.
    ///
    /// Deliberately a number, not the summary text: returning a 400-word
    /// summary per hit would dominate the response for no retrieval benefit.
    /// Use `note_read` when the whole note is actually wanted.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_score: Option<f32>,
}

/// A note's best-matching passage and where it lives.
#[derive(serde::Serialize, JsonSchema)]
pub(crate) struct MatchedChunk {
    /// 0-based index of the passage within the note.
    pub(crate) index: usize,
    /// Heading trail, outermost first, e.g. `["Design", "Retry policy"]`. An
    /// empty array means the passage sits above the note's first heading.
    pub(crate) heading_path: Vec<String>,
    /// The passage text, as embedded.
    pub(crate) passage: String,
    /// This chunk's raw cosine similarity — not the note's ranking `score`,
    /// which may have come from the summary arm instead.
    pub(crate) score: f32,
}

pub async fn search_semantic(
    vault: &Vault,
    params: SearchSemanticParams,
    default_alpha: f32,
    runtime: &SemanticRuntime,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let top_k = params.top_k.unwrap_or(10).min(MAX_RESULTS_CAP);
    let include_content = params.include_content.unwrap_or(false);
    let lexical_prefetch = params.lexical_prefetch.unwrap_or(false);
    let alpha = params.alpha.unwrap_or(default_alpha).clamp(0.0, 1.0);

    let results = match runtime.mode {
        SemanticMode::Daemon => {
            search_semantic_daemon(
                vault,
                &params,
                top_k,
                include_content,
                lexical_prefetch,
                alpha,
                runtime,
            )
            .await
        }
        SemanticMode::Local => search_semantic_local(
            vault,
            &params.query,
            top_k,
            include_content,
            lexical_prefetch,
            alpha,
        ),
        SemanticMode::Auto => match search_semantic_daemon(
            vault,
            &params,
            top_k,
            include_content,
            lexical_prefetch,
            alpha,
            runtime,
        )
        .await
        {
            Ok(results) => Ok(results),
            Err(err) if local_backend_available(vault) && should_fallback_to_local(&err) => {
                tracing::warn!(error = %err, "semantic daemon unavailable in auto mode; falling back to local embeddings backend");
                runtime
                    .vault_ensured
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                search_semantic_local(
                    vault,
                    &params.query,
                    top_k,
                    include_content,
                    lexical_prefetch,
                    alpha,
                )
            }
            Err(err) => Err(err),
        },
    }
    .map_err(to_semantic_tool_error)?;

    let json = serde_json::to_string_pretty(&results)
        .map_err(|e| VaultError::Other(format!("JSON serialization failed: {e}")))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn semantic_candidate_limit(top_k: usize) -> usize {
    if top_k == 0 {
        0
    } else {
        top_k
            .saturating_mul(SEMANTIC_FILTER_OVERFETCH_FACTOR)
            .max(top_k.saturating_add(SEMANTIC_FILTER_OVERFETCH_MIN_EXTRA))
            .min(MAX_RESULTS_CAP)
    }
}

async fn search_semantic_daemon(
    vault: &Vault,
    params: &SearchSemanticParams,
    top_k: usize,
    include_content: bool,
    lexical_prefetch: bool,
    alpha: f32,
    runtime: &SemanticRuntime,
) -> Result<Vec<SemanticSearchResult>, VaultError> {
    let Some(client) = runtime.daemon_client.as_ref() else {
        let reason = runtime
            .daemon_unavailable_reason
            .as_deref()
            .unwrap_or("semantic daemon client is not initialized");
        return Err(VaultError::DaemonUnavailable(reason.to_string()));
    };

    if !runtime
        .vault_ensured
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        match client.ensure_vault(vault.root(), true, None).await {
            Ok(_) => {
                runtime
                    .vault_ensured
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Err(err) => return Err(err),
        }
    }

    let candidate_limit = semantic_candidate_limit(top_k);
    let daemon_result = if lexical_prefetch {
        let prefetch_count = runtime.prefetch_count.max(candidate_limit);
        client
            .search_hybrid(
                vault.root(),
                &params.query,
                candidate_limit,
                prefetch_count,
                alpha,
                include_content,
            )
            .await?
    } else {
        client
            .search_semantic(
                vault.root(),
                &params.query,
                candidate_limit,
                include_content,
            )
            .await?
    };

    Ok(daemon_result
        .results
        .into_iter()
        .filter_map(|hit| {
            let path = std::path::PathBuf::from(hit.path);
            if vault.get_note_metadata(&path).is_err() {
                return None;
            }
            Some(SemanticSearchResult {
                path,
                title: hit.title,
                score: hit.score,
                tags: hit.tags,
                snippet: hit.snippet,
                content: hit.content,
                // The daemon IPC protocol carries note-level hits only, so
                // passage provenance is unavailable in `daemon` mode. The
                // fields are omitted rather than guessed.
                match_type: None,
                best_chunk: None,
                summary_score: None,
            })
        })
        .take(top_k)
        .collect())
}

/// Resolve a match back to its heading trail and passage text.
///
/// The passage is re-derived, not stored: `chunk_note` is deterministic for a
/// given body and config, so index `i` addresses exactly the passage the
/// indexer embedded. That keeps a second copy of the corpus out of the cache
/// and off disk, at the cost of chunking one note per result.
#[cfg(has_embeddings)]
#[derive(Default)]
pub(crate) struct Provenance {
    pub(crate) match_type: Option<&'static str>,
    pub(crate) best_chunk: Option<MatchedChunk>,
    pub(crate) summary_score: Option<f32>,
}

/// Build the reportable evidence for one hit.
///
/// The passage is re-derived rather than stored: `chunk_note` is deterministic
/// for a given body and config, so index `i` addresses exactly the passage the
/// indexer embedded. That keeps a second copy of the corpus off disk, at the
/// cost of chunking one note per result.
///
/// `best_chunk` is filled in regardless of which arm won. A summary win still
/// has a most-relevant passage, and withholding it would force an agent to
/// re-read the whole note to find what it already knows. `match_type` carries
/// the attribution, so supplying the passage is not a claim that it ranked.
#[cfg(has_embeddings)]
pub(crate) fn resolve_provenance(
    matched: Option<crate::vault::embeddings::NoteMatch>,
    note_text: Option<&str>,
    config: crate::vault::chunker::ChunkConfig,
) -> Provenance {
    use crate::vault::embeddings::MatchedOn;

    let Some(matched) = matched else {
        // Hybrid blending: the rank is not attributable to one representation.
        return Provenance::default();
    };

    let match_type = match matched.winner {
        MatchedOn::Chunk(_) => "chunk",
        MatchedOn::Summary => "summary",
        MatchedOn::WholeNote => "note",
    };

    let best_chunk = matched.best_chunk.and_then(|(index, score)| {
        let text = note_text?;
        let body = crate::vault::frontmatter::get_body(text);
        let chunk = crate::vault::chunker::chunk_note(body, config)
            .into_iter()
            .nth(index)?;
        Some(MatchedChunk {
            index,
            heading_path: chunk.heading_path,
            passage: chunk.text,
            score,
        })
    });

    Provenance {
        match_type: Some(match_type),
        best_chunk,
        summary_score: matched.summary_score,
    }
}

#[cfg(has_embeddings)]
fn search_semantic_local(
    vault: &Vault,
    query: &str,
    top_k: usize,
    include_content: bool,
    lexical_prefetch: bool,
    alpha: f32,
) -> Result<Vec<SemanticSearchResult>, VaultError> {
    let candidate_limit = semantic_candidate_limit(top_k);
    let hits: Vec<(
        std::path::PathBuf,
        f32,
        Option<crate::vault::embeddings::NoteMatch>,
    )> = if lexical_prefetch {
        vault
            .search_hybrid(
                query,
                candidate_limit,
                DEFAULT_PREFETCH_COUNT.max(candidate_limit),
                alpha,
            )?
            .into_iter()
            .map(|(path, score)| (path, score, None))
            .collect()
    } else {
        vault.search_semantic_detailed(query, candidate_limit)?
    };

    let word_re = if !include_content {
        compile_query_word_regex(query)
    } else {
        None
    };
    // Same body and same config the indexer chunked with, so `Chunk(i)` refers
    // to the same passage. A note edited since it was indexed can drift until
    // it is re-embedded; the heading is then stale rather than wrong-by-design.
    let chunk_config = crate::vault::chunker::ChunkConfig::from_env();

    let mut results = Vec::with_capacity(hits.len());
    for (path, score, matched) in hits {
        let meta = match vault.get_note_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let title = meta.title.clone();
        let tags = meta.tags.clone();
        let note_text = vault.read_note(&path).ok();

        let (content, snippet) = if include_content {
            (note_text.clone(), None)
        } else {
            let snip = note_text.as_ref().map(|text| {
                let body = crate::vault::frontmatter::get_body(text);
                if let Some(ref re) = word_re
                    && let Some(found) = re.find(body)
                {
                    let (ctx, _, _, _) = crate::vault::index::extract_match_context(
                        body,
                        found.start(),
                        found.end(),
                        SNIPPET_CONTEXT_LEN,
                    );
                    return ctx;
                }
                body_preview(text, SNIPPET_FALLBACK_CHARS)
            });
            (None, snip)
        };

        let provenance = resolve_provenance(matched, note_text.as_deref(), chunk_config);

        results.push(SemanticSearchResult {
            path,
            title,
            score,
            tags,
            snippet,
            content,
            match_type: provenance.match_type,
            best_chunk: provenance.best_chunk,
            summary_score: provenance.summary_score,
        });
        if results.len() == top_k {
            break;
        }
    }

    Ok(results)
}

#[cfg(not(has_embeddings))]
fn search_semantic_local(
    _vault: &Vault,
    _query: &str,
    _top_k: usize,
    _include_content: bool,
    _lexical_prefetch: bool,
    _alpha: f32,
) -> Result<Vec<SemanticSearchResult>, VaultError> {
    Err(VaultError::Embedding(
        "Semantic search is not available. Rebuild with --features embeddings or --features embeddings-api".to_string(),
    ))
}

#[cfg(has_embeddings)]
use crate::vault::search_utils::{body_preview, compile_query_word_regex};

fn local_backend_available(vault: &Vault) -> bool {
    #[cfg(has_embeddings)]
    {
        vault.embeddings_configured()
    }
    #[cfg(not(has_embeddings))]
    {
        let _ = vault;
        false
    }
}

fn should_fallback_to_local(err: &VaultError) -> bool {
    match err {
        VaultError::DaemonUnavailable(_)
        | VaultError::DaemonIpc(_)
        | VaultError::DaemonTimeout { .. }
        | VaultError::DaemonBootstrap(_) => true,
        VaultError::DaemonRpc { code, .. } => matches!(
            *code,
            protocol::ERR_DAEMON_UNAVAILABLE
                | protocol::ERR_BOOTSTRAP_REQUIRED
                | protocol::ERR_INCOMPATIBLE_API_VERSION
        ),
        _ => false,
    }
}

fn to_semantic_tool_error(err: VaultError) -> rmcp::ErrorData {
    match err {
        VaultError::Embedding(message) => rmcp::ErrorData::new(
            ErrorCode::INVALID_REQUEST,
            message,
            None::<serde_json::Value>,
        ),
        VaultError::DaemonRpc {
            code,
            message,
            data,
        } if code == protocol::ERR_VAULT_NOT_READY => {
            rmcp::ErrorData::new(ErrorCode::INVALID_REQUEST, message, data)
        }
        other => other.into(),
    }
}

// ── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::Path;
    #[cfg(unix)]
    use std::path::PathBuf;

    use super::*;

    /// A matched chunk index must resolve to that chunk's heading trail and
    /// text. Without this the server can rank by passage but can only tell an
    /// agent which note matched, not where in it - which is the difference
    /// between "somewhere in this 4000-word note" and a citable answer.
    #[cfg(has_embeddings)]
    #[test]
    fn provenance_resolves_a_chunk_to_its_heading_and_passage() {
        use crate::vault::chunker::ChunkConfig;
        use crate::vault::embeddings::{MatchedOn, NoteMatch};

        let note = "---\ntitle: Design\n---\n\
                    # Design\nintro paragraph\n\n\
                    ## Retry policy\nwe settled on five attempts with backoff\n\n\
                    ## Rollout\nship behind a flag\n";
        let config = ChunkConfig::default();
        let body = crate::vault::frontmatter::get_body(note);
        let chunks = crate::vault::chunker::chunk_note(body, config);
        let target = chunks
            .iter()
            .position(|c| c.text.contains("five attempts"))
            .expect("the retry section must be its own chunk");

        // A chunk win: the passage and its heading trail must be exact.
        let found = resolve_provenance(
            Some(NoteMatch {
                winner: MatchedOn::Chunk(target),
                score: 0.81,
                best_chunk: Some((target, 0.81)),
                summary_score: Some(0.70),
            }),
            Some(note),
            config,
        );
        assert_eq!(found.match_type, Some("chunk"));
        let chunk = found
            .best_chunk
            .expect("a chunk win must carry its passage");
        assert_eq!(chunk.index, target);
        assert_eq!(
            chunk.heading_path,
            vec!["Design".to_string(), "Retry policy".to_string()],
            "the trail must be segments, not a joined string"
        );
        assert!(
            chunk.passage.contains("five attempts"),
            "the passage must be the text that matched"
        );

        // A SUMMARY win must still carry the note's best passage as evidence,
        // while attributing the ranking honestly. Withholding it would force an
        // agent to re-read the whole note to find what is already known.
        let found = resolve_provenance(
            Some(NoteMatch {
                winner: MatchedOn::Summary,
                score: 0.90,
                best_chunk: Some((target, 0.62)),
                summary_score: Some(0.90),
            }),
            Some(note),
            config,
        );
        assert_eq!(found.match_type, Some("summary"));
        let chunk = found
            .best_chunk
            .expect("a summary win must STILL report the best chunk");
        assert!(chunk.passage.contains("five attempts"));
        assert_eq!(chunk.index, target);

        // ...and the two arms' scores must stay distinct, so a caller can see
        // how close they were rather than being handed one blended number.
        assert!((chunk.score - 0.62).abs() < 1e-6, "chunk score must be raw");
        assert!((found.summary_score.unwrap() - 0.90).abs() < 1e-6);
        assert_ne!(found.summary_score.unwrap(), chunk.score);

        // Hybrid blending is not attributable to one representation.
        let found = resolve_provenance(None, Some(note), config);
        assert!(found.match_type.is_none() && found.best_chunk.is_none());
    }

    /// A heading containing the breadcrumb separator must not be split into two
    /// levels. This is why the trail is carried as segments rather than parsed
    /// back out of `"A > B"`.
    #[cfg(has_embeddings)]
    #[test]
    fn heading_path_survives_a_heading_containing_the_separator() {
        use crate::vault::chunker::{ChunkConfig, chunk_note};

        let body = "# Costs > Benefits\n\nsome text under an awkward heading\n";
        let chunks = chunk_note(body, ChunkConfig::default());
        assert_eq!(
            chunks[0].heading_path,
            vec!["Costs > Benefits".to_string()],
            "one heading, even though it contains the separator"
        );
    }

    /// An out-of-range index must degrade, not panic. A note edited after it
    /// was indexed can have fewer chunks than the stored key implies.
    #[cfg(has_embeddings)]
    #[test]
    fn provenance_survives_a_stale_chunk_index() {
        use crate::vault::chunker::ChunkConfig;
        use crate::vault::embeddings::{MatchedOn, NoteMatch};

        let found = resolve_provenance(
            Some(NoteMatch {
                winner: MatchedOn::Chunk(999),
                score: 0.5,
                best_chunk: Some((999, 0.5)),
                summary_score: None,
            }),
            Some("# Short\njust one chunk\n"),
            ChunkConfig::default(),
        );
        assert_eq!(found.match_type, Some("chunk"));
        assert!(
            found.best_chunk.is_none(),
            "a stale index must degrade to no evidence, not panic"
        );
    }
    #[cfg(has_embeddings)]
    use crate::config::EmbeddingProvider;
    use crate::test_helpers::{create_test_vault, extract_text, tantivy_config, test_config};
    #[cfg(unix)]
    use crate::{
        client::semantic_daemon::{DaemonConnectPolicy, SemanticDaemonClient},
        daemon::server::IpcEndpoint,
    };
    #[cfg(unix)]
    use serde_json::json;
    #[cfg(unix)]
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[cfg(unix)]
    fn start_prefetch_capture_server(socket_path: PathBuf) -> tokio::task::JoinHandle<usize> {
        tokio::spawn(async move {
            if socket_path.exists() {
                let _ = std::fs::remove_file(&socket_path);
            }
            let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind unix socket");
            let mut captured_prefetch = 0usize;

            for _ in 0..2 {
                let (stream, _) = listener.accept().await.expect("accept client");
                let (reader, mut writer) = tokio::io::split(stream);
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read request");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("request should be valid JSON");
                let id = request
                    .get("id")
                    .cloned()
                    .expect("request should include id");
                let method = request
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .expect("request should include method");

                let response = match method {
                    "ensure_vault" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "vault_id": "test-vault",
                            "ready": true,
                            "watch_enabled": true,
                            "model_name": "BAAI/bge-small-en-v1.5"
                        }
                    }),
                    "search_hybrid" => {
                        captured_prefetch = request
                            .get("params")
                            .and_then(|params| params.get("prefetch"))
                            .and_then(serde_json::Value::as_u64)
                            .expect("search_hybrid should include prefetch")
                            as usize;
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "results": []
                            }
                        })
                    }
                    other => panic!("unexpected method in daemon test server: {other}"),
                };

                writer
                    .write_all(
                        format!(
                            "{}\n",
                            serde_json::to_string(&response).expect("serialize response")
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write response");
                writer.flush().await.expect("flush response");
            }

            captured_prefetch
        })
    }

    #[cfg(unix)]
    fn start_semantic_filter_server(
        socket_path: PathBuf,
        filtered_path: &'static str,
    ) -> tokio::task::JoinHandle<usize> {
        tokio::spawn(async move {
            if socket_path.exists() {
                let _ = std::fs::remove_file(&socket_path);
            }
            let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind unix socket");
            let mut captured_top_k = 0usize;

            for _ in 0..2 {
                let (stream, _) = listener.accept().await.expect("accept client");
                let (reader, mut writer) = tokio::io::split(stream);
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read request");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("request should be valid JSON");
                let id = request
                    .get("id")
                    .cloned()
                    .expect("request should include id");
                let method = request
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .expect("request should include method");

                let response = match method {
                    "ensure_vault" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "vault_id": "test-vault",
                            "ready": true,
                            "watch_enabled": true,
                            "model_name": "BAAI/bge-small-en-v1.5"
                        }
                    }),
                    "search_semantic" => {
                        captured_top_k = request
                            .get("params")
                            .and_then(|params| params.get("top_k"))
                            .and_then(serde_json::Value::as_u64)
                            .expect("search_semantic should include top_k")
                            as usize;
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "results": [
                                    {
                                        "path": filtered_path,
                                        "title": "Filtered",
                                        "score": 1.0,
                                        "tags": [],
                                        "snippet": "filtered",
                                        "content": null,
                                        "subpath": null
                                    },
                                    {
                                        "path": "rust.md",
                                        "title": "Rust",
                                        "score": 0.9,
                                        "tags": ["lang", "systems"],
                                        "snippet": "Rust is a systems language.",
                                        "content": null,
                                        "subpath": null
                                    }
                                ]
                            }
                        })
                    }
                    other => panic!("unexpected method in daemon test server: {other}"),
                };

                writer
                    .write_all(
                        format!(
                            "{}\n",
                            serde_json::to_string(&response).expect("serialize response")
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write response");
                writer.flush().await.expect("flush response");
            }

            captured_top_k
        })
    }

    #[cfg(all(unix, has_embeddings))]
    fn start_semantic_not_ready_server(socket_path: PathBuf) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if socket_path.exists() {
                let _ = std::fs::remove_file(&socket_path);
            }
            let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind unix socket");

            for _ in 0..2 {
                let (stream, _) = listener.accept().await.expect("accept client");
                let (reader, mut writer) = tokio::io::split(stream);
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read request");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("request should be valid JSON");
                let id = request
                    .get("id")
                    .cloned()
                    .expect("request should include id");
                let method = request
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .expect("request should include method");

                let response = match method {
                    "ensure_vault" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "vault_id": "warming-vault",
                            "ready": false,
                            "watch_enabled": true,
                            "model_name": "test-model",
                            "phase": "warming",
                            "indexed_notes": 1,
                            "total_notes": 4,
                            "pending_notes": 3
                        }
                    }),
                    "search_semantic" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": protocol::ERR_VAULT_NOT_READY,
                            "message": "semantic index is warming; retry the query shortly",
                            "data": {
                                "phase": "warming",
                                "ready": false,
                                "indexed_notes": 1,
                                "total_notes": 4,
                                "pending_notes": 3
                            }
                        }
                    }),
                    other => panic!("unexpected method in daemon test server: {other}"),
                };

                writer
                    .write_all(
                        format!(
                            "{}\n",
                            serde_json::to_string(&response).expect("serialize response")
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write response");
                writer.flush().await.expect("flush response");
            }
        })
    }

    async fn setup_search_vault() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());

        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        vault
            .write_note(
                Path::new("rust.md"),
                "---\ntags: [lang, systems]\nstatus: stable\n---\n# Rust\nRust is a systems language.\n",
            )
            .unwrap();
        vault
            .write_note(
                Path::new("python.md"),
                "---\ntags: [lang, scripting]\nstatus: in progress\n---\n# Python\nPython is dynamic.\n",
            )
            .unwrap();
        vault
            .write_note(
                Path::new("notes.md"),
                "# Notes\nSome random notes about #inbox stuff.\n\n#inbox/read #inbox/todo\n",
            )
            .unwrap();
        vault
            .write_note(
                Path::new("empty.md"),
                "# Empty\nNothing interesting here.\n",
            )
            .unwrap();

        (dir, vault)
    }

    async fn setup_excluded_search_vault() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        std::fs::create_dir_all(dir.path().join("Archive")).unwrap();
        std::fs::write(
            dir.path().join("Archive/hidden.md"),
            "# Hidden\nThis excluded note should not be visible.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("rust.md"),
            "---\ntags: [lang, systems]\n---\n# Rust\nRust is a systems language.\n",
        )
        .unwrap();

        let mut config = test_config(dir.path());
        config.exclude_patterns = vec!["Archive/".into()];
        let vault = Vault::open(&config).await.unwrap();

        (dir, vault)
    }

    // ── search_text ─────────────────────────────────────────────────

    #[tokio::test]
    async fn search_text_finds_match() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_text(
            &vault,
            SearchTextParams {
                query: "systems".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("rust.md"));
        assert!(!text.contains("python.md"));
    }

    #[tokio::test]
    async fn search_text_limits_results() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_text(
            &vault,
            SearchTextParams {
                query: "is".into(),
                max_results: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[tokio::test]
    async fn search_text_empty_query_returns_empty() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_text(
            &vault,
            SearchTextParams {
                query: String::new(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert!(parsed.is_empty());
    }

    // ── search_regex ────────────────────────────────────────────────

    #[tokio::test]
    async fn search_regex_valid_pattern() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_regex(
            &vault,
            SearchRegexParams {
                pattern: r"(?i)python".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("python.md"));
    }

    #[tokio::test]
    async fn search_regex_invalid_pattern_returns_error() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_regex(
            &vault,
            SearchRegexParams {
                pattern: "[invalid".into(),
                ..Default::default()
            },
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn search_regex_limits_results() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_regex(
            &vault,
            SearchRegexParams {
                pattern: r"\w+".into(),
                max_results: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert!(parsed.len() <= 2);
    }

    // ── search_metadata (tag) ──────────────────────────────────────

    #[tokio::test]
    async fn search_metadata_tag_exact() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "tag".into(),
                tag: Some("inbox".into()),
                include_nested: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("notes.md"));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[tokio::test]
    async fn search_metadata_tag_include_nested() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "tag".into(),
                tag: Some("inbox".into()),
                include_nested: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("notes.md"));
    }

    #[tokio::test]
    async fn search_metadata_tag_strips_hash_prefix() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "tag".into(),
                tag: Some("#lang".into()),
                include_nested: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    #[tokio::test]
    async fn search_metadata_tag_missing_tag_errors() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "tag".into(),
                ..Default::default()
            },
        )
        .await;

        assert!(result.is_err());
    }

    // ── search_metadata (frontmatter) ───────────────────────────────

    #[tokio::test]
    async fn search_metadata_frontmatter_eq() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "frontmatter".into(),
                field: Some("status".into()),
                value: Some(serde_json::json!("stable")),
                operator: Some(FrontmatterOperator::Eq),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("rust.md"));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[tokio::test]
    async fn search_metadata_frontmatter_eq_array_contains() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "frontmatter".into(),
                field: Some("tags".into()),
                value: Some(serde_json::json!("systems")),
                operator: Some(FrontmatterOperator::Eq),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("rust.md"));
    }

    #[tokio::test]
    async fn search_metadata_frontmatter_contains_substring() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "frontmatter".into(),
                field: Some("status".into()),
                value: Some(serde_json::json!("progress")),
                operator: Some(FrontmatterOperator::Contains),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("python.md"));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[tokio::test]
    async fn search_metadata_frontmatter_exists() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "frontmatter".into(),
                field: Some("status".into()),
                operator: Some(FrontmatterOperator::Exists),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.len(), 2); // rust.md + python.md
    }

    #[tokio::test]
    async fn search_metadata_frontmatter_exists_missing_field() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "frontmatter".into(),
                field: Some("nonexistent".into()),
                operator: Some(FrontmatterOperator::Exists),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        assert!(parsed.is_empty());
    }

    #[tokio::test]
    async fn search_metadata_frontmatter_eq_without_value_errors() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "frontmatter".into(),
                field: Some("status".into()),
                operator: Some(FrontmatterOperator::Eq),
                ..Default::default()
            },
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn search_metadata_preserves_and_matches_explicit_null_value() {
        let missing: SearchMetadataParams = serde_json::from_value(serde_json::json!({
            "type": "frontmatter",
            "field": "status"
        }))
        .unwrap();
        assert!(missing.value.is_none());

        let explicit_null: SearchMetadataParams = serde_json::from_value(serde_json::json!({
            "type": "frontmatter",
            "field": "status",
            "value": null
        }))
        .unwrap();
        assert_eq!(explicit_null.value, Some(serde_json::Value::Null));

        let (_dir, vault) = setup_search_vault().await;
        vault
            .set_frontmatter_field(Path::new("rust.md"), "reviewed_at", serde_json::Value::Null)
            .unwrap();
        let params: SearchMetadataParams = serde_json::from_value(serde_json::json!({
            "type": "frontmatter",
            "field": "reviewed_at",
            "operator": "eq",
            "value": null
        }))
        .unwrap();
        let result = search_metadata(&vault, params).await.unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(extract_text(&result)).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["path"], "rust.md");
    }

    #[tokio::test]
    async fn search_metadata_frontmatter_missing_field_errors() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "frontmatter".into(),
                value: Some(serde_json::json!("test")),
                ..Default::default()
            },
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn search_metadata_invalid_type_errors() {
        let (_dir, vault) = setup_search_vault().await;
        let result = search_metadata(
            &vault,
            SearchMetadataParams {
                search_type: "invalid".into(),
                ..Default::default()
            },
        )
        .await;

        assert!(result.is_err());
    }

    // ── search_text with Tantivy BM25 ──────────────────────────────

    async fn setup_tantivy_vault() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());

        let vault = Vault::open(&tantivy_config(dir.path())).await.unwrap();

        vault
            .write_note(
                Path::new("rust.md"),
                "---\ntags: [lang, systems]\nstatus: stable\n---\n# Rust\nRust is a systems programming language.\n",
            )
            .unwrap();
        vault
            .write_note(
                Path::new("python.md"),
                "---\ntags: [lang, scripting]\nstatus: in progress\n---\n# Python\nPython is a dynamic scripting language.\n",
            )
            .unwrap();
        vault
            .write_note(
                Path::new("cooking.md"),
                "# Cooking Tips\nHow to make a great pasta dish.\n",
            )
            .unwrap();

        (dir, vault)
    }

    #[tokio::test]
    async fn search_text_tantivy_returns_scores() {
        let (_dir, vault) = setup_tantivy_vault().await;
        let result = search_text(
            &vault,
            SearchTextParams {
                query: "systems".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();

        assert!(!parsed.is_empty());
        assert!(text.contains("rust.md"));
        // BM25 results should have a score
        assert!(parsed[0].get("score").is_some());
        assert!(parsed[0]["score"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn search_text_tantivy_ranked_descending() {
        let (_dir, vault) = setup_tantivy_vault().await;
        let result = search_text(
            &vault,
            SearchTextParams {
                query: "language".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();

        if parsed.len() >= 2 {
            let s0 = parsed[0]["score"].as_f64().unwrap();
            let s1 = parsed[1]["score"].as_f64().unwrap();
            assert!(s0 >= s1, "results should be sorted by score descending");
        }
    }

    #[tokio::test]
    async fn search_text_tantivy_fuzzy() {
        let (_dir, vault) = setup_tantivy_vault().await;
        let result = search_text(
            &vault,
            SearchTextParams {
                query: "pyhton".into(),
                fuzzy: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("python.md"),
            "fuzzy should match 'pyhton' to 'python'"
        );
    }

    #[tokio::test]
    async fn search_text_tantivy_field_filter() {
        let (_dir, vault) = setup_tantivy_vault().await;
        let result = search_text(
            &vault,
            SearchTextParams {
                query: "cooking".into(),
                fields: Some(vec![SearchField::Title]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("cooking.md"),
            "title search for 'cooking' should find cooking.md"
        );
    }

    #[tokio::test]
    async fn search_text_tantivy_context_snippets() {
        let (_dir, vault) = setup_tantivy_vault().await;
        let result = search_text(
            &vault,
            SearchTextParams {
                query: "pasta".into(),
                context_length: Some(50),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = extract_text(&result);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();

        assert!(!parsed.is_empty());
        let matches = parsed[0]["matches"].as_array().unwrap();
        assert!(!matches.is_empty(), "should have context matches");
        assert!(matches[0]["context"].as_str().unwrap().contains("pasta"));
    }

    // ── SearchSemanticParams ────────────────────────────────────────

    #[test]
    fn semantic_params_defaults() {
        let params: SearchSemanticParams = serde_json::from_str(r#"{"query": "test"}"#).unwrap();
        assert_eq!(params.query, "test");
        assert!(params.alpha.is_none());
        assert!(params.lexical_prefetch.is_none());
        assert!(params.top_k.is_none());
    }

    #[test]
    fn semantic_params_with_alpha() {
        let params: SearchSemanticParams =
            serde_json::from_str(r#"{"query": "q", "alpha": 0.7, "lexical_prefetch": true}"#)
                .unwrap();
        assert!((params.alpha.unwrap() - 0.7).abs() < f32::EPSILON);
        assert_eq!(params.lexical_prefetch, Some(true));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_prefetch_overfetches_without_forcing_min_50() {
        let (_dir, vault) = setup_search_vault().await;
        let socket_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = socket_dir.path().join("semanticd.sock");
        let server = start_prefetch_capture_server(socket_path.clone());

        let runtime = SemanticRuntime {
            mode: SemanticMode::Daemon,
            daemon_client: Some(SemanticDaemonClient::new(
                IpcEndpoint::UnixSocket(socket_path),
                DaemonConnectPolicy::default(),
            )),
            daemon_unavailable_reason: None,
            prefetch_count: 7,
            vault_ensured: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let result = search_semantic(
            &vault,
            SearchSemanticParams {
                query: "systems language".to_string(),
                top_k: Some(5),
                include_content: Some(false),
                lexical_prefetch: Some(true),
                alpha: Some(0.25),
            },
            0.25,
            &runtime,
        )
        .await
        .expect("daemon search should succeed");
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(extract_text(&result)).expect("parse result");
        assert!(parsed.is_empty(), "mock daemon returns empty result set");

        let captured_prefetch = server.await.expect("server join");
        assert_eq!(
            captured_prefetch,
            semantic_candidate_limit(5),
            "runtime prefetch may grow to cover the filtered candidate window"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_semantic_overfetches_before_filtering_hidden_results() {
        let (_dir, vault) = setup_search_vault().await;
        let socket_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = socket_dir.path().join("semanticd.sock");
        let server = start_semantic_filter_server(socket_path.clone(), "missing-hidden.md");

        let runtime = SemanticRuntime {
            mode: SemanticMode::Daemon,
            daemon_client: Some(SemanticDaemonClient::new(
                IpcEndpoint::UnixSocket(socket_path),
                DaemonConnectPolicy::default(),
            )),
            daemon_unavailable_reason: None,
            prefetch_count: 50,
            vault_ensured: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let result = search_semantic(
            &vault,
            SearchSemanticParams {
                query: "systems language".to_string(),
                top_k: Some(1),
                include_content: Some(false),
                lexical_prefetch: Some(false),
                alpha: None,
            },
            0.25,
            &runtime,
        )
        .await
        .expect("daemon search should succeed");
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(extract_text(&result)).expect("parse result");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["path"], "rust.md");

        let captured_top_k = server.await.expect("server join");
        assert_eq!(captured_top_k, semantic_candidate_limit(1));
        assert!(
            captured_top_k > 1,
            "daemon request should over-fetch before MCP-side visibility filtering"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_semantic_filters_excluded_hits_after_overfetching() {
        let (_dir, vault) = setup_excluded_search_vault().await;
        let socket_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = socket_dir.path().join("semanticd.sock");
        let server = start_semantic_filter_server(socket_path.clone(), "Archive/hidden.md");

        let runtime = SemanticRuntime {
            mode: SemanticMode::Daemon,
            daemon_client: Some(SemanticDaemonClient::new(
                IpcEndpoint::UnixSocket(socket_path),
                DaemonConnectPolicy::default(),
            )),
            daemon_unavailable_reason: None,
            prefetch_count: 50,
            vault_ensured: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let result = search_semantic(
            &vault,
            SearchSemanticParams {
                query: "systems language".to_string(),
                top_k: Some(1),
                include_content: Some(false),
                lexical_prefetch: Some(false),
                alpha: None,
            },
            0.25,
            &runtime,
        )
        .await
        .expect("daemon search should succeed");
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(extract_text(&result)).expect("parse result");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["path"], "rust.md");

        let captured_top_k = server.await.expect("server join");
        assert_eq!(captured_top_k, semantic_candidate_limit(1));
    }

    #[cfg(all(unix, has_embeddings))]
    #[tokio::test]
    async fn auto_mode_preserves_daemon_warming_error_without_local_fallback() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let mut config = test_config(dir.path());
        config.embeddings = true;
        config.embedding_provider = Some(EmbeddingProvider::Local);
        config.embeddings_model = "definitely-not-a-real-local-model".into();
        let vault = Vault::open(&config).await.unwrap();

        let socket_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = socket_dir.path().join("semanticd.sock");
        let server = start_semantic_not_ready_server(socket_path.clone());
        let runtime = SemanticRuntime {
            mode: SemanticMode::Auto,
            daemon_client: Some(SemanticDaemonClient::new(
                IpcEndpoint::UnixSocket(socket_path),
                DaemonConnectPolicy::default(),
            )),
            daemon_unavailable_reason: None,
            prefetch_count: 50,
            vault_ensured: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let error = search_semantic(
            &vault,
            SearchSemanticParams {
                query: "semantic".into(),
                top_k: Some(5),
                include_content: Some(false),
                lexical_prefetch: Some(false),
                alpha: None,
            },
            0.25,
            &runtime,
        )
        .await
        .expect_err("warming daemon must return an explicit MCP error");

        assert_eq!(error.code, ErrorCode::INVALID_REQUEST);
        assert_eq!(
            error.message,
            "semantic index is warming; retry the query shortly"
        );
        let data = error.data.expect("warming status data should reach MCP");
        assert_eq!(data["phase"], "warming");
        assert_eq!(data["ready"], false);
        assert!(
            runtime
                .vault_ensured
                .load(std::sync::atomic::Ordering::Relaxed),
            "successful attachment must remain cached even while semantic warm-up continues"
        );
        server.await.expect("server join");
    }

    #[cfg(has_embeddings)]
    #[tokio::test]
    async fn auto_mode_still_falls_back_for_actual_daemon_unavailability() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        let mut config = test_config(dir.path());
        config.embeddings = true;
        config.embedding_provider = Some(EmbeddingProvider::Local);
        config.embeddings_model = "definitely-not-a-real-local-model".into();
        let vault = Vault::open(&config).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while vault.embedding_load_error().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("invalid local backend should fail deterministically");

        let ensured = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let runtime = SemanticRuntime {
            mode: SemanticMode::Auto,
            daemon_client: None,
            daemon_unavailable_reason: Some("semantic daemon is offline".into()),
            prefetch_count: 50,
            vault_ensured: std::sync::Arc::clone(&ensured),
        };
        let error = search_semantic(
            &vault,
            SearchSemanticParams {
                query: "semantic".into(),
                top_k: Some(5),
                include_content: Some(false),
                lexical_prefetch: Some(false),
                alpha: None,
            },
            0.25,
            &runtime,
        )
        .await
        .expect_err("local backend failure should surface after daemon fallback");

        assert!(
            !error.message.contains("semantic daemon is offline"),
            "the local backend should have been attempted"
        );
        assert!(
            !ensured.load(std::sync::atomic::Ordering::Relaxed),
            "transport failure should invalidate the cached attachment"
        );
    }
}
