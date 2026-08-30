//! Daemon query handlers: vault attach, semantic search, and hybrid search.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use super::protocol::{
    self, EnsureVaultParams, EnsureVaultResult, OpenHintParams, OpenHintResult, SearchHybridParams,
    SearchResult, SearchSemanticParams, SemanticHit, SemanticPhase, SemanticStatus,
};
use super::vault_context::VaultContext;
use super::vault_registry::VaultRegistry;
use crate::error::VaultError;
use crate::vault::search_utils::{body_preview, compile_query_word_regex};

const DEFAULT_TOP_K: usize = 10;
const DEFAULT_PREFETCH_COUNT: usize = 50;
const DEFAULT_ALPHA: f32 = 0.25;
const SNIPPET_CONTEXT_LEN: usize = 150;
const SNIPPET_FALLBACK_CHARS: usize = 300;

#[derive(Debug)]
pub struct QueryError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl QueryError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

pub type QueryResult<T> = Result<T, QueryError>;

pub async fn ensure_vault(
    registry: &VaultRegistry,
    params: EnsureVaultParams,
) -> QueryResult<EnsureVaultResult> {
    let watch_enabled = params.watch.unwrap_or(true);
    let requested_model = params
        .model_name
        .as_deref()
        .unwrap_or(registry.model_name());
    let context = registry
        .ensure_vault(
            Path::new(&params.vault_root),
            watch_enabled,
            requested_model,
        )
        .await
        .map_err(map_vault_error)?;

    let watch_enabled = context.watch_enabled().map_err(map_vault_error)?;
    let status = semantic_status(&context);
    Ok(EnsureVaultResult {
        vault_id: context.vault_id().to_string(),
        ready: status.ready,
        watch_enabled,
        model_name: context.model_name().to_string(),
        phase: Some(status.phase),
        indexed_notes: Some(status.indexed_notes),
        total_notes: Some(status.total_notes),
        pending_notes: Some(status.pending_notes),
        last_error: status.last_error,
    })
}

pub async fn search_semantic(
    registry: &VaultRegistry,
    params: SearchSemanticParams,
) -> QueryResult<SearchResult> {
    let context = require_context(registry, &params.vault_root).await?;
    require_semantic_ready(&context)?;
    let top_k = params.top_k.unwrap_or(DEFAULT_TOP_K);
    let include_content = params.include_content.unwrap_or(false);

    // With embeddings compiled in, ask for the detailed form so each hit can
    // say which representation ranked it. Without them `require_semantic_ready`
    // has already returned an error above; the branch exists only to compile.
    #[cfg(has_embeddings)]
    {
        let hits = context
            .search_semantic_hits(&params.query, top_k)
            .map_err(map_vault_error)?;

        let matches: std::collections::HashMap<PathBuf, _> = hits
            .iter()
            .map(|(path, matched)| (path.clone(), *matched))
            .collect();
        let scores: Vec<(PathBuf, f32)> = hits
            .into_iter()
            .map(|(path, matched)| (path, matched.score))
            .collect();

        let chunk_config = crate::vault::chunker::ChunkConfig::from_env();
        build_hits(
            &context,
            scores,
            &params.query,
            include_content,
            |path, text| resolve_hit_provenance(matches.get(path).copied(), text, chunk_config),
        )
    }

    #[cfg(not(has_embeddings))]
    {
        let scores = context
            .search_semantic_scores(&params.query, top_k)
            .map_err(map_vault_error)?;
        build_hits(&context, scores, &params.query, include_content, |_, _| {
            HitProvenance::default()
        })
    }
}

/// Map the store's `NoteMatch` onto the wire fields.
///
/// Reuses `tools::search::resolve_provenance` so the daemon and the in-process
/// path cannot drift on what counts as a chunk win, which passage is reported,
/// or how a summary win is labelled.
#[cfg(has_embeddings)]
fn resolve_hit_provenance(
    matched: Option<crate::vault::embeddings::NoteMatch>,
    text: Option<&str>,
    chunk_config: crate::vault::chunker::ChunkConfig,
) -> HitProvenance {
    let provenance = crate::tools::search::resolve_provenance(matched, text, chunk_config);

    HitProvenance {
        match_type: provenance.match_type.map(str::to_string),
        best_chunk: provenance.best_chunk.map(|chunk| protocol::SemanticChunk {
            index: chunk.index,
            heading_path: chunk.heading_path,
            passage: chunk.passage,
            score: chunk.score,
        }),
        summary_score: provenance.summary_score,
    }
}

