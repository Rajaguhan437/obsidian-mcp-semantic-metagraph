//! `note_related` — what else in the vault is about this note.
//!
//! Two different questions get answered together, because the interesting
//! answer is where they disagree:
//!
//! - **semantically related** — nearest notes by embedding, seeded from this
//!   note's own stored vector. No query string, no embedding call.
//! - **linked** — what this note already points at, and what points back.
//!
//! A note that scores high and is *not* linked is a connection the vault has
//! not recorded yet. That is the signal worth surfacing, and it only exists
//! because both sets are returned rather than one.
//!
//! This module composes the two layers; it does not couple them. The graph code
//! still knows nothing about embeddings and vice versa — both are reached
//! through `Vault`.

use rmcp::handler::server::wrapper::Json;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::VaultError;
use crate::vault::Vault;

// Only the has_embeddings implementation reads these.
#[cfg(has_embeddings)]
const DEFAULT_TOP_K: usize = 10;
#[cfg(has_embeddings)]
const MAX_TOP_K: usize = 50;

#[derive(Deserialize, JsonSchema, Default)]
pub struct NoteRelatedParams {
    /// Vault-relative path of the note to find relatives of.
    pub path: String,
    /// Maximum semantically related notes to return (default: 10, max: 50).
    #[serde(default)]
    pub top_k: Option<usize>,
    /// Include the matched passage and heading path for each result
    /// (default: true). Set false for a compact list of paths and scores.
    #[serde(default)]
    pub include_passages: Option<bool>,
}

#[derive(serde::Serialize, JsonSchema)]
pub struct RelatedNote {
    path: std::path::PathBuf,
    title: String,
    /// Same ranking score `search_semantic` reports. Not a cosine: a weighted
    /// summary match can exceed 1.0.
    score: f32,
    /// True if this note is already linked from or to the subject note. False
    /// is the interesting case — related, but the vault does not say so.
    linked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    best_chunk: Option<super::search::MatchedChunk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_score: Option<f32>,
}

#[derive(serde::Serialize, JsonSchema)]
pub struct LinkedNeighbours {
    /// Notes this note links to, resolved.
    outgoing: Vec<std::path::PathBuf>,
    /// Notes that link to this note.
    backlinks: Vec<std::path::PathBuf>,
}

#[derive(serde::Serialize, JsonSchema)]
pub struct NoteRelatedResult {
    path: std::path::PathBuf,
    title: String,
    /// Semantically nearest notes, most similar first.
    related: Vec<RelatedNote>,
    /// What the link graph already records, for comparison.
    linked: LinkedNeighbours,
    /// How many of `related` are not yet linked either way — candidate
    /// connections the vault has not recorded.
    unlinked_related: usize,
}

pub async fn note_related(
    vault: &Vault,
    params: NoteRelatedParams,
) -> Result<Json<NoteRelatedResult>, rmcp::ErrorData> {
    // See the note in `search::search_semantic`: returning `Json` publishes an
    // output schema, which is what makes `linked` and the provenance fields
    // legible to a client instead of arriving as undocumented JSON text.
    Ok(Json(note_related_inner(vault, params)?))
}

#[cfg(has_embeddings)]
fn note_related_inner(
    vault: &Vault,
    params: NoteRelatedParams,
) -> Result<NoteRelatedResult, VaultError> {
    use std::collections::HashSet;

    let path = std::path::PathBuf::from(&params.path);
    let meta = vault.get_note_metadata(&path)?;
    let top_k = params.top_k.unwrap_or(DEFAULT_TOP_K).clamp(1, MAX_TOP_K);
    let include_passages = params.include_passages.unwrap_or(true);

    // The link graph first: cheap, and it is what marks each result.
    // `WikiLink` carries the raw target; resolving it to a path is the vault's
    // job. Unresolved targets are broken links and simply have no neighbour.
    let outgoing: Vec<std::path::PathBuf> = vault
        .outgoing_links(&path)?
        .into_iter()
        .filter_map(|link| vault.resolve_link(&link.target))
        .collect();
    let backlinks: Vec<std::path::PathBuf> = vault
        .backlinks(&path)?
        .into_iter()
        .map(|note| note.path)
        .collect();
    let linked_set: HashSet<&std::path::Path> = outgoing
        .iter()
        .chain(backlinks.iter())
        .map(|p| p.as_path())
        .collect();

    let chunk_config = crate::vault::chunker::ChunkConfig::from_env();
    let mut related = Vec::new();
    let mut unlinked_related = 0usize;
    for (candidate, score, matched) in vault.related_notes(&path, top_k)? {
        let Ok(candidate_meta) = vault.get_note_metadata(&candidate) else {
            continue;
        };
        let linked = linked_set.contains(candidate.as_path());
        if !linked {
            unlinked_related += 1;
        }
        let provenance = if include_passages {
            let text = vault.read_note(&candidate).ok();
            super::search::resolve_provenance(matched, text.as_deref(), chunk_config)
        } else {
            super::search::resolve_provenance(matched, None, chunk_config)
        };
        related.push(RelatedNote {
            path: candidate,
            title: candidate_meta.title,
            score,
            linked,
            match_type: provenance.match_type,
            best_chunk: provenance.best_chunk,
            summary_score: provenance.summary_score,
        });
    }

    Ok(NoteRelatedResult {
        path,
        title: meta.title,
        related,
        linked: LinkedNeighbours {
            outgoing,
            backlinks,
        },
        unlinked_related,
    })
}

#[cfg(not(has_embeddings))]
fn note_related_inner(
    _vault: &Vault,
    _params: NoteRelatedParams,
) -> Result<NoteRelatedResult, VaultError> {
    Err(VaultError::Embedding(
        "note_related needs semantic search: build with --features embeddings or \
         embeddings-api, and set OBSIDIAN_EMBEDDINGS=true"
            .into(),
    ))
}