pub async fn search_hybrid(
    registry: &VaultRegistry,
    params: SearchHybridParams,
) -> QueryResult<SearchResult> {
    let context = require_context(registry, &params.vault_root).await?;
    if params.query.is_empty() {
        return Ok(SearchResult {
            results: Vec::new(),
        });
    }
    require_semantic_ready(&context)?;

    let top_k = params.top_k.unwrap_or(DEFAULT_TOP_K);
    let include_content = params.include_content.unwrap_or(false);
    let prefetch = params.prefetch.unwrap_or(DEFAULT_PREFETCH_COUNT).max(top_k);
    let alpha = params.alpha.unwrap_or(DEFAULT_ALPHA).clamp(0.0, 1.0);

    let bm25_hits = context
        .search_bm25(&params.query, prefetch)
        .map_err(map_vault_error)?;
    if bm25_hits.is_empty() {
        return Ok(SearchResult {
            results: Vec::new(),
        });
    }

    let combined = context
        .search_hybrid_scores(&params.query, &bm25_hits, alpha, top_k)
        .map_err(map_vault_error)?;

    // No provenance on the hybrid path: a blended rank is not attributable to
    // one representation, so the fields are omitted rather than guessed.
    build_hits(
        &context,
        combined,
        &params.query,
        include_content,
        |_, _| HitProvenance::default(),
    )
}

pub async fn open_hint(
    registry: &VaultRegistry,
    params: OpenHintParams,
) -> QueryResult<OpenHintResult> {
    let context = require_context(registry, &params.vault_root).await?;
    let (path_part, subpath) = split_subpath(&params.path);
    let relative = Path::new(path_part);
    if relative.is_absolute() {
        return Err(QueryError::new(
            protocol::ERR_INVALID_PARAMS,
            "path must be vault-relative",
        ));
    }

    let canonical_path = match context.canonical_existing_relative_path(relative) {
        Ok(path) => match context.read_note(&path) {
            Ok(_) => Some(path),
            Err(err) => return Err(map_vault_error(err)),
        },
        Err(VaultError::NoteNotFound(_)) => None,
        Err(err) => return Err(map_vault_error(err)),
    };
    let exists = canonical_path.is_some();
    let path = match canonical_path {
        Some(path) => path.to_string_lossy().into_owned(),
        None => path_part.to_string(),
    };
    Ok(OpenHintResult {
        path,
        exists,
        subpath,
    })
}

async fn require_context(
    registry: &VaultRegistry,
    vault_root: &str,
) -> QueryResult<Arc<VaultContext>> {
    match registry
        .get_context_by_root(Path::new(vault_root))
        .await
        .map_err(map_vault_error)?
    {
        Some(context) => Ok(context),
        None => Err(QueryError::new(
            protocol::ERR_VAULT_NOT_READY,
            "vault not ready; call ensure_vault first",
        )),
    }
}

/// The provenance fields of one hit, as they go on the wire.
#[derive(Default)]
struct HitProvenance {
    match_type: Option<String>,
    best_chunk: Option<protocol::SemanticChunk>,
    summary_score: Option<f32>,
}

/// Resolve provenance for a note, given its text.
///
/// Taken as a callback rather than a flag so the hybrid path — where a blended
/// rank is not attributable to one representation — and a daemon built without
/// embeddings can both pass a resolver that yields nothing, without this
/// function needing to know why.
fn build_hits<F>(
    context: &VaultContext,
    scores: Vec<(PathBuf, f32)>,
    query: &str,
    include_content: bool,
    mut provenance_for: F,
) -> QueryResult<SearchResult>
where
    F: FnMut(&Path, Option<&str>) -> HitProvenance,
{
    let word_re = if include_content {
        None
    } else {
        compile_query_word_regex(query)
    };

    let mut results = Vec::with_capacity(scores.len());
    for (path, score) in scores {
        let Some(meta) = context.note_metadata(&path).map_err(map_vault_error)? else {
            continue;
        };
        let title = meta.title;
        let tags = meta.tags;

        // Read once: the passage lookup needs the text whether or not the
        // caller asked for content.
        let text = context.read_note(&path).ok();
        let provenance = provenance_for(&path, text.as_deref());

        let (content, snippet) = if include_content {
            (text, None)
        } else {
            let snippet = text.as_deref().map(|text| {
                let body = crate::vault::frontmatter::get_body(text);
                if let Some(re) = word_re.as_ref()
                    && let Some(matched) = re.find(body)
                {
                    let (context_text, _, _, _) = crate::vault::index::extract_match_context(
                        body,
                        matched.start(),
                        matched.end(),
                        SNIPPET_CONTEXT_LEN,
                    );
                    return context_text;
                }
                body_preview(text, SNIPPET_FALLBACK_CHARS)
            });
            (None, snippet)
        };

        results.push(SemanticHit {
            path: path.to_string_lossy().to_string(),
            title,
            score,
            tags,
            snippet,
            content,
            subpath: None,
            match_type: provenance.match_type,
            best_chunk: provenance.best_chunk,
            summary_score: provenance.summary_score,
        });
    }

    Ok(SearchResult { results })
}

fn semantic_status(context: &VaultContext) -> SemanticStatus {
    #[cfg(has_embeddings)]
    {
        let status = context.embedding_status();
        SemanticStatus {
            phase: match status.phase {
                crate::vault::embedding_runtime::EmbeddingPhase::Warming => SemanticPhase::Warming,
                crate::vault::embedding_runtime::EmbeddingPhase::Ready => SemanticPhase::Ready,
                crate::vault::embedding_runtime::EmbeddingPhase::Degraded => {
                    SemanticPhase::Degraded
                }
            },
            ready: status.queryable,
            indexed_notes: status.indexed_notes,
            total_notes: status.total_notes,
            pending_notes: status.pending_notes,
            last_error: status.last_error,
        }
    }
    #[cfg(not(has_embeddings))]
    {
        let _ = context;
        SemanticStatus {
            phase: SemanticPhase::Degraded,
            ready: false,
            indexed_notes: 0,
            total_notes: 0,
            pending_notes: 0,
            last_error: Some("daemon binary was compiled without embedding support".into()),
        }
    }
}

fn require_semantic_ready(context: &VaultContext) -> QueryResult<()> {
    let status = semantic_status(context);
    if status.ready {
        return Ok(());
    }

    #[cfg(not(has_embeddings))]
    let (code, message) = (
        protocol::ERR_BOOTSTRAP_REQUIRED,
        "semantic daemon has no embedding backend; use a daemon built with embedding support",
    );
    #[cfg(has_embeddings)]
    let (code, message) = match status.phase {
        SemanticPhase::Warming => (
            protocol::ERR_VAULT_NOT_READY,
            "semantic index is warming; retry the query shortly",
        ),
        SemanticPhase::Degraded => (
            protocol::ERR_VAULT_NOT_READY,
            "semantic index is degraded and unavailable; inspect status and retry after recovery",
        ),
        SemanticPhase::Ready => (
            protocol::ERR_VAULT_NOT_READY,
            "semantic index is not queryable yet; retry the query shortly",
        ),
    };

    let data = serde_json::to_value(&status).unwrap_or_else(|_| serde_json::json!({}));
    Err(QueryError {
        code,
        message: message.into(),
        data: Some(data),
    })
}

fn split_subpath(path: &str) -> (&str, Option<String>) {
    if let Some((base, subpath)) = path.split_once('#') {
        if subpath.is_empty() {
            (base, None)
        } else {
            (base, Some(subpath.to_string()))
        }
    } else {
        (path, None)
    }
}

fn map_vault_error(err: VaultError) -> QueryError {
    let message = err.to_string();
    match err {
        VaultError::InvalidPath(_)
        | VaultError::OutsideVault(_)
        | VaultError::AlreadyExists(_)
        | VaultError::PatchTargetNotFound { .. }
        | VaultError::InvalidRegex { .. } => QueryError::new(protocol::ERR_INVALID_PARAMS, message),
        VaultError::Embedding(_) => QueryError::new(protocol::ERR_BOOTSTRAP_REQUIRED, message),
        _ => QueryError::new(protocol::ERR_INTERNAL, message),
    }
}
