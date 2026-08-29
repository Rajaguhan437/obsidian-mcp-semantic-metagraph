//! Embedding store and model wrapper for semantic search (Layer 2).
//!
//! Gated behind `#[cfg(has_embeddings)]` (either `embeddings` or `embeddings-api`
//! Cargo feature). Provides:
//! - `EmbeddingStore`: in-memory HashMap of note embeddings with brute-force
//!   cosine similarity search and bincode persistence.
//! - `EmbeddingModel`: backend-agnostic wrapper supporting local fastembed
//!   (`--features embeddings`) and OpenAI-compatible API (`--features embeddings-api`).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

#[cfg(feature = "embeddings-api")]
use std::sync::Arc;

use crate::config::EmbeddingProvider;
use crate::error::{VaultError, VaultResult};
use sha2::{Digest, Sha256};

const CACHE_MAGIC: [u8; 8] = *b"OBSMCPEM";
const CACHE_SCHEMA_VERSION: u16 = 1;
// v2: chunk-level embedding text (see vault::chunker). Bumping this
// invalidates every v1 whole-note cache automatically.
pub(crate) const EMBEDDING_INPUT_VERSION: u16 = 3;
const MAX_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 1_000_000;
const MAX_CACHE_PATH_BYTES: usize = 16 * 1024;
/// Expected upper bound on chunks per note, used only to size the cache
/// load budget. Exceeding it costs a rebuild, not corruption.
const MAX_CHUNKS_PER_NOTE_HINT: usize = 512;

/// Separator between a note path and its chunk ordinal inside store keys.
/// NUL can never occur in a real filesystem path, so `note\0<idx>` is
/// unambiguous and lets the chunk-level index reuse the existing
/// `HashMap<PathBuf, _>` store, cache format and integrity checks untouched.
pub(crate) const CHUNK_SEP: char = '\u{0}';

/// Build the store key for chunk `index` of `note`.
pub(crate) fn chunk_key(note: &Path, index: usize) -> PathBuf {
    let mut s = note.to_string_lossy().into_owned();
    s.push(CHUNK_SEP);
    s.push_str(&index.to_string());
    PathBuf::from(s)
}

/// Recover the note path from a store key. Plain note paths pass through, so
/// this is safe to call on both chunk keys and legacy whole-note keys.
pub(crate) fn note_path_of(key: &Path) -> &Path {
    match key.to_str() {
        Some(s) => match s.split_once(CHUNK_SEP) {
            Some((note, _)) => Path::new(note),
            None => key,
        },
        None => key,
    }
}

/// Suffix marking the per-note SUMMARY entry, distinct from any chunk ordinal.
const SUMMARY_MARKER: &str = "s";

/// Default weight applied to the summary arm.
///
/// Measured on a 413-note vault: every ranking metric is identical across
/// [1.18, 1.28] (overall nDCG .9442, deep .9412, casual .9754, R@1 .9211) and
/// deep-content retrieval degrades sharply at 1.32 (.941 -> .919).
///
/// Within that flat plateau the weight still decides how often a CHUNK, rather
/// than the summary, wins a note - and therefore how often a result can name
/// the passage responsible. That share falls monotonically as the weight rises
/// (31.4% at 1.18, 27.5% at 1.20, 18.8% at 1.25, 12.2% at 1.30 of top-8 hits).
/// The default sits at the low end of the plateau: ranking is unchanged, and
/// more results can cite their evidence. 1.20 keeps margin above the 1.15 drop
/// and well below the 1.32 collapse.
pub(crate) const DEFAULT_SUMMARY_WEIGHT: f32 = 1.20;

/// Store key for a note's summary vector.
pub(crate) fn summary_key(note: &Path) -> PathBuf {
    let mut s = note.to_string_lossy().into_owned();
    s.push(CHUNK_SEP);
    s.push_str(SUMMARY_MARKER);
    PathBuf::from(s)
}

/// True if `key` addresses a summary vector rather than a chunk.
pub(crate) fn is_summary_key(key: &Path) -> bool {
    key.to_str()
        .and_then(|s| s.split_once(CHUNK_SEP))
        .is_some_and(|(_, suffix)| suffix == SUMMARY_MARKER)
}

/// The chunk index encoded in `key`, or None for a summary or note-level key.
pub(crate) fn chunk_index_of(key: &Path) -> Option<usize> {
    key.to_str()
        .and_then(|s| s.split_once(CHUNK_SEP))
        .and_then(|(_, suffix)| suffix.parse::<usize>().ok())
}

/// Which stored representation produced a note's score.
///
/// Ranking only needs the score, but a caller usually wants to know *where* in
/// the note the match is. `Chunk(i)` is resolvable back to a heading trail and
/// the passage text by re-chunking the note with the same configuration --
/// deterministic, and cheaper than storing the text a second time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatchedOn {
    /// The i-th chunk, in the order `chunk_note` produces them.
    Chunk(usize),
    /// The whole-note summary vector (title + headings + first 400 words).
    Summary,
    /// A legacy v1 whole-note entry, from a cache written before chunking.
    WholeNote,
}

/// Why one note ranked, for one query.
///
/// `winner` and `best_chunk` are deliberately independent. Which arm won
/// decides the score; whether a passage can be cited does not have to. A note
/// whose summary won still has a best chunk, and reporting it lets an agent
/// see the most relevant passage without pretending it caused the ranking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NoteMatch {
    /// The representation that produced [`Self::score`].
    pub winner: MatchedOn,
    /// The ranking score: `max(best_chunk, w_sum * summary)`. Not a cosine -
    /// a weighted summary win can exceed 1.0.
    pub score: f32,
    /// The note's best chunk and its RAW (unweighted) cosine, whenever the
    /// note has chunks at all - including when the summary won.
    pub best_chunk: Option<(usize, f32)>,
    /// The summary arm's WEIGHTED score, if the note has a summary vector.
    pub summary_score: Option<f32>,
}

/// Weight applied to the summary arm, from `OBSIDIAN_SUMMARY_WEIGHT`.
///
/// A weight of 0 disables the summary arm entirely (pure chunk retrieval).
pub(crate) fn summary_weight() -> f32 {
    match std::env::var("OBSIDIAN_SUMMARY_WEIGHT") {
        Ok(raw) => match raw.trim().parse::<f32>() {
            Ok(value) if value >= 0.0 && value.is_finite() => {
                if value > 1.30 {
                    tracing::warn!(
                        weight = value,
                        "OBSIDIAN_SUMMARY_WEIGHT above 1.30 measurably degrades \
                         deep-content retrieval; 1.20-1.30 is the tested range"
                    );
                }
                value
            }
            _ => DEFAULT_SUMMARY_WEIGHT,
        },
        Err(_) => DEFAULT_SUMMARY_WEIGHT,
    }
}

/// Weight of the lexical (BM25) arm in experimental hybrid ranking.
///
/// **Zero by default: hybrid ranking is OFF.** On the corpus this fork was
/// tuned against, adding BM25 never beat semantic-only. The decisive
/// measurement was that BM25 ranked *nothing* the semantic system missed
/// (0 of 76 queries), while it could have spoiled 19 - its contribution was a
/// strict subset. The cause is structural: the summary vector already embeds
/// title and all headings, which are exactly the fields BM25 boosts hardest.
///
/// Other vaults - heavy in rare proper nouns, identifiers or exact strings -
/// may differ, so the knob exists. Values around 0.10 were the least harmful
/// here; above ~0.25 quality degrades sharply.
pub(crate) fn lexical_weight() -> f32 {
    std::env::var("OBSIDIAN_LEXICAL_WEIGHT")
        .ok()
        .and_then(|raw| raw.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0)
}

/// Scale a score set by its own maximum, leaving the floor at zero.
///
/// Deliberately NOT min-max: min-max stretches the weakest candidate of every
/// query up to 0, which hands a flat bonus to irrelevant documents when a query
/// has no real lexical signal.
pub(crate) fn unit_calibrate(scores: &mut [f32]) {
    let max = scores.iter().copied().fold(0.0f32, f32::max);
    if max > 1e-9 {
        for score in scores.iter_mut() {
            *score /= max;
        }
    } else {
        for score in scores.iter_mut() {
            *score = 0.0;
        }
    }
}

/// Prefix prepended to every DOCUMENT before embedding.
///
/// Asymmetric models need this (Snowflake Arctic wants `query: ` on queries and
/// nothing on documents; E5 wants `query: `/`passage: `). Configured explicitly
/// rather than sniffed from the model name.
pub(crate) fn document_prefix() -> String {
    std::env::var("OBSIDIAN_EMBEDDING_DOC_PREFIX").unwrap_or_else(|_| "passage: ".to_string())
}

/// Prefix prepended to every QUERY before embedding. See [`document_prefix`].
pub(crate) fn query_prefix() -> String {
    std::env::var("OBSIDIAN_EMBEDDING_QUERY_PREFIX").unwrap_or_else(|_| "query: ".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum EmbeddingBackendKind {
    Local,
    Api,
}

/// Identifies the complete vector space represented by an embedding cache.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EmbeddingSpaceIdentity {
    pub backend: EmbeddingBackendKind,
    pub model: String,
    pub endpoint_fingerprint: Option<[u8; 32]>,
    pub dimension: usize,
    pub input_version: u16,
}

impl EmbeddingSpaceIdentity {
    #[cfg(feature = "embeddings")]
    fn local(model: String, dimension: usize) -> Self {
        Self {
            backend: EmbeddingBackendKind::Local,
            model,
            endpoint_fingerprint: None,
            dimension,
            input_version: EMBEDDING_INPUT_VERSION,
        }
    }

    #[cfg(feature = "embeddings-api")]
    fn api(model: String, base_url: &str, dimension: usize) -> Self {
        Self {
            backend: EmbeddingBackendKind::Api,
            model,
            endpoint_fingerprint: Some(endpoint_fingerprint(base_url)),
            dimension,
            input_version: EMBEDDING_INPUT_VERSION,
        }
    }
}

pub(crate) trait Embedder: Send + Sync {
    fn dimension(&self) -> usize;
    fn space_identity(&self) -> &EmbeddingSpaceIdentity;
    fn embed_batch(&self, texts: &[&str]) -> VaultResult<Vec<Vec<f32>>>;
}

// ── Cosine similarity ──────────────────────────────────────────────────

pub(crate) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let (dot, norm_a, norm_b) = a
        .iter()
        .zip(b)
        .fold((0.0f32, 0.0f32, 0.0f32), |(d, na, nb), (&x, &y)| {
            (d + x * y, na + x * x, nb + y * y)
        });
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

// ── EmbeddingStore ─────────────────────────────────────────────────────

/// In-memory store mapping vault-relative note paths to embedding vectors.
///
/// Search is brute-force cosine similarity — O(n * dim). For dim=384 and
/// n=5000 this is ~2M multiply-adds, well under 5ms on modern hardware.
pub struct EmbeddingStore {
    embeddings: HashMap<PathBuf, EmbeddingEntry>,
    dim: usize,
    identity: Option<EmbeddingSpaceIdentity>,
    first_pass_complete: bool,
}

#[derive(Debug, Clone)]
struct EmbeddingEntry {
    vector: Vec<f32>,
    content_hash: Option<[u8; 32]>,
}

#[derive(serde::Serialize)]
struct EmbeddingCacheDataRef<'a> {
    magic: [u8; 8],
    schema_version: u16,
    identity: &'a EmbeddingSpaceIdentity,
    first_pass_complete: bool,
    entries: Vec<EmbeddingCacheEntryRef<'a>>,
}

#[derive(serde::Serialize)]
struct EmbeddingCacheEntryRef<'a> {
    path: String,
    content_hash: [u8; 32],
    vector: &'a [f32],
}

#[derive(serde::Serialize, serde::Deserialize)]
struct EmbeddingCacheData {
    magic: [u8; 8],
    schema_version: u16,
    identity: EmbeddingSpaceIdentity,
    first_pass_complete: bool,
    entries: Vec<EmbeddingCacheEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct EmbeddingCacheEntry {
    path: String,
    content_hash: [u8; 32],
    vector: Vec<f32>,
}

impl EmbeddingStore {
    /// Create an empty store for embeddings of the given dimensionality.
    pub fn new(dim: usize) -> Self {
        Self {
            embeddings: HashMap::new(),
            dim,
            identity: None,
            first_pass_complete: false,
        }
    }

    pub(crate) fn new_with_identity(identity: EmbeddingSpaceIdentity) -> Self {
        Self {
            embeddings: HashMap::new(),
            dim: identity.dimension,
            identity: Some(identity),
            first_pass_complete: false,
        }
    }

    /// Insert or replace the embedding for a note.
    ///
    /// Vectors with a dimension mismatch are rejected (logged + skipped)
    /// to prevent garbage cosine-similarity results from a misconfigured
    /// API backend.
    pub fn insert(&mut self, path: PathBuf, vec: Vec<f32>) {
        if validate_vector(&vec, self.dim).is_err() {
            tracing::warn!(
                path = %path.display(),
                expected = self.dim,
                got = vec.len(),
                "embedding dimension mismatch — skipping insert"
            );
            return;
        }
        self.embeddings.insert(
            path,
            EmbeddingEntry {
                vector: vec,
                content_hash: None,
            },
        );
        self.first_pass_complete = false;
    }

    pub(crate) fn insert_hashed(
        &mut self,
        path: PathBuf,
        content_hash: [u8; 32],
        vector: Vec<f32>,
    ) -> VaultResult<()> {
        validate_vector(&vector, self.dim)?;
        self.embeddings.insert(
            path,
            EmbeddingEntry {
                vector,
                content_hash: Some(content_hash),
            },
        );
        Ok(())
    }

    /// Best (maximum) cosine similarity across every chunk of `note`.
    ///
    /// The hybrid re-ranker scores one BM25 candidate at a time BY NOTE PATH.
    /// With a chunk-level store a bare `get(note)` always misses, which
    /// silently zeroes the semantic half of `alpha*BM25 + (1-alpha)*semantic`
    /// and degrades hybrid search to pure BM25. Walk the note's chunks instead.
    /// Chunk keys are contiguous from 0, so this stops at the first gap.
    pub(crate) fn best_score_for_note(&self, note: &Path, query_vec: &[f32]) -> Option<f32> {
        self.match_for_note(note, query_vec).map(|m| m.score)
    }

    /// As [`Self::best_score_for_note`], but also reports WHICH stored
    /// representation won.
    ///
    /// The score alone is enough to rank, but not enough to tell a caller where
    /// in the note the match actually is. Keeping the winning key lets
    /// `search_semantic` return the matched passage and its heading trail
    /// instead of only the note that contains it somewhere.
    pub(crate) fn match_for_note(&self, note: &Path, query_vec: &[f32]) -> Option<NoteMatch> {
        // Legacy whole-note key (v1 caches) still answers directly.
        if let Some(entry) = self.embeddings.get(note) {
            return Some(NoteMatch {
                winner: MatchedOn::WholeNote,
                score: cosine_similarity(query_vec, &entry.vector),
                best_chunk: None,
                summary_score: None,
            });
        }
        let mut best_chunk: Option<(usize, f32)> = None;
        let mut index = 0usize;
        while let Some(entry) = self.embeddings.get(&chunk_key(note, index)) {
            let score = cosine_similarity(query_vec, &entry.vector);
            if best_chunk.is_none_or(|(_, current)| score > current) {
                best_chunk = Some((index, score));
            }
            index += 1;
        }
        let summary_score = self
            .embeddings
            .get(&summary_key(note))
            .map(|entry| summary_weight() * cosine_similarity(query_vec, &entry.vector));
        Self::decide(best_chunk, summary_score)
    }

    /// Pick the winning arm. The summary arm competes with the best chunk,
    /// scaled by its weight.
    ///
    /// `max` is deliberate and monotone: a summary can only rescue a note,
    /// never dilute one whose answer lives in a single chunk. Ties go to the
    /// chunk, because a specific passage is the more useful attribution.
    fn decide(best_chunk: Option<(usize, f32)>, summary_score: Option<f32>) -> Option<NoteMatch> {
        let chunk_score = best_chunk.map(|(_, score)| score);
        let (winner, score) = match (chunk_score, summary_score) {
            (Some(chunk), Some(summary)) if summary > chunk => (MatchedOn::Summary, summary),
            (Some(chunk), _) => (
                MatchedOn::Chunk(best_chunk.expect("chunk score implies a chunk").0),
                chunk,
            ),
            (None, Some(summary)) => (MatchedOn::Summary, summary),
            (None, None) => return None,
        };
        Some(NoteMatch {
            winner,
            score,
            best_chunk,
            summary_score,
        })
    }

    /// True if `note` has at least one chunk in the store.
    ///
    /// O(1): chunks are always written from index 0 upward and committed as a
    /// unit, so the presence of chunk 0 is equivalent to the note being indexed.
    pub(crate) fn has_note(&self, note: &Path) -> bool {
        self.embeddings.contains_key(&chunk_key(note, 0))
            || self.embeddings.contains_key(&summary_key(note))
    }

    /// Remove every chunk belonging to `note`. Returns true if anything went.
    pub(crate) fn remove_note(&mut self, note: &Path) -> bool {
        let before = self.embeddings.len();
        self.embeddings.retain(|key, _| note_path_of(key) != note);
        self.embeddings.len() != before
    }

    pub fn remove(&mut self, path: &Path) -> bool {
        self.embeddings.remove(path).is_some()
    }

    /// Retrieve a note's embedding vector.
    pub fn get(&self, path: &Path) -> Option<&[f32]> {
        self.embeddings
            .get(path)
            .map(|entry| entry.vector.as_slice())
    }

    pub(crate) fn content_hash(&self, path: &Path) -> Option<&[u8; 32]> {
        self.embeddings
            .get(path)
            .and_then(|entry| entry.content_hash.as_ref())
    }

    pub fn len(&self) -> usize {
        self.embeddings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> Option<&EmbeddingSpaceIdentity> {
        self.identity.as_ref()
    }

    pub(crate) fn first_pass_complete(&self) -> bool {
        self.first_pass_complete
    }

    pub(crate) fn set_first_pass_complete(&mut self, complete: bool) {
        self.first_pass_complete = complete;
    }

    /// Drop entries for notes that no longer exist.
    ///
    /// `paths` holds NOTE paths, but entries are keyed by CHUNK
    /// (`note\0<idx>`), so the membership test must resolve the key back to
    /// its note first. Comparing raw keys against note paths matches nothing
    /// and silently wipes the whole cache on every startup, re-embedding the
    /// entire vault each run.
    pub(crate) fn retain_paths(&mut self, paths: &HashSet<PathBuf>) -> bool {
        let previous_len = self.embeddings.len();
        self.embeddings
            .retain(|key, _| paths.contains(note_path_of(key)));
        self.embeddings.len() != previous_len
    }

    /// Find the `top_k` most similar notes to `query_vec`, sorted by
    /// descending cosine similarity.
    pub fn query(&self, query_vec: &[f32], top_k: usize) -> Vec<(PathBuf, f32)> {
        Self::drop_provenance(self.query_detailed(query_vec, top_k))
    }

    /// As [`Self::query`], but keeps the full match record for each note.
    pub(crate) fn query_detailed(
        &self,
        query_vec: &[f32],
        top_k: usize,
    ) -> Vec<(PathBuf, NoteMatch)> {
        // Collapse chunk keys to note paths (best chunk wins) so every consumer
        // of the store sees note-level results, exactly as before chunking.
        Self::rank_detailed(self.collapse_to_notes(query_vec, None), top_k)
    }

    fn drop_provenance(scored: Vec<(PathBuf, NoteMatch)>) -> Vec<(PathBuf, f32)> {
        scored
            .into_iter()
            .map(|(note, matched)| (note, matched.score))
            .collect()
    }

    /// Score every chunk and reduce to one `(note, best_score, matched_on)`.
    ///
    /// The winning key is kept, not just its score, so a caller can report the
    /// matched passage rather than only the note containing it.
    fn collapse_to_notes(
        &self,
        query_vec: &[f32],
        allowed: Option<&HashSet<PathBuf>>,
    ) -> Vec<(PathBuf, NoteMatch)> {
        /// Both arms accumulated per note, so evidence survives losing.
        #[derive(Default, Clone, Copy)]
        struct Arms {
            best_chunk: Option<(usize, f32)>,
            summary: Option<f32>,
            whole: Option<f32>,
        }

        let weight = summary_weight();
        let mut arms: HashMap<PathBuf, Arms> = HashMap::new();
        for (key, entry) in self.embeddings.iter() {
            let note = note_path_of(key);
            if allowed.is_some_and(|set| !set.contains(note)) {
                continue;
            }
            let cosine = cosine_similarity(query_vec, &entry.vector);
            let slot = arms.entry(note.to_path_buf()).or_default();
            if is_summary_key(key) {
                slot.summary = Some(weight * cosine);
            } else if let Some(index) = chunk_index_of(key) {
                if slot.best_chunk.is_none_or(|(_, current)| cosine > current) {
                    slot.best_chunk = Some((index, cosine));
                }
            } else {
                // Legacy v1 whole-note entry.
                slot.whole = Some(cosine);
            }
        }
        arms.into_iter()
            .filter_map(|(note, arm)| {
                if let Some(score) = arm.whole {
                    return Some((
                        note,
                        NoteMatch {
                            winner: MatchedOn::WholeNote,
                            score,
                            best_chunk: None,
                            summary_score: None,
                        },
                    ));
                }
                Self::decide(arm.best_chunk, arm.summary).map(|m| (note, m))
            })
            .collect()
    }

    pub(crate) fn query_paths(
        &self,
        query_vec: &[f32],
        allowed_paths: &HashSet<PathBuf>,
        top_k: usize,
    ) -> Vec<(PathBuf, f32)> {
        Self::drop_provenance(self.query_paths_detailed(query_vec, allowed_paths, top_k))
    }

    /// As [`Self::query_paths`], but keeps the full match record.
    pub(crate) fn query_paths_detailed(
        &self,
        query_vec: &[f32],
        allowed_paths: &HashSet<PathBuf>,
        top_k: usize,
    ) -> Vec<(PathBuf, NoteMatch)> {
        // Keys are chunk keys. Score every chunk, then collapse to one score per
        // note by taking its best chunk, so the hybrid fusion contract
        // (`alpha * BM25 + (1 - alpha) * semantic`, keyed by note path) is
        // preserved exactly as upstream expects.
        Self::rank_detailed(
            self.collapse_to_notes(query_vec, Some(allowed_paths)),
            top_k,
        )
    }

    fn rank_detailed(
        mut scored: Vec<(PathBuf, NoteMatch)>,
        top_k: usize,
    ) -> Vec<(PathBuf, NoteMatch)> {
        let cmp = |a: &(PathBuf, NoteMatch), b: &(PathBuf, NoteMatch)| {
            b.1.score
                .partial_cmp(&a.1.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        };

        if top_k < scored.len() {
            scored.select_nth_unstable_by(top_k, cmp);
            scored.truncate(top_k);
            scored.sort_unstable_by(cmp);
        } else {
            scored.sort_unstable_by(cmp);
        }
        scored
    }

    /// Serialize the store to a binary cache file.
    pub fn save(&self, path: &Path) -> VaultResult<()> {
        let bytes = self.encode_cache()?;
        Self::persist_cache_bytes(path, &bytes, None).map(|_| ())
    }

    pub(crate) fn encode_cache(&self) -> VaultResult<Vec<u8>> {
        let identity = self.identity.as_ref().ok_or_else(|| {
            VaultError::Embedding("embedding store has no vector-space identity".into())
        })?;
        validate_space_identity(identity)?;
        if identity.dimension != self.dim {
            return Err(VaultError::Embedding(
                "embedding store identity has an invalid dimension".into(),
            ));
        }

        let mut entries = self
            .embeddings
            .iter()
            .map(|(path, entry)| {
                let path = path.to_str().ok_or_else(|| {
                    VaultError::Embedding(format!(
                        "embedding cache path is not valid UTF-8: '{}'",
                        path.display()
                    ))
                })?;
                validate_cache_path(path)?;
                validate_vector(&entry.vector, self.dim)?;
                let content_hash = entry.content_hash.ok_or_else(|| {
                    VaultError::Embedding(format!(
                        "embedding cache entry '{path}' has no prepared-text hash"
                    ))
                })?;
                Ok(EmbeddingCacheEntryRef {
                    path: path.to_string(),
                    content_hash,
                    vector: &entry.vector,
                })
            })
            .collect::<VaultResult<Vec<_>>>()?;
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let data = EmbeddingCacheDataRef {
            magic: CACHE_MAGIC,
            schema_version: CACHE_SCHEMA_VERSION,
            identity,
            first_pass_complete: self.first_pass_complete,
            entries,
        };
        let bytes = bincode::serde::encode_to_vec(&data, bincode::config::standard())
            .map_err(|e| VaultError::Embedding(format!("cache serialize error: {e}")))?;
        if bytes.len() as u64 > MAX_CACHE_BYTES {
            return Err(VaultError::Embedding(format!(
                "embedding cache is too large to persist: {} bytes (limit {MAX_CACHE_BYTES})",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    pub(crate) fn persist_cache_bytes_if_live(
        path: &Path,
        bytes: &[u8],
        live: &AtomicBool,
    ) -> VaultResult<bool> {
        Self::persist_cache_bytes(path, bytes, Some(live))
    }

    fn persist_cache_bytes(
        path: &Path,
        bytes: &[u8],
        live: Option<&AtomicBool>,
    ) -> VaultResult<bool> {
        if live.is_some_and(|flag| !flag.load(AtomicOrdering::Acquire)) {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            temp.write_all(bytes)?;
            temp.flush()?;
            temp.as_file().sync_all()?;
            if live.is_some_and(|flag| !flag.load(AtomicOrdering::Acquire)) {
                return Ok(false);
            }
            temp.persist(path)
                .map_err(|error| VaultError::Io(error.error))?;
            return Ok(true);
        }
        Err(VaultError::Embedding(format!(
            "embedding cache path has no parent: {}",
            path.display()
        )))
    }

    /// Deserialize a store from a binary cache file.
    pub fn load(path: &Path) -> VaultResult<Self> {
        Self::load_bounded(path, None, MAX_CACHE_ENTRIES)
    }

    pub(crate) fn load_for_space(
        path: &Path,
        expected: &EmbeddingSpaceIdentity,
        current_note_count: usize,
    ) -> VaultResult<Self> {
        // Entries are CHUNKS, not notes. Allow a generous per-note chunk fan-out
        // so a legitimate chunk-level cache is never rejected as oversized.
        Self::load_bounded(
            path,
            Some(expected),
            current_note_count
                .saturating_mul(MAX_CHUNKS_PER_NOTE_HINT)
                .saturating_add(1024)
                .min(MAX_CACHE_ENTRIES),
        )
    }

    fn load_bounded(
        path: &Path,
        expected: Option<&EmbeddingSpaceIdentity>,
        max_entries: usize,
    ) -> VaultResult<Self> {
        let metadata = std::fs::metadata(path)?;
        let expected_dim = expected.map_or(384, |identity| identity.dimension.max(1));
        let per_entry = MAX_CACHE_PATH_BYTES
            .saturating_add(expected_dim.saturating_mul(std::mem::size_of::<f32>()))
            .saturating_add(128);
        let derived_limit = max_entries
            .max(1)
            .saturating_mul(per_entry)
            .saturating_add(1024 * 1024) as u64;
        let byte_limit = derived_limit.min(MAX_CACHE_BYTES);
        if metadata.len() > byte_limit {
            return Err(VaultError::Embedding(format!(
                "embedding cache is too large: {} bytes (limit {byte_limit})",
                metadata.len()
            )));
        }

        let bytes = std::fs::read(path)?;
        let config = bincode::config::standard().with_limit::<1073741824>();
        let (data, consumed): (EmbeddingCacheData, usize) =
            bincode::serde::decode_from_slice(&bytes, config)
                .map_err(|e| VaultError::Embedding(format!("cache deserialize error: {e}")))?;
        if consumed != bytes.len() {
            return Err(VaultError::Embedding(
                "embedding cache contains trailing bytes".into(),
            ));
        }
        if data.magic != CACHE_MAGIC {
            return Err(VaultError::Embedding(
                "unsupported legacy embedding cache format".into(),
            ));
        }
        if data.schema_version != CACHE_SCHEMA_VERSION {
            return Err(VaultError::Embedding(format!(
                "unsupported embedding cache schema version {}",
                data.schema_version
            )));
        }
        validate_space_identity(&data.identity)?;
        if let Some(expected) = expected
            && &data.identity != expected
        {
            return Err(VaultError::Embedding(
                "embedding cache vector-space identity mismatch".into(),
            ));
        }
        if data.entries.len() > max_entries {
            return Err(VaultError::Embedding(format!(
                "embedding cache contains too many entries: {} (limit {max_entries})",
                data.entries.len()
            )));
        }

        let dim = data.identity.dimension;
        let mut embeddings = HashMap::with_capacity(data.entries.len());
        let mut canonical_paths = HashSet::with_capacity(data.entries.len());
        for entry in data.entries {
            let relative = validate_cache_path(&entry.path)?;
            let canonical = super::path::canonical_unicode_key(&entry.path);
            if !canonical_paths.insert(canonical) || embeddings.contains_key(&relative) {
                return Err(VaultError::Embedding(format!(
                    "embedding cache contains duplicate path '{}'",
                    entry.path
                )));
            }
            validate_vector(&entry.vector, dim)?;
            embeddings.insert(
                relative,
                EmbeddingEntry {
                    vector: entry.vector,
                    content_hash: Some(entry.content_hash),
                },
            );
        }

        Ok(Self {
            embeddings,
            dim,
            identity: Some(data.identity),
            first_pass_complete: data.first_pass_complete,
        })
    }
}

fn validate_space_identity(identity: &EmbeddingSpaceIdentity) -> VaultResult<()> {
    if identity.dimension == 0 {
        return Err(VaultError::Embedding(
            "embedding cache dimension must be greater than zero".into(),
        ));
    }
    if identity.model.trim().is_empty() {
        return Err(VaultError::Embedding(
            "embedding cache model identity must not be empty".into(),
        ));
    }
    if identity.input_version != EMBEDDING_INPUT_VERSION {
        return Err(VaultError::Embedding(format!(
            "unsupported embedding input version {}",
            identity.input_version
        )));
    }
    match (identity.backend, identity.endpoint_fingerprint) {
        (EmbeddingBackendKind::Local, None) | (EmbeddingBackendKind::Api, Some(_)) => Ok(()),
        (EmbeddingBackendKind::Local, Some(_)) => Err(VaultError::Embedding(
            "local embedding cache identity must not contain an API endpoint fingerprint".into(),
        )),
        (EmbeddingBackendKind::Api, None) => Err(VaultError::Embedding(
            "API embedding cache identity is missing its endpoint fingerprint".into(),
        )),
    }
}

fn validate_cache_path(path: &str) -> VaultResult<PathBuf> {
    if path.is_empty() || path.len() > MAX_CACHE_PATH_BYTES || path.contains('\\') {
        return Err(VaultError::Embedding(format!(
            "invalid embedding cache path '{path}'"
        )));
    }
    let original = Path::new(path);
    let normalized = super::path::normalize_relative(original).map_err(|error| {
        VaultError::Embedding(format!("invalid embedding cache path '{path}': {error}"))
    })?;
    if normalized != original || normalized.to_string_lossy() != path {
        return Err(VaultError::Embedding(format!(
            "embedding cache path is not normalized: '{path}'"
        )));
    }
    Ok(normalized)
}

fn validate_vector(vector: &[f32], expected_dim: usize) -> VaultResult<()> {
    if vector.len() != expected_dim {
        return Err(VaultError::Embedding(format!(
            "embedding dimension mismatch: expected {expected_dim}, got {}",
            vector.len()
        )));
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(VaultError::Embedding(
            "embedding vector contains a non-finite value".into(),
        ));
    }
    Ok(())
}

pub(crate) fn prepared_text_hash(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

#[cfg(feature = "embeddings-api")]
fn endpoint_fingerprint(base_url: &str) -> [u8; 32] {
    let normalized = base_url.trim().trim_end_matches('/');
    Sha256::digest(normalized.as_bytes()).into()
}

#[cfg(feature = "embeddings-api")]
fn short_fingerprint(fingerprint: &[u8; 32]) -> String {
    fingerprint[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ── EmbeddingBackend ───────────────────────────────────────────────────

enum EmbeddingBackend {
    #[cfg(feature = "embeddings")]
    Local(Box<std::sync::Mutex<fastembed::TextEmbedding>>),

    #[cfg(feature = "embeddings-api")]
    Api(ApiEmbeddingClient),
}

#[cfg(feature = "embeddings-api")]
struct ApiEmbeddingRequest {
    texts: Vec<String>,
    response: std::sync::mpsc::Sender<VaultResult<Vec<Vec<f32>>>>,
}

#[cfg(feature = "embeddings-api")]
struct ApiEmbeddingClient {
    sender: Option<std::sync::mpsc::SyncSender<ApiEmbeddingRequest>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "embeddings-api")]
impl ApiEmbeddingClient {
    const WORKER_COUNT: usize = 2;
    const QUEUE_CAPACITY: usize = 2;

    fn start(
        client: reqwest::blocking::Client,
        base_url: String,
        model: String,
        api_key: zeroize::Zeroizing<String>,
    ) -> VaultResult<Self> {
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<ApiEmbeddingRequest>(Self::QUEUE_CAPACITY);
        let receiver = Arc::new(std::sync::Mutex::new(receiver));
        let base_url = Arc::new(base_url);
        let model = Arc::new(model);
        let api_key = Arc::new(api_key);
        let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::with_capacity(Self::WORKER_COUNT);
        for worker_index in 0..Self::WORKER_COUNT {
            let client = client.clone();
            let receiver = Arc::clone(&receiver);
            let base_url = Arc::clone(&base_url);
            let model = Arc::clone(&model);
            let api_key = Arc::clone(&api_key);
            let worker = match std::thread::Builder::new()
                .name(format!("obsidian-mcp-embedding-api-{worker_index}"))
                .spawn(move || {
                    loop {
                        let request = receiver
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .recv();
                        let Ok(request) = request else {
                            break;
                        };
                        let texts = request.texts.iter().map(String::as_str).collect::<Vec<_>>();
                        let result = embed_batch_api(
                            &client,
                            base_url.as_str(),
                            model.as_str(),
                            api_key.as_str(),
                            &texts,
                        );
                        let _ = request.response.send(result);
                    }
                }) {
                Ok(worker) => worker,
                Err(error) => {
                    drop(sender);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(VaultError::Embedding(format!(
                        "failed to start embedding API request worker: {error}"
                    )));
                }
            };
            workers.push(worker);
        }
        Ok(Self {
            sender: Some(sender),
            workers,
        })
    }

    fn embed_batch(&self, texts: &[&str]) -> VaultResult<Vec<Vec<f32>>> {
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| VaultError::Embedding("embedding API worker unavailable".into()))?;
        let (response, receiver) = std::sync::mpsc::channel();
        sender
            .send(ApiEmbeddingRequest {
                texts: texts.iter().map(|text| (*text).to_string()).collect(),
                response,
            })
            .map_err(|_| VaultError::Embedding("embedding API worker stopped".into()))?;
        receiver
            .recv()
            .map_err(|_| VaultError::Embedding("embedding API worker stopped".into()))?
    }
}

#[cfg(feature = "embeddings-api")]
impl Drop for ApiEmbeddingClient {
    fn drop(&mut self) {
        self.sender.take();
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                tracing::warn!("embedding API request worker panicked during shutdown");
            }
        }
    }
}

// ── EmbeddingModel ─────────────────────────────────────────────────────

/// Backend-agnostic embedding model supporting local fastembed and
/// OpenAI-compatible API backends.
pub struct EmbeddingModel {
    backend: EmbeddingBackend,
    dim: usize,
    identity: EmbeddingSpaceIdentity,
}

impl EmbeddingModel {
    /// Load an embedding model using the specified (or inferred) backend.
    ///
    /// `provider` selects the backend explicitly; `None` infers from compiled
    /// features (local preferred when both are available).
    pub async fn load(model_name: &str, provider: Option<EmbeddingProvider>) -> VaultResult<Self> {
        match resolve_provider(provider) {
            EmbeddingProvider::Local => Self::load_local(model_name).await,
            EmbeddingProvider::Api => Self::load_api(model_name).await,
        }
    }

    /// Embed a batch of texts. Returns one vector per input text.
    pub fn embed_batch(&self, texts: &[&str]) -> VaultResult<Vec<Vec<f32>>> {
        <Self as Embedder>::embed_batch(self, texts)
    }

    /// Embed a single text. Convenience wrapper over `embed_batch`.
    pub fn embed_one(&self, text: &str) -> VaultResult<Vec<f32>> {
        let mut results = self.embed_batch(&[text])?;
        results
            .pop()
            .ok_or_else(|| VaultError::Embedding("embed returned empty result".into()))
    }

    /// Embedding dimensionality for the loaded model.
    pub fn dim(&self) -> usize {
        self.dim
    }

    // ── Local backend (fastembed) ──────────────────────────────────────

    #[cfg(feature = "embeddings")]
    async fn load_local(model_name: &str) -> VaultResult<Self> {
        let model_name = model_name.to_owned();

        tokio::task::spawn_blocking(move || {
            let (model_enum, canonical_model, dim) = resolve_local_model(&model_name)?;
            let identity = EmbeddingSpaceIdentity::local(canonical_model, dim);

            let options = fastembed::InitOptions::new(model_enum).with_show_download_progress(true);

            let inner = fastembed::TextEmbedding::try_new(options)
                .map_err(|e| VaultError::Embedding(format!("model load failed: {e}")))?;

            Ok(Self {
                backend: EmbeddingBackend::Local(Box::new(std::sync::Mutex::new(inner))),
                dim,
                identity,
            })
        })
        .await
        .map_err(|e| VaultError::Embedding(format!("spawn_blocking join error: {e}")))?
    }

    #[cfg(not(feature = "embeddings"))]
    async fn load_local(_model_name: &str) -> VaultResult<Self> {
        Err(VaultError::Embedding(
            "local embedding backend not compiled (needs --features embeddings)".into(),
        ))
    }

    // ── API backend (OpenAI-compatible) ────────────────────────────────

    #[cfg(feature = "embeddings-api")]
    async fn load_api(model_name: &str) -> VaultResult<Self> {
        let model_name = model_name.to_owned();

        tokio::task::spawn_blocking(move || {
            let api_key = zeroize::Zeroizing::new(
                read_env_with_fallback("OBSIDIAN_EMBEDDING_API_KEY", "OPENAI_API_KEY").ok_or_else(
                    || {
                        VaultError::Embedding(
                            "API key required: set OBSIDIAN_EMBEDDING_API_KEY or OPENAI_API_KEY"
                                .into(),
                        )
                    },
                )?,
            );

            let base_url = read_env_with_fallback("OBSIDIAN_EMBEDDING_API_BASE", "OPENAI_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

            let model = read_env_with_fallback("OBSIDIAN_EMBEDDING_API_MODEL", "OPENAI_MODEL")
                .unwrap_or(model_name);

            let client = build_api_client()?;
            let api_client =
                ApiEmbeddingClient::start(client, base_url.clone(), model.clone(), api_key)?;

            let dim = match parse_usize_env("OBSIDIAN_EMBEDDING_DIM") {
                Some(0) => {
                    return Err(VaultError::Embedding(
                        "OBSIDIAN_EMBEDDING_DIM must be greater than zero".into(),
                    ));
                }
                Some(d) => {
                    tracing::info!(dim = d, "using explicit embedding dimension");
                    d
                }
                None => {
                    tracing::info!("probing embedding API for dimension…");
                    probe_api_dimension(&api_client)?
                }
            };
            let identity = EmbeddingSpaceIdentity::api(model.clone(), &base_url, dim);
            let endpoint = identity
                .endpoint_fingerprint
                .as_ref()
                .map(short_fingerprint)
                .unwrap_or_default();

            tracing::info!(
                endpoint_fingerprint = %endpoint,
                model = %model,
                dim,
                "API embedding backend ready"
            );

            Ok(Self {
                backend: EmbeddingBackend::Api(api_client),
                dim,
                identity,
            })
        })
        .await
        .map_err(|e| VaultError::Embedding(format!("spawn_blocking join error: {e}")))?
    }

    #[cfg(not(feature = "embeddings-api"))]
    async fn load_api(_model_name: &str) -> VaultResult<Self> {
        Err(VaultError::Embedding(
            "API embedding backend not compiled (needs --features embeddings-api)".into(),
        ))
    }
}

impl Embedder for EmbeddingModel {
    fn dimension(&self) -> usize {
        self.dim
    }

    fn space_identity(&self) -> &EmbeddingSpaceIdentity {
        &self.identity
    }

    fn embed_batch(&self, texts: &[&str]) -> VaultResult<Vec<Vec<f32>>> {
        let vectors = match &self.backend {
            #[cfg(feature = "embeddings")]
            EmbeddingBackend::Local(inner) => {
                let mut model = inner
                    .lock()
                    .map_err(|e| VaultError::Embedding(format!("model lock poisoned: {e}")))?;
                model
                    .embed(texts, Some(64))
                    .map_err(|e| VaultError::Embedding(format!("embed failed: {e}")))?
            }
            #[cfg(feature = "embeddings-api")]
            EmbeddingBackend::Api(client) => client.embed_batch(texts)?,
        };
        validate_embedding_batch(vectors, texts.len(), self.dim)
    }
}

#[cfg(feature = "embeddings")]
fn resolve_local_model(
    configured_name: &str,
) -> VaultResult<(fastembed::EmbeddingModel, String, usize)> {
    let configured_name = configured_name.trim();
    let model = match configured_name.parse().ok() {
        Some(model) => model,
        None => {
            let supported = fastembed::TextEmbedding::list_supported_models();
            let mut matches = supported
                .iter()
                .filter(|info| info.model_code.eq_ignore_ascii_case(configured_name))
                .map(|info| info.model.clone())
                .collect::<Vec<_>>();
            if matches.is_empty()
                && let Some(repository_name) = configured_name.split_once('/').map(|(_, name)| name)
            {
                matches = supported
                    .iter()
                    .filter(|info| {
                        info.model_code
                            .split_once('/')
                            .is_some_and(|(_, name)| name.eq_ignore_ascii_case(repository_name))
                    })
                    .map(|info| info.model.clone())
                    .collect();
            }
            match matches.as_slice() {
                [model] => model.clone(),
                [] => {
                    return Err(VaultError::Embedding(format!(
                        "unknown local embedding model '{configured_name}'"
                    )));
                }
                _ => {
                    return Err(VaultError::Embedding(format!(
                        "ambiguous local embedding model '{configured_name}'; use a fastembed model enum name"
                    )));
                }
            }
        }
    };
    let info = fastembed::TextEmbedding::get_model_info(&model).map_err(|error| {
        VaultError::Embedding(format!(
            "embedding metadata unavailable for local model '{configured_name}': {error}"
        ))
    })?;
    let canonical_model = format!("{model:?}");
    let dimension = info.dim;
    Ok((model, canonical_model, dimension))
}

// ── Provider resolution ────────────────────────────────────────────────

fn resolve_provider(explicit: Option<EmbeddingProvider>) -> EmbeddingProvider {
    if let Some(p) = explicit {
        return p;
    }

    let has_local = cfg!(feature = "embeddings");
    let has_api = cfg!(feature = "embeddings-api");

    match (has_local, has_api) {
        (true, _) => EmbeddingProvider::Local,
        (false, true) => EmbeddingProvider::Api,
        (false, false) => unreachable!("embeddings module compiled without any backend"),
    }
}

// ── API client helpers ─────────────────────────────────────────────────

#[cfg(feature = "embeddings-api")]
fn build_api_client() -> Result<reqwest::blocking::Client, VaultError> {
    let mut builder =
        reqwest::blocking::ClientBuilder::new().timeout(std::time::Duration::from_secs(30));

    if let Ok(cert_path) = std::env::var("OBSIDIAN_EMBEDDING_CA_CERT") {
        let cert_pem = std::fs::read(&cert_path).map_err(|e| {
            VaultError::Embedding(format!("failed to read CA cert {cert_path}: {e}"))
        })?;
        let cert = reqwest::Certificate::from_pem(&cert_pem)
            .map_err(|e| VaultError::Embedding(format!("invalid CA cert: {e}")))?;
        builder = builder.add_root_certificate(cert);
    }

    if std::env::var("OBSIDIAN_EMBEDDING_TLS_VERIFY")
        .map(|v| v.eq_ignore_ascii_case("false") || v == "0")
        .unwrap_or(false)
    {
        tracing::warn!(
            "TLS verification disabled for embedding API — NOT recommended for production"
        );
        builder = builder.danger_accept_invalid_certs(true);
    }

    builder
        .build()
        .map_err(|e| VaultError::Embedding(format!("failed to build HTTP client: {e}")))
}

#[cfg(feature = "embeddings-api")]
fn probe_api_dimension(client: &ApiEmbeddingClient) -> Result<usize, VaultError> {
    let vecs = client.embed_batch(&["dim"])?;
    let first = vecs
        .first()
        .ok_or_else(|| VaultError::Embedding("dimension probe returned empty result".into()))?;
    if first.is_empty() {
        return Err(VaultError::Embedding(
            "dimension probe returned zero-length vector".into(),
        ));
    }
    Ok(first.len())
}

#[cfg(feature = "embeddings-api")]
fn embed_batch_api(
    client: &reqwest::blocking::Client,
    base_url: &str,
    model: &str,
    api_key: &str,
    texts: &[&str],
) -> Result<Vec<Vec<f32>>, VaultError> {
    let url = format!("{}/embeddings", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "input": texts,
        "encoding_format": "float",
    });

    const MAX_RETRIES: u8 = 3;
    let mut attempt = 0u8;
    loop {
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .map_err(|error| {
                let detail = if error.is_timeout() {
                    "request timed out"
                } else if error.is_connect() {
                    "connection failed"
                } else if error.is_builder() {
                    "request could not be constructed"
                } else {
                    "request failed"
                };
                VaultError::Embedding(format!("embedding API {detail}"))
            })?;

        let status = response.status();
        if status.as_u16() == 429 && attempt < MAX_RETRIES {
            let wait = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1u64 << attempt)
                .min(30);
            attempt += 1;
            tracing::warn!(
                retry_after_secs = wait,
                attempt = attempt,
                max_retries = MAX_RETRIES,
                "embedding API rate limited (attempt {attempt}/{MAX_RETRIES})"
            );
            std::thread::sleep(std::time::Duration::from_secs(wait));
            continue;
        }

        if !status.is_success() {
            return Err(VaultError::Embedding(format!(
                "embedding API returned HTTP status {status}"
            )));
        }

        let resp: serde_json::Value = response.json().map_err(|_| {
            VaultError::Embedding("embedding API returned invalid JSON".to_string())
        })?;

        return parse_embedding_response(&resp, texts.len());
    }
}

/// Parse an OpenAI-compatible embedding API response into embedding vectors.
///
/// Providers may either omit every `index` and preserve array order, or include
/// a complete unique index set. Mixed or partial responses are rejected.
#[cfg(feature = "embeddings-api")]
fn parse_embedding_response(
    resp: &serde_json::Value,
    expected_count: usize,
) -> Result<Vec<Vec<f32>>, VaultError> {
    let data = resp["data"]
        .as_array()
        .ok_or_else(|| VaultError::Embedding("missing 'data' array in API response".into()))?;
    if data.len() != expected_count {
        return Err(VaultError::Embedding(format!(
            "embedding API returned {} vectors for {expected_count} inputs",
            data.len()
        )));
    }

    let indexed_response = data.first().is_some_and(|item| !item["index"].is_null());

    let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
    for (array_pos, item) in data.iter().enumerate() {
        let has_index = !item["index"].is_null();
        if has_index != indexed_response {
            return Err(VaultError::Embedding(
                "embedding API returned mixed indexed and unindexed items".into(),
            ));
        }
        let idx = if indexed_response {
            let raw = item["index"].as_u64().ok_or_else(|| {
                VaultError::Embedding("embedding response index must be an unsigned integer".into())
            })?;
            usize::try_from(raw).map_err(|_| {
                VaultError::Embedding("embedding response index is out of range".into())
            })?
        } else {
            array_pos
        };
        if idx >= expected_count {
            return Err(VaultError::Embedding(format!(
                "embedding response index {idx} is out of range for {expected_count} inputs"
            )));
        }
        let vec = item["embedding"]
            .as_array()
            .ok_or_else(|| {
                VaultError::Embedding("missing 'embedding' array in response item".into())
            })?
            .iter()
            .map(|v| {
                v.as_f64()
                    .ok_or_else(|| {
                        VaultError::Embedding("non-numeric value in embedding vector".into())
                    })
                    .and_then(|f| {
                        let value = f as f32;
                        value.is_finite().then_some(value).ok_or_else(|| {
                            VaultError::Embedding("non-finite value in embedding vector".into())
                        })
                    })
            })
            .collect::<Result<Vec<f32>, _>>()?;
        indexed.push((idx, vec));
    }

    if indexed_response {
        indexed.sort_unstable_by_key(|(idx, _)| *idx);
        for (expected, (actual, _)) in indexed.iter().enumerate() {
            if *actual != expected {
                return Err(VaultError::Embedding(format!(
                    "embedding response indices are not unique and contiguous: expected {expected}, got {actual}"
                )));
            }
        }
    }
    Ok(indexed.into_iter().map(|(_, vec)| vec).collect())
}

pub(crate) fn validate_embedding_batch(
    vectors: Vec<Vec<f32>>,
    expected_count: usize,
    expected_dim: usize,
) -> VaultResult<Vec<Vec<f32>>> {
    if vectors.len() != expected_count {
        return Err(VaultError::Embedding(format!(
            "embedding backend returned {} vectors for {expected_count} inputs",
            vectors.len()
        )));
    }
    for vector in &vectors {
        validate_vector(vector, expected_dim)?;
    }
    Ok(vectors)
}

// ── Env var helpers (API backend) ──────────────────────────────────────

#[cfg(feature = "embeddings-api")]
fn read_env_with_fallback(primary: &str, fallback: &str) -> Option<String> {
    let read_trimmed = |var: &str| {
        std::env::var(var)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    read_trimmed(primary).or_else(|| read_trimmed(fallback))
}

#[cfg(feature = "embeddings-api")]
fn parse_usize_env(var_name: &str) -> Option<usize> {
    std::env::var(var_name).ok()?.trim().parse::<usize>().ok()
}

// ── Text preparation ───────────────────────────────────────────────────

const MAX_BODY_WORDS: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyCacheMigration {
    NotFound,
    AlreadyPresent(PathBuf),
    Migrated(PathBuf),
}

pub fn migrate_legacy_cache_to_daemon_store(
    vault_root: &Path,
    semantic_home: &Path,
) -> VaultResult<LegacyCacheMigration> {
    let vault_id = crate::daemon::home::compute_vault_id(vault_root)?;
    let target = semantic_home
        .join("vaults")
        .join(vault_id)
        .join("embeddings.bin");
    let legacy_source = vault_root
        .join(".obsidian")
        .join("obsidian-mcp")
        .join("embeddings.bin");
    let new_source = vault_root
        .join(".obsidian-mcp")
        .join("embeddings")
        .join("embeddings.bin");

    migrate_cache_candidates_to_path(&[new_source, legacy_source], &target)
}

pub(crate) fn migrate_cache_candidates_to_path(
    sources: &[PathBuf],
    target: &Path,
) -> VaultResult<LegacyCacheMigration> {
    if target.exists() {
        return Ok(LegacyCacheMigration::AlreadyPresent(target.to_path_buf()));
    }

    let Some(source) = sources.iter().find(|source| source.is_file()) else {
        return Ok(LegacyCacheMigration::NotFound);
    };

    let source_file = std::fs::File::open(source)?;
    let source_len = source_file.metadata()?.len();
    if source_len > MAX_CACHE_BYTES {
        return Err(VaultError::Embedding(format!(
            "embedding cache is too large to relocate: {source_len} bytes (limit {MAX_CACHE_BYTES})"
        )));
    }

    let parent = target.parent().ok_or_else(|| {
        VaultError::Embedding(format!(
            "embedding cache migration target has no parent: {}",
            target.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;

    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    let mut limited_source = source_file.take(MAX_CACHE_BYTES + 1);
    let copied = std::io::copy(&mut limited_source, &mut temp)?;
    if copied > MAX_CACHE_BYTES {
        return Err(VaultError::Embedding(format!(
            "embedding cache is too large to relocate: more than {MAX_CACHE_BYTES} bytes"
        )));
    }
    temp.flush()?;
    temp.as_file().sync_all()?;

    match temp.persist_noclobber(target) {
        Ok(_) => Ok(LegacyCacheMigration::Migrated(target.to_path_buf())),
        Err(error)
            if error.error.kind() == std::io::ErrorKind::AlreadyExists || target.exists() =>
        {
            Ok(LegacyCacheMigration::AlreadyPresent(target.to_path_buf()))
        }
        Err(error) => Err(VaultError::Io(error.error)),
    }
}

/// Prepare text for embedding from note components.
///
/// Format: `"{title}\n{headings joined with " | "}\n{body truncated to 400 words}"`.
/// The body should already have frontmatter stripped.
pub fn prepare_embed_text(title: &str, headings: &[String], body: &str) -> String {
    let headings_line = headings.join(" | ");

    let truncated_body: String = body
        .split_whitespace()
        .take(MAX_BODY_WORDS)
        .collect::<Vec<_>>()
        .join(" ");

    if headings_line.is_empty() {
        format!("{title}\n{truncated_body}")
    } else {
        format!("{title}\n{headings_line}\n{truncated_body}")
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── cosine_similarity ──────────────────────────────────────────

    #[test]
    fn cosine_similarity_self_is_one() {
        let v = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!(
            (sim - 1.0).abs() < 1e-6,
            "self-similarity should be 1.0, got {sim}"
        );
    }

    #[test]
    fn cosine_similarity_orthogonal_is_zero() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(
            sim.abs() < 1e-6,
            "orthogonal vectors should have similarity ~0, got {sim}"
        );
    }

    #[test]
    fn cosine_similarity_opposite_is_negative() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(
            (sim + 1.0).abs() < 1e-6,
            "opposite vectors should be -1.0, got {sim}"
        );
    }

    #[test]
    fn cosine_similarity_zero_vector_returns_zero() {
        let a = vec![1.0, 2.0];
        let zero = vec![0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &zero), 0.0);
        assert_eq!(cosine_similarity(&zero, &a), 0.0);
    }

    // ── EmbeddingStore ─────────────────────────────────────────────

    fn test_identity(dim: usize) -> EmbeddingSpaceIdentity {
        EmbeddingSpaceIdentity {
            backend: EmbeddingBackendKind::Local,
            model: "test-model".to_string(),
            endpoint_fingerprint: None,
            dimension: dim,
            input_version: EMBEDDING_INPUT_VERSION,
        }
    }

    fn make_store() -> EmbeddingStore {
        let mut store = EmbeddingStore::new_with_identity(test_identity(3));
        store
            .insert_hashed(
                PathBuf::from("a.md"),
                prepared_text_hash("a"),
                vec![1.0, 0.0, 0.0],
            )
            .unwrap();
        store
            .insert_hashed(
                PathBuf::from("b.md"),
                prepared_text_hash("b"),
                vec![0.0, 1.0, 0.0],
            )
            .unwrap();
        store
            .insert_hashed(
                PathBuf::from("c.md"),
                prepared_text_hash("c"),
                vec![0.7, 0.7, 0.0],
            )
            .unwrap();
        store.set_first_pass_complete(true);
        store
    }

    fn write_cache_data(path: &Path, data: &EmbeddingCacheData) {
        let bytes = bincode::serde::encode_to_vec(data, bincode::config::standard()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn single_entry_cache(
        identity: EmbeddingSpaceIdentity,
        path: &str,
        vector: Vec<f32>,
    ) -> EmbeddingCacheData {
        EmbeddingCacheData {
            magic: CACHE_MAGIC,
            schema_version: CACHE_SCHEMA_VERSION,
            identity,
            first_pass_complete: true,
            entries: vec![EmbeddingCacheEntry {
                path: path.to_string(),
                content_hash: prepared_text_hash("cached text"),
                vector,
            }],
        }
    }

    #[test]
    fn query_returns_top_k_sorted() {
        let store = make_store();
        let query = vec![1.0, 0.0, 0.0];
        let results = store.query(&query, 2);

        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].0,
            PathBuf::from("a.md"),
            "exact match should rank first"
        );
        assert!(
            results[0].1 > results[1].1,
            "results should be sorted by descending score"
        );
    }

    #[test]
    fn query_top_k_exceeding_store_size() {
        let store = make_store();
        let query = vec![1.0, 0.0, 0.0];
        let results = store.query(&query, 100);
        assert_eq!(results.len(), 3);
    }

    // ── chunk-level store regressions ───────────────────────────────

    /// REGRESSION: the RUNTIME loads through load_for_space, which validates the
    /// space identity and bounds entry count by NOTE count. Chunk-level stores
    /// hold many entries per note, so a budget derived from note count alone
    /// silently rejects a valid cache and the vault re-embeds on every start.
    #[test]
    fn load_for_space_accepts_a_chunk_level_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        let identity = test_identity(3);
        let mut store = EmbeddingStore::new_with_identity(identity.clone());
        let note = PathBuf::from("one.md");
        let hash = prepared_text_hash("body");
        for i in 0..40usize {
            store
                .insert_hashed(chunk_key(&note, i), hash, vec![1.0, 0.0, 0.0])
                .unwrap();
        }
        store.set_first_pass_complete(true);
        store.save(&path).unwrap();

        // one note on disk, forty chunk rows in the cache
        let loaded = EmbeddingStore::load_for_space(&path, &identity, 1)
            .expect("a chunk-level cache for 1 note must load");
        assert_eq!(loaded.len(), 40);
        assert_eq!(loaded.content_hash(&chunk_key(&note, 0)), Some(&hash));
        assert!(loaded.first_pass_complete());
    }

    // ── experimental hybrid ranking (Phase 3) ─────────────────────────

    /// REGRESSION: Phase 2 made the semantic score reach w_sum (1.25), which
    /// silently unbalanced the two pre-existing hybrid blends - they mix it with
    /// a [0,1] lexical score, so the alpha the caller sets stopped meaning what
    /// it says. The blend-safe accessor must rescale to [0,1] without reordering.
    #[test]
    fn blend_rescaling_is_monotone_and_bounded() {
        let w = DEFAULT_SUMMARY_WEIGHT;
        assert!(w > 1.0, "this test only matters while the weight exceeds 1");

        // a perfect summary match is w before rescaling, 1.0 after
        let raw = [1.0f32 * w, 0.6 * w, 0.2];
        let scaled: Vec<f32> = raw.iter().map(|s| s / w.max(1.0)).collect();
        assert!(
            scaled.iter().all(|s| *s <= 1.0 + 1e-6),
            "must land inside [0,1]"
        );

        // order is preserved - rescaling must not reorder the semantic arm
        for pair in scaled.windows(2) {
            assert!(pair[0] >= pair[1], "rescaling reordered the semantic arm");
        }
    }

    /// Hybrid ranking must be OFF unless explicitly enabled.
    #[test]
    fn lexical_weight_defaults_to_disabled() {
        if std::env::var("OBSIDIAN_LEXICAL_WEIGHT").is_err() {
            assert_eq!(lexical_weight(), 0.0);
        }
    }

    /// Unit calibration scales by the maximum and leaves the floor at zero, so
    /// a query with no lexical signal contributes nothing. Min-max would lift
    /// the weakest candidate to 0 and hand every document a flat bonus.
    #[test]
    fn unit_calibrate_scales_by_max_and_keeps_the_floor() {
        let mut v = [2.0f32, 1.0, 0.0];
        unit_calibrate(&mut v);
        assert!((v[0] - 1.0).abs() < 1e-6);
        assert!((v[1] - 0.5).abs() < 1e-6);
        assert!(v[2].abs() < 1e-6, "zero must stay zero, unlike min-max");
    }

    /// A query with no lexical hits must not produce NaN or a flat bonus.
    #[test]
    fn unit_calibrate_handles_an_all_zero_arm() {
        let mut v = [0.0f32, 0.0, 0.0];
        unit_calibrate(&mut v);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    // ── summary arm (Phase 2) ────────────────────────────────────────

    /// The summary key must be distinguishable from every chunk ordinal, and
    /// must still resolve back to its note like any other key.
    #[test]
    fn summary_key_is_distinct_from_chunk_keys() {
        let note = PathBuf::from("dir/a note.md");
        let summary = summary_key(&note);
        assert!(is_summary_key(&summary));
        assert_eq!(note_path_of(&summary), note.as_path());
        for i in 0..5 {
            let c = chunk_key(&note, i);
            assert_ne!(c, summary, "chunk {i} collided with the summary key");
            assert!(!is_summary_key(&c));
        }
    }

    /// The summary competes with the best chunk, scaled by its weight, and the
    /// combination is a MAX so it can never drag a good chunk down.
    #[test]
    fn summary_arm_is_weighted_and_never_dilutes_a_good_chunk() {
        let note = PathBuf::from("n.md");
        let query = [1.0f32, 0.0, 0.0];

        // summary matches, chunks do not -> weighted summary wins
        let mut store = EmbeddingStore::new(3);
        store
            .insert_hashed(chunk_key(&note, 0), [0u8; 32], vec![0.0, 1.0, 0.0])
            .unwrap();
        store
            .insert_hashed(summary_key(&note), [0u8; 32], vec![1.0, 0.0, 0.0])
            .unwrap();
        let scored = store.best_score_for_note(&note, &query).unwrap();
        assert!(
            (scored - DEFAULT_SUMMARY_WEIGHT).abs() < 1e-5,
            "summary should score cos*w, got {scored}"
        );

        // a perfect chunk still wins when the summary is irrelevant
        let mut store = EmbeddingStore::new(3);
        store
            .insert_hashed(chunk_key(&note, 0), [0u8; 32], vec![1.0, 0.0, 0.0])
            .unwrap();
        store
            .insert_hashed(summary_key(&note), [0u8; 32], vec![0.0, 0.0, 1.0])
            .unwrap();
        let scored = store.best_score_for_note(&note, &query).unwrap();
        assert!(
            (scored - 1.0).abs() < 1e-5,
            "the matching chunk should win, got {scored}"
        );
    }

    /// query() must weight the summary the same way best_score_for_note does,
    /// or ranking and re-ranking disagree about the same note.
    #[test]
    fn query_weights_the_summary_arm_consistently() {
        let note = PathBuf::from("n.md");
        let mut store = EmbeddingStore::new(3);
        store
            .insert_hashed(chunk_key(&note, 0), [0u8; 32], vec![0.0, 1.0, 0.0])
            .unwrap();
        store
            .insert_hashed(summary_key(&note), [0u8; 32], vec![1.0, 0.0, 0.0])
            .unwrap();
        let hits = store.query(&[1.0, 0.0, 0.0], 5);
        assert_eq!(hits.len(), 1, "one note, not one row per entry");
        assert_eq!(hits[0].0, note);
        assert!(
            (hits[0].1 - store.best_score_for_note(&note, &[1.0, 0.0, 0.0]).unwrap()).abs() < 1e-5,
            "query() and best_score_for_note() must agree"
        );
    }

    /// Deleting a note must take its summary with it.
    #[test]
    fn remove_note_drops_the_summary_too() {
        let note = PathBuf::from("gone.md");
        let mut store = EmbeddingStore::new(2);
        store
            .insert_hashed(chunk_key(&note, 0), [0u8; 32], vec![1.0, 0.0])
            .unwrap();
        store
            .insert_hashed(summary_key(&note), [0u8; 32], vec![1.0, 0.0])
            .unwrap();
        assert!(store.remove_note(&note));
        assert!(store.is_empty(), "summary vector outlived its note");
        assert!(!store.has_note(&note));
    }

    /// A note short enough to yield no chunks is still indexed via its summary.
    #[test]
    fn has_note_accepts_a_summary_only_note() {
        let note = PathBuf::from("tiny.md");
        let mut store = EmbeddingStore::new(2);
        store
            .insert_hashed(summary_key(&note), [0u8; 32], vec![1.0, 0.0])
            .unwrap();
        assert!(store.has_note(&note));
    }

    /// Prefix defaults match the benchmarked arctic configuration.
    #[test]
    fn prefixes_default_to_the_validated_configuration() {
        if std::env::var("OBSIDIAN_EMBEDDING_QUERY_PREFIX").is_err() {
            assert_eq!(query_prefix(), "query: ");
        }
        if std::env::var("OBSIDIAN_EMBEDDING_DOC_PREFIX").is_err() {
            assert_eq!(document_prefix(), "passage: ");
        }
    }

    /// REGRESSION: chunk keys embed a NUL separator (note<idx>). If the cache
    /// codec mangles or rejects that, every reload looks like a cache miss and
    /// the whole vault silently re-embeds on every start.
    #[test]
    fn chunk_keys_and_their_hashes_survive_a_cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        let identity = test_identity(3);
        let mut store = EmbeddingStore::new_with_identity(identity.clone());
        let note = PathBuf::from("dir/some note.md");
        let hash = prepared_text_hash("body");
        store
            .insert_hashed(chunk_key(&note, 0), hash, vec![1.0, 0.0, 0.0])
            .unwrap();
        store
            .insert_hashed(chunk_key(&note, 1), hash, vec![0.0, 1.0, 0.0])
            .unwrap();
        store.set_first_pass_complete(true);
        store.save(&path).unwrap();

        let loaded = EmbeddingStore::load(&path).unwrap();
        assert_eq!(loaded.len(), 2, "both chunk rows must survive");
        assert!(loaded.has_note(&note), "note must still be discoverable");
        assert_eq!(
            loaded.content_hash(&chunk_key(&note, 0)),
            Some(&hash),
            "content hash must survive so an unchanged note is not re-embedded"
        );
    }

    /// REGRESSION: the hybrid re-ranker scores one BM25 candidate at a time by
    /// NOTE path. With a chunk-level store a bare `get(note)` always misses and
    /// returned 0.0, which silently zeroed the semantic half of
    /// `alpha*BM25 + (1-alpha)*semantic` and degraded hybrid search to pure
    /// BM25 - the benchmark reproduced pure-BM25 numbers to three decimals.
    #[test]
    fn best_score_for_note_finds_chunks_not_just_whole_note_keys() {
        let mut store = EmbeddingStore::new(3);
        let note = PathBuf::from("deep.md");
        store
            .insert_hashed(chunk_key(&note, 0), [0u8; 32], vec![1.0, 0.0, 0.0])
            .unwrap();
        store
            .insert_hashed(chunk_key(&note, 1), [0u8; 32], vec![0.0, 1.0, 0.0])
            .unwrap();

        // a bare note-path lookup finds nothing - this is what regressed
        assert!(store.get(&note).is_none());

        // but the note is still scoreable, and takes its BEST chunk
        let score = store
            .best_score_for_note(&note, &[0.0, 1.0, 0.0])
            .expect("note must be scoreable from its chunks");
        assert!(
            (score - 1.0).abs() < 1e-5,
            "expected best chunk to win, got {score}"
        );
    }

    /// The score alone cannot tell a caller where in a note the match is.
    /// `best_chunk_for_note` must report the winning chunk's index so
    /// `search_semantic` can return the passage and its heading trail.
    #[test]
    fn match_for_note_reports_which_chunk_won() {
        let mut store = EmbeddingStore::new(3);
        let note = PathBuf::from("deep.md");
        store
            .insert_hashed(chunk_key(&note, 0), [0u8; 32], vec![1.0, 0.0, 0.0])
            .unwrap();
        store
            .insert_hashed(chunk_key(&note, 1), [0u8; 32], vec![0.0, 1.0, 0.0])
            .unwrap();
        store
            .insert_hashed(chunk_key(&note, 2), [0u8; 32], vec![0.0, 0.0, 1.0])
            .unwrap();

        let m = store
            .match_for_note(&note, &[0.0, 0.0, 1.0])
            .expect("note must be scoreable");
        assert_eq!(m.winner, MatchedOn::Chunk(2));
        assert_eq!(m.best_chunk.unwrap().0, 2);

        let m = store
            .match_for_note(&note, &[0.0, 1.0, 0.0])
            .expect("note must be scoreable");
        assert_eq!(m.winner, MatchedOn::Chunk(1));

        // The score must not change now that evidence rides along with it.
        let score = store.best_score_for_note(&note, &[0.0, 1.0, 0.0]).unwrap();
        assert!((score - 1.0).abs() < 1e-5, "score changed: {score}");
    }

    /// A summary win must be attributed to the summary, AND still carry the
    /// note's best chunk. Attribution and evidence are separate concerns: the
    /// agent needs to know the passage did not cause the rank, but it still
    /// wants the passage.
    #[test]
    fn summary_win_is_attributed_honestly_but_still_carries_its_best_chunk() {
        let mut store = EmbeddingStore::new(2);
        let note = PathBuf::from("summary-wins.md");
        store
            .insert_hashed(chunk_key(&note, 0), [0u8; 32], vec![1.0, 0.0])
            .unwrap();
        store
            .insert_hashed(chunk_key(&note, 1), [0u8; 32], vec![0.6, 0.8])
            .unwrap();
        store
            .insert_hashed(summary_key(&note), [0u8; 32], vec![0.0, 1.0])
            .unwrap();

        let m = store
            .match_for_note(&note, &[0.0, 1.0])
            .expect("note must be scoreable");
        assert_eq!(m.winner, MatchedOn::Summary, "the summary arm won");
        let (index, chunk_score) = m
            .best_chunk
            .expect("the best chunk must survive losing the ranking");
        assert_eq!(index, 1, "chunk 1 is the closer of the two");

        // The two arms must stay distinct and separately reportable.
        let summary_score = m.summary_score.expect("summary arm present");
        assert!(summary_score > chunk_score);
        assert!(
            (m.score - summary_score).abs() < 1e-6,
            "score is the winner"
        );
        assert!(
            (chunk_score - 0.8).abs() < 1e-5,
            "chunk score must be the RAW cosine, unweighted: {chunk_score}"
        );
    }

    /// Adding evidence must not reorder anything. `query` ranks by the same
    /// score it always did, and must agree with the per-note accessor.
    #[test]
    fn provenance_does_not_change_ranking_or_scores() {
        let mut store = EmbeddingStore::new(2);
        let a = PathBuf::from("a.md");
        let b = PathBuf::from("b.md");
        let c = PathBuf::from("c.md");
        store
            .insert_hashed(chunk_key(&a, 0), [0u8; 32], vec![1.0, 0.0])
            .unwrap();
        store
            .insert_hashed(chunk_key(&b, 0), [0u8; 32], vec![0.7071, 0.7071])
            .unwrap();
        store
            .insert_hashed(summary_key(&b), [0u8; 32], vec![0.9, 0.4359])
            .unwrap();
        store
            .insert_hashed(chunk_key(&c, 0), [0u8; 32], vec![0.0, 1.0])
            .unwrap();

        let query = [1.0, 0.0];
        let ranked = store.query(&query, 10);
        let detailed = store.query_detailed(&query, 10);

        // Same order, same scores, whichever accessor is used.
        assert_eq!(ranked.len(), detailed.len());
        for (plain, rich) in ranked.iter().zip(detailed.iter()) {
            assert_eq!(plain.0, rich.0, "order must match");
            assert!((plain.1 - rich.1.score).abs() < 1e-6, "score must match");
            // ...and both must agree with the single-note accessor.
            let single = store
                .best_score_for_note(&plain.0, &query)
                .expect("indexed note must be scoreable");
            assert!(
                (single - plain.1).abs() < 1e-6,
                "best_score_for_note disagrees with query() for {:?}",
                plain.0
            );
        }
        assert!(ranked.windows(2).all(|w| w[0].1 >= w[1].1), "descending");

        // `b` outranks `a` even though `a` is a perfect chunk match: b's
        // summary scores 0.9, and the 1.20 weight lifts it to 1.08. That is the
        // summary arm doing its job, and it is exactly why a top hit often
        // cannot attribute itself to a passage.
        assert_eq!(
            ranked[0].0, b,
            "the weighted summary arm outranks a 1.0 chunk"
        );
        let top = &detailed[0].1;
        assert_eq!(top.winner, MatchedOn::Summary);
        assert!(
            top.best_chunk.is_some(),
            "and it still reports b's best chunk as evidence"
        );
        assert!(ranked[1].0 == a && ranked[2].0 == c);
    }

    /// A chunk key must resolve back to its index, and a summary key must not
    /// masquerade as chunk 0 - that would mislabel every summary-won hit.
    #[test]
    fn chunk_index_round_trips_and_summary_is_not_a_chunk() {
        let note = PathBuf::from("notes/alpha.md");
        for i in [0usize, 1, 7, 42] {
            assert_eq!(chunk_index_of(&chunk_key(&note, i)), Some(i));
        }
        assert_eq!(chunk_index_of(&summary_key(&note)), None);
        assert_eq!(chunk_index_of(&note), None);
    }

    /// REGRESSION: status counted indexed notes with `store.get(path)`, which
    /// never matches a chunk key, so a fully indexed vault reported 0 notes.
    #[test]
    fn has_note_counts_notes_not_chunks() {
        let mut store = EmbeddingStore::new(2);
        let a = PathBuf::from("a.md");
        let b = PathBuf::from("b.md");
        for i in 0..3 {
            store
                .insert_hashed(chunk_key(&a, i), [0u8; 32], vec![1.0, 0.0])
                .unwrap();
        }
        assert!(store.has_note(&a));
        assert!(!store.has_note(&b));
        assert_eq!(store.len(), 3, "three chunks stored");
    }

    /// A note that shrinks must not leave orphaned vectors behind.
    #[test]
    fn remove_note_drops_every_chunk() {
        let mut store = EmbeddingStore::new(2);
        let note = PathBuf::from("shrink.md");
        let other = PathBuf::from("keep.md");
        for i in 0..4 {
            store
                .insert_hashed(chunk_key(&note, i), [0u8; 32], vec![1.0, 0.0])
                .unwrap();
        }
        store
            .insert_hashed(chunk_key(&other, 0), [0u8; 32], vec![0.0, 1.0])
            .unwrap();

        assert!(store.remove_note(&note));
        assert!(!store.has_note(&note));
        assert!(store.has_note(&other), "sibling notes must survive");
        assert_eq!(store.len(), 1);
    }

    /// Chunk keys must round-trip back to their note path.
    #[test]
    fn chunk_keys_round_trip_and_plain_paths_pass_through() {
        let note = PathBuf::from("dir/some note.md");
        assert_eq!(note_path_of(&chunk_key(&note, 7)), note.as_path());
        assert_eq!(note_path_of(&note), note.as_path());
    }

    /// Query results collapse chunk keys back to NOTE paths, best chunk wins.
    #[test]
    fn query_collapses_chunks_to_notes() {
        let mut store = EmbeddingStore::new(2);
        let a = PathBuf::from("a.md");
        store
            .insert_hashed(chunk_key(&a, 0), [0u8; 32], vec![0.2, 0.98])
            .unwrap();
        store
            .insert_hashed(chunk_key(&a, 1), [0u8; 32], vec![1.0, 0.0])
            .unwrap();

        let hits = store.query(&[1.0, 0.0], 10);
        assert_eq!(hits.len(), 1, "one note, not one row per chunk");
        assert_eq!(hits[0].0, a, "result must be a note path, not a chunk key");
        assert!((hits[0].1 - 1.0).abs() < 1e-5, "best chunk should win");
    }

    #[test]
    fn query_paths_ranks_only_authoritative_members() {
        let store = make_store();
        let allowed = HashSet::from([PathBuf::from("b.md"), PathBuf::from("c.md")]);
        let results = store.query_paths(&[1.0, 0.0, 0.0], &allowed, 10);

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(path, _)| allowed.contains(path)));
        assert_eq!(results[0].0, PathBuf::from("c.md"));
    }

    #[test]
    fn insert_remove_updates_results() {
        let mut store = make_store();
        assert_eq!(store.len(), 3);

        store.remove(Path::new("a.md"));
        assert_eq!(store.len(), 2);
        assert!(store.get(Path::new("a.md")).is_none());

        let query = vec![1.0, 0.0, 0.0];
        let results = store.query(&query, 10);
        assert!(!results.iter().any(|(p, _)| p == Path::new("a.md")));

        store.insert(PathBuf::from("d.md"), vec![0.9, 0.1, 0.0]);
        assert_eq!(store.len(), 3);
        let results = store.query(&query, 1);
        assert_eq!(results[0].0, PathBuf::from("d.md"));
    }

    #[test]
    fn get_returns_embedding() {
        let store = make_store();
        let vec = store.get(Path::new("a.md")).unwrap();
        assert_eq!(vec, &[1.0, 0.0, 0.0]);
        assert!(store.get(Path::new("nonexistent.md")).is_none());
    }

    #[test]
    fn persistence_roundtrip() {
        let store = make_store();
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");

        store.save(&cache_path).unwrap();
        let loaded = EmbeddingStore::load(&cache_path).unwrap();

        assert_eq!(loaded.dim(), store.dim());
        assert_eq!(loaded.len(), store.len());
        assert_eq!(loaded.identity(), store.identity());
        assert!(loaded.first_pass_complete());
        assert_eq!(
            loaded.content_hash(Path::new("a.md")),
            store.content_hash(Path::new("a.md"))
        );

        let query = vec![1.0, 0.0, 0.0];
        let original_results = store.query(&query, 3);
        let loaded_results = loaded.query(&query, 3);

        assert_eq!(original_results.len(), loaded_results.len());
        for (orig, load) in original_results.iter().zip(&loaded_results) {
            assert_eq!(orig.0, load.0);
            assert!((orig.1 - load.1).abs() < 1e-6);
        }
    }

    #[test]
    fn empty_store_query() {
        let store = EmbeddingStore::new(3);
        assert!(store.is_empty());
        let results = store.query(&[1.0, 0.0, 0.0], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn cache_rejects_trailing_bytes() {
        let store = make_store();
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");
        store.save(&cache_path).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&cache_path)
            .unwrap();
        file.write_all(b"trailing").unwrap();

        let error = EmbeddingStore::load(&cache_path).err().unwrap();
        assert!(error.to_string().contains("trailing bytes"));
    }

    #[test]
    fn cache_rejects_legacy_truncated_wrong_magic_and_wrong_schema() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");

        std::fs::write(&cache_path, b"legacy cache").unwrap();
        assert!(EmbeddingStore::load(&cache_path).is_err());

        let valid = make_store().encode_cache().unwrap();
        std::fs::write(&cache_path, &valid[..valid.len() - 1]).unwrap();
        assert!(EmbeddingStore::load(&cache_path).is_err());

        let mut wrong_magic = single_entry_cache(test_identity(3), "one.md", vec![1.0; 3]);
        wrong_magic.magic = *b"NOTCACHE";
        write_cache_data(&cache_path, &wrong_magic);
        let error = EmbeddingStore::load(&cache_path).err().unwrap();
        assert!(error.to_string().contains("legacy embedding cache"));

        let mut wrong_schema = single_entry_cache(test_identity(3), "one.md", vec![1.0; 3]);
        wrong_schema.schema_version = CACHE_SCHEMA_VERSION + 1;
        write_cache_data(&cache_path, &wrong_schema);
        let error = EmbeddingStore::load(&cache_path).err().unwrap();
        assert!(error.to_string().contains("schema version"));
    }

    #[test]
    fn cache_rejects_invalid_and_non_normalized_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");
        let invalid_paths = [
            "",
            "/absolute.md",
            "../escape.md",
            "folder/../escape.md",
            "./note.md",
            "folder\\note.md",
            "folder//note.md",
        ];

        for path in invalid_paths {
            let data = single_entry_cache(test_identity(3), path, vec![1.0; 3]);
            write_cache_data(&cache_path, &data);
            assert!(
                EmbeddingStore::load(&cache_path).is_err(),
                "cache path should be rejected: {path:?}"
            );
        }

        let oversized_path = format!("{}.md", "a".repeat(MAX_CACHE_PATH_BYTES));
        let data = single_entry_cache(test_identity(3), &oversized_path, vec![1.0; 3]);
        write_cache_data(&cache_path, &data);
        assert!(EmbeddingStore::load(&cache_path).is_err());
    }

    #[test]
    fn cache_rejects_invalid_identity_and_vector_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");

        let invalid_identities = [
            EmbeddingSpaceIdentity {
                dimension: 0,
                ..test_identity(3)
            },
            EmbeddingSpaceIdentity {
                model: "  ".to_string(),
                ..test_identity(3)
            },
            EmbeddingSpaceIdentity {
                input_version: EMBEDDING_INPUT_VERSION + 1,
                ..test_identity(3)
            },
            EmbeddingSpaceIdentity {
                endpoint_fingerprint: Some([7; 32]),
                ..test_identity(3)
            },
            EmbeddingSpaceIdentity {
                backend: EmbeddingBackendKind::Api,
                endpoint_fingerprint: None,
                ..test_identity(3)
            },
        ];
        for identity in invalid_identities {
            let dim = identity.dimension.max(1);
            let data = single_entry_cache(identity, "one.md", vec![1.0; dim]);
            write_cache_data(&cache_path, &data);
            assert!(EmbeddingStore::load(&cache_path).is_err());
        }

        let data = single_entry_cache(test_identity(3), "one.md", vec![1.0; 2]);
        write_cache_data(&cache_path, &data);
        let error = EmbeddingStore::load(&cache_path).err().unwrap();
        assert!(error.to_string().contains("dimension mismatch"));
    }

    #[test]
    fn cache_load_is_bounded_by_current_vault_and_file_size() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");
        let identity = test_identity(1);
        let entries = (0..1025)
            .map(|index| EmbeddingCacheEntry {
                path: format!("{index}.md"),
                content_hash: prepared_text_hash("cached text"),
                vector: vec![1.0],
            })
            .collect();
        write_cache_data(
            &cache_path,
            &EmbeddingCacheData {
                magic: CACHE_MAGIC,
                schema_version: CACHE_SCHEMA_VERSION,
                identity: identity.clone(),
                first_pass_complete: true,
                entries,
            },
        );
        let error = EmbeddingStore::load_for_space(&cache_path, &identity, 0)
            .err()
            .unwrap();
        assert!(error.to_string().contains("too many entries"));

        let per_entry =
            MAX_CACHE_PATH_BYTES + identity.dimension * std::mem::size_of::<f32>() + 128;
        let derived_limit = 1024usize * per_entry + 1024 * 1024;
        let file = std::fs::File::create(&cache_path).unwrap();
        file.set_len((derived_limit + 1) as u64).unwrap();
        let error = EmbeddingStore::load_for_space(&cache_path, &identity, 0)
            .err()
            .unwrap();
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn failed_encoding_preserves_previous_cache_and_success_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");
        let original = make_store();
        original.save(&cache_path).unwrap();
        let original_bytes = std::fs::read(&cache_path).unwrap();

        let mut invalid = EmbeddingStore::new_with_identity(test_identity(3));
        invalid
            .insert_hashed(
                PathBuf::from("../escape.md"),
                prepared_text_hash("bad"),
                vec![1.0, 0.0, 0.0],
            )
            .unwrap();
        assert!(invalid.save(&cache_path).is_err());
        assert_eq!(std::fs::read(&cache_path).unwrap(), original_bytes);

        let mut replacement = make_store();
        replacement
            .insert_hashed(
                PathBuf::from("a.md"),
                prepared_text_hash("replacement"),
                vec![0.0, 0.0, 1.0],
            )
            .unwrap();
        replacement.save(&cache_path).unwrap();
        let loaded = EmbeddingStore::load(&cache_path).unwrap();
        assert_eq!(loaded.get(Path::new("a.md")), Some(&[0.0, 0.0, 1.0][..]));
    }

    #[cfg_attr(
        windows,
        ignore = "PRE-EXISTING upstream failure, not a fork regression: verified \
failing identically at upstream fea2e1f. Windows denies the atomic replace \
while a concurrent reader holds the cache file open (os error 5). The test is \
meaningful on Linux/macOS and still runs there."
    )]
    #[test]
    fn concurrent_readers_observe_only_complete_atomic_cache_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");
        let original = make_store();
        original.save(&cache_path).unwrap();

        let mut replacement = make_store();
        replacement
            .insert_hashed(
                PathBuf::from("a.md"),
                prepared_text_hash("replacement"),
                vec![0.0, 0.0, 1.0],
            )
            .unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writer_path = cache_path.clone();
        let writer_start = std::sync::Arc::clone(&start);
        let writer = std::thread::spawn(move || {
            writer_start.wait();
            for iteration in 0..20 {
                if iteration % 2 == 0 {
                    replacement.save(&writer_path).unwrap();
                } else {
                    original.save(&writer_path).unwrap();
                }
            }
        });

        start.wait();
        let mut reads = 0;
        loop {
            let loaded = EmbeddingStore::load(&cache_path).unwrap();
            let vector = loaded.get(Path::new("a.md")).unwrap();
            assert!(vector == [1.0, 0.0, 0.0] || vector == [0.0, 0.0, 1.0]);
            reads += 1;
            if writer.is_finished() {
                break;
            }
        }
        writer.join().unwrap();
        assert!(reads > 0);
    }

    #[test]
    fn cache_encoding_rejects_entries_without_hashes() {
        let mut store = EmbeddingStore::new_with_identity(test_identity(3));
        store.insert(PathBuf::from("one.md"), vec![1.0, 0.0, 0.0]);
        let error = store.encode_cache().unwrap_err();
        assert!(error.to_string().contains("prepared-text hash"));
    }

    #[test]
    fn cache_rejects_wrong_vector_space() {
        let store = make_store();
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");
        store.save(&cache_path).unwrap();

        let mut expected = test_identity(3);
        expected.model = "different-model".to_string();
        let error = EmbeddingStore::load_for_space(&cache_path, &expected, 3)
            .err()
            .unwrap();
        assert!(error.to_string().contains("identity mismatch"));
    }

    #[test]
    fn every_vector_space_component_participates_in_cache_compatibility() {
        let store = make_store();
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");
        store.save(&cache_path).unwrap();

        let mut mismatches = Vec::new();
        let mut model = test_identity(3);
        model.model = "other-model".to_string();
        mismatches.push(model);
        let mut dimension = test_identity(4);
        dimension.model = "test-model".to_string();
        mismatches.push(dimension);
        let mut input = test_identity(3);
        input.input_version += 1;
        mismatches.push(input);
        mismatches.push(EmbeddingSpaceIdentity {
            backend: EmbeddingBackendKind::Api,
            model: "test-model".to_string(),
            endpoint_fingerprint: Some([1; 32]),
            dimension: 3,
            input_version: EMBEDDING_INPUT_VERSION,
        });

        for expected in mismatches {
            let error = EmbeddingStore::load_for_space(&cache_path, &expected, 3)
                .err()
                .unwrap();
            assert!(error.to_string().contains("identity mismatch"));
        }
    }

    #[test]
    fn cache_rejects_duplicate_normalized_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");
        let data = EmbeddingCacheData {
            magic: CACHE_MAGIC,
            schema_version: CACHE_SCHEMA_VERSION,
            identity: test_identity(3),
            first_pass_complete: true,
            entries: vec![
                EmbeddingCacheEntry {
                    path: "Cafe\u{301}.md".to_string(),
                    content_hash: prepared_text_hash("one"),
                    vector: vec![1.0, 0.0, 0.0],
                },
                EmbeddingCacheEntry {
                    path: "Caf\u{e9}.md".to_string(),
                    content_hash: prepared_text_hash("two"),
                    vector: vec![0.0, 1.0, 0.0],
                },
            ],
        };
        let bytes = bincode::serde::encode_to_vec(&data, bincode::config::standard()).unwrap();
        std::fs::write(&cache_path, bytes).unwrap();

        let error = EmbeddingStore::load(&cache_path).err().unwrap();
        assert!(error.to_string().contains("duplicate path"));
    }

    #[test]
    fn cache_rejects_non_finite_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.bin");
        let data = EmbeddingCacheData {
            magic: CACHE_MAGIC,
            schema_version: CACHE_SCHEMA_VERSION,
            identity: test_identity(3),
            first_pass_complete: true,
            entries: vec![EmbeddingCacheEntry {
                path: "bad.md".to_string(),
                content_hash: prepared_text_hash("bad"),
                vector: vec![1.0, f32::NAN, 0.0],
            }],
        };
        let bytes = bincode::serde::encode_to_vec(&data, bincode::config::standard()).unwrap();
        std::fs::write(&cache_path, bytes).unwrap();

        let error = EmbeddingStore::load(&cache_path).err().unwrap();
        assert!(error.to_string().contains("non-finite"));
    }

    // ── prepare_embed_text ─────────────────────────────────────────

    #[test]
    fn prepare_embed_text_truncates_body() {
        let long_body: String = (0..600)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let result = prepare_embed_text("Title", &[], &long_body);

        let word_count = result.lines().last().unwrap().split_whitespace().count();
        assert_eq!(word_count, MAX_BODY_WORDS);
    }

    #[test]
    fn prepare_embed_text_joins_headings() {
        let headings = vec!["Introduction".to_string(), "Summary".to_string()];
        let result = prepare_embed_text("My Note", &headings, "Some body text.");

        assert!(result.starts_with("My Note\n"));
        assert!(result.contains("Introduction | Summary"));
        assert!(result.ends_with("Some body text."));
    }

    #[test]
    fn prepare_embed_text_no_headings() {
        let result = prepare_embed_text("Title", &[], "Body here.");
        assert_eq!(result, "Title\nBody here.");
    }

    #[test]
    fn prepare_embed_text_short_body_unchanged() {
        let body = "Short body with a few words.";
        let result = prepare_embed_text("T", &[], body);
        assert!(result.contains(body));
    }

    #[test]
    fn migrate_legacy_cache_copies_once_and_keeps_source() {
        let vault_root = tempfile::tempdir().expect("temp vault root");
        let semantic_home = tempfile::tempdir().expect("temp semantic home");
        std::fs::create_dir_all(vault_root.path().join(".obsidian")).expect("create .obsidian");

        let source = vault_root
            .path()
            .join(".obsidian")
            .join("obsidian-mcp")
            .join("embeddings.bin");
        std::fs::create_dir_all(source.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source, b"legacy-cache-bytes").expect("write legacy cache");

        let first = migrate_legacy_cache_to_daemon_store(vault_root.path(), semantic_home.path())
            .expect("first migration should succeed");
        let migrated_path = match first {
            LegacyCacheMigration::Migrated(path) => path,
            other => panic!("expected migrated outcome, got: {other:?}"),
        };
        assert!(source.exists(), "source cache should not be deleted");
        assert!(migrated_path.exists(), "target cache should be created");
        assert_eq!(
            std::fs::read(&source).expect("read source bytes"),
            std::fs::read(&migrated_path).expect("read target bytes")
        );

        let second = migrate_legacy_cache_to_daemon_store(vault_root.path(), semantic_home.path())
            .expect("second migration should succeed");
        assert_eq!(second, LegacyCacheMigration::AlreadyPresent(migrated_path));
    }

    #[test]
    fn migrate_legacy_cache_without_source_is_noop() {
        let vault_root = tempfile::tempdir().expect("temp vault root");
        let semantic_home = tempfile::tempdir().expect("temp semantic home");
        std::fs::create_dir_all(vault_root.path().join(".obsidian")).expect("create .obsidian");

        let outcome = migrate_legacy_cache_to_daemon_store(vault_root.path(), semantic_home.path())
            .expect("migration should succeed");
        assert_eq!(outcome, LegacyCacheMigration::NotFound);
    }

    #[test]
    fn migrate_legacy_cache_rejects_oversized_source_before_copying() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("oversized.bin");
        let target = directory.path().join("target").join("embeddings.bin");
        let source_file = std::fs::File::create(&source).expect("create sparse source");
        source_file
            .set_len(MAX_CACHE_BYTES + 1)
            .expect("extend sparse source");

        let error = migrate_cache_candidates_to_path(&[source], &target)
            .expect_err("oversized migration must fail");

        assert!(error.to_string().contains("too large to relocate"));
        assert!(!target.exists(), "oversized cache must not be published");
    }

    #[test]
    fn migrate_legacy_cache_checks_daemon_store_first() {
        let vault_root = tempfile::tempdir().expect("temp vault root");
        let semantic_home = tempfile::tempdir().expect("temp semantic home");
        let vault_id = crate::daemon::home::compute_vault_id(vault_root.path()).unwrap();
        let target = semantic_home
            .path()
            .join("vaults")
            .join(vault_id)
            .join("embeddings.bin");
        std::fs::create_dir_all(target.parent().expect("target parent"))
            .expect("create target dir");
        std::fs::write(&target, b"daemon-cache-bytes").expect("write target cache");

        let outcome = migrate_legacy_cache_to_daemon_store(vault_root.path(), semantic_home.path())
            .expect("migration should succeed");

        assert_eq!(outcome, LegacyCacheMigration::AlreadyPresent(target));
    }

    #[test]
    fn migrate_legacy_cache_uses_active_local_source() {
        let vault_root = tempfile::tempdir().expect("temp vault root");
        let semantic_home = tempfile::tempdir().expect("temp semantic home");

        let new_source = vault_root
            .path()
            .join(".obsidian-mcp")
            .join("embeddings")
            .join("embeddings.bin");
        std::fs::create_dir_all(new_source.parent().expect("parent")).expect("create new dir");
        std::fs::write(&new_source, b"new-cache-bytes").expect("write new cache");

        let result = migrate_legacy_cache_to_daemon_store(vault_root.path(), semantic_home.path())
            .expect("migration should succeed");
        let migrated_path = match result {
            LegacyCacheMigration::Migrated(path) => path,
            other => panic!("expected Migrated, got: {other:?}"),
        };
        assert!(new_source.exists(), "new source should not be deleted");
        assert_eq!(
            std::fs::read(&new_source).expect("read new source"),
            std::fs::read(&migrated_path).expect("read target"),
        );
    }

    #[test]
    fn migrate_legacy_cache_prefers_active_location_over_older_legacy_location() {
        let vault_root = tempfile::tempdir().expect("temp vault root");
        let semantic_home = tempfile::tempdir().expect("temp semantic home");

        let legacy_source = vault_root
            .path()
            .join(".obsidian")
            .join("obsidian-mcp")
            .join("embeddings.bin");
        std::fs::create_dir_all(legacy_source.parent().expect("parent"))
            .expect("create legacy dir");
        std::fs::write(&legacy_source, b"legacy-bytes").expect("write legacy");

        let new_source = vault_root
            .path()
            .join(".obsidian-mcp")
            .join("embeddings")
            .join("embeddings.bin");
        std::fs::create_dir_all(new_source.parent().expect("parent")).expect("create new dir");
        std::fs::write(&new_source, b"new-bytes").expect("write new");

        let result = migrate_legacy_cache_to_daemon_store(vault_root.path(), semantic_home.path())
            .expect("migration should succeed");
        let migrated_path = match result {
            LegacyCacheMigration::Migrated(path) => path,
            other => panic!("expected Migrated, got: {other:?}"),
        };
        assert_eq!(
            std::fs::read(&migrated_path).expect("read target"),
            b"new-bytes",
            "the active local cache should be preferred over the older legacy location"
        );
    }

    #[test]
    fn concurrent_cache_migrations_publish_one_complete_source_without_overwrite() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first_source = directory.path().join("first.bin");
        let second_source = directory.path().join("second.bin");
        let target = directory.path().join("target").join("embeddings.bin");
        let first_bytes = vec![0x11; 512 * 1024];
        let second_bytes = vec![0x22; 512 * 1024];
        std::fs::write(&first_source, &first_bytes).expect("write first source");
        std::fs::write(&second_source, &second_bytes).expect("write second source");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers = [first_source, second_source]
            .into_iter()
            .map(|source| {
                let barrier = std::sync::Arc::clone(&barrier);
                let target = target.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    migrate_cache_candidates_to_path(&[source], &target)
                        .expect("migration should resolve atomically")
                })
            })
            .collect::<Vec<_>>();

        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().expect("migration worker should join"))
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, LegacyCacheMigration::Migrated(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, LegacyCacheMigration::AlreadyPresent(_)))
                .count(),
            1
        );

        let published = std::fs::read(&target).expect("read published target");
        assert!(published == first_bytes || published == second_bytes);
    }

    // ── resolve_provider ──────────────────────────────────────────

    #[test]
    fn resolve_provider_explicit_local() {
        let result = resolve_provider(Some(EmbeddingProvider::Local));
        assert_eq!(result, EmbeddingProvider::Local);
    }

    #[test]
    fn resolve_provider_explicit_api() {
        let result = resolve_provider(Some(EmbeddingProvider::Api));
        assert_eq!(result, EmbeddingProvider::Api);
    }

    #[test]
    fn resolve_provider_none_infers_from_features() {
        let result = resolve_provider(None);
        if cfg!(feature = "embeddings") {
            assert_eq!(result, EmbeddingProvider::Local);
        } else if cfg!(feature = "embeddings-api") {
            assert_eq!(result, EmbeddingProvider::Api);
        }
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn local_model_enum_and_repository_alias_have_one_canonical_identity() {
        let (enum_model, enum_identity, enum_dim) = resolve_local_model("BGESmallENV15").unwrap();
        let (repo_model, repo_identity, repo_dim) =
            resolve_local_model("BAAI/bge-small-en-v1.5").unwrap();

        assert_eq!(enum_model, repo_model);
        assert_eq!(enum_identity, "BGESmallENV15");
        assert_eq!(enum_identity, repo_identity);
        assert_eq!(enum_dim, repo_dim);
        assert_eq!(repo_dim, 384);
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn unknown_local_model_is_rejected_instead_of_falling_back() {
        let error = resolve_local_model("definitely-not-a-model").err().unwrap();
        assert!(error.to_string().contains("unknown local embedding model"));
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn ambiguous_local_repository_alias_is_rejected() {
        let error = resolve_local_model("Xenova/all-MiniLM-L12-v2")
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("ambiguous local embedding model")
        );
    }

    // ── API response parsing ──────────────────────────────────────

    #[cfg(feature = "embeddings-api")]
    mod api_response_tests {
        use super::*;
        use std::sync::{LazyLock, Mutex};

        static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

        fn with_env_lock<F: FnOnce()>(f: F) {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            f();
        }

        #[test]
        fn parse_valid_single_embedding() {
            let resp = serde_json::json!({
                "data": [{"embedding": [0.1, 0.2, 0.3]}]
            });
            let result = parse_embedding_response(&resp, 1).unwrap();
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].len(), 3);
            assert!((result[0][0] - 0.1).abs() < 1e-6);
        }

        #[test]
        fn parse_valid_multiple_embeddings() {
            let resp = serde_json::json!({
                "data": [
                    {"embedding": [0.1, 0.2]},
                    {"embedding": [0.3, 0.4]}
                ]
            });
            let result = parse_embedding_response(&resp, 2).unwrap();
            assert_eq!(result.len(), 2);
            assert_eq!(result[0], vec![0.1f32, 0.2]);
            assert_eq!(result[1], vec![0.3f32, 0.4]);
        }

        #[test]
        fn parse_missing_data_field() {
            let resp = serde_json::json!({"object": "list"});
            let err = parse_embedding_response(&resp, 1).unwrap_err();
            assert!(err.to_string().contains("missing 'data' array"));
        }

        #[test]
        fn parse_missing_embedding_in_item() {
            let resp = serde_json::json!({
                "data": [{"index": 0}]
            });
            let err = parse_embedding_response(&resp, 1).unwrap_err();
            assert!(err.to_string().contains("missing 'embedding' array"));
        }

        #[test]
        fn parse_non_numeric_value_in_vector() {
            let resp = serde_json::json!({
                "data": [{"embedding": [0.1, "bad", 0.3]}]
            });
            let err = parse_embedding_response(&resp, 1).unwrap_err();
            assert!(err.to_string().contains("non-numeric value"));
        }

        #[test]
        fn parse_reorders_by_index_field() {
            let resp = serde_json::json!({
                "data": [
                    {"index": 1, "embedding": [0.3, 0.4]},
                    {"index": 0, "embedding": [0.1, 0.2]}
                ]
            });
            let result = parse_embedding_response(&resp, 2).unwrap();
            assert_eq!(result.len(), 2);
            assert_eq!(result[0], vec![0.1f32, 0.2]);
            assert_eq!(result[1], vec![0.3f32, 0.4]);
        }

        #[test]
        fn parse_falls_back_to_array_order_without_index() {
            let resp = serde_json::json!({
                "data": [
                    {"embedding": [0.1, 0.2]},
                    {"embedding": [0.3, 0.4]}
                ]
            });
            let result = parse_embedding_response(&resp, 2).unwrap();
            assert_eq!(result[0], vec![0.1f32, 0.2]);
            assert_eq!(result[1], vec![0.3f32, 0.4]);
        }

        #[test]
        fn parse_empty_data_array() {
            let resp = serde_json::json!({"data": []});
            let result = parse_embedding_response(&resp, 0).unwrap();
            assert!(result.is_empty());
        }

        #[test]
        fn parse_empty_embedding_vector() {
            let resp = serde_json::json!({
                "data": [{"embedding": []}]
            });
            let result = parse_embedding_response(&resp, 1).unwrap();
            assert_eq!(result.len(), 1);
            assert!(result[0].is_empty());
        }

        #[test]
        fn parse_rejects_partial_response() {
            let resp = serde_json::json!({
                "data": [{"embedding": [0.1, 0.2]}]
            });
            let error = parse_embedding_response(&resp, 2).err().unwrap();
            assert!(error.to_string().contains("1 vectors for 2 inputs"));
        }

        #[test]
        fn parse_rejects_mixed_index_presence() {
            let resp = serde_json::json!({
                "data": [
                    {"index": 0, "embedding": [0.1, 0.2]},
                    {"embedding": [0.3, 0.4]}
                ]
            });
            let error = parse_embedding_response(&resp, 2).err().unwrap();
            assert!(error.to_string().contains("mixed indexed and unindexed"));
        }

        #[test]
        fn parse_rejects_duplicate_indices() {
            let resp = serde_json::json!({
                "data": [
                    {"index": 0, "embedding": [0.1, 0.2]},
                    {"index": 0, "embedding": [0.3, 0.4]}
                ]
            });
            let error = parse_embedding_response(&resp, 2).err().unwrap();
            assert!(error.to_string().contains("not unique and contiguous"));
        }

        #[test]
        fn parse_rejects_out_of_range_index() {
            let resp = serde_json::json!({
                "data": [
                    {"index": 0, "embedding": [0.1, 0.2]},
                    {"index": 2, "embedding": [0.3, 0.4]}
                ]
            });
            let error = parse_embedding_response(&resp, 2).err().unwrap();
            assert!(error.to_string().contains("out of range"));
        }

        #[test]
        fn common_validator_rejects_wrong_dimension_and_non_finite_values() {
            let wrong_dimension = validate_embedding_batch(vec![vec![0.1]], 1, 2)
                .err()
                .unwrap();
            assert!(wrong_dimension.to_string().contains("dimension mismatch"));

            let non_finite = validate_embedding_batch(vec![vec![0.1, f32::INFINITY]], 1, 2)
                .err()
                .unwrap();
            assert!(non_finite.to_string().contains("non-finite"));
        }

        #[test]
        fn api_cache_identity_uses_endpoint_fingerprint_not_plaintext_url() {
            let endpoint = "https://embedding-user:secret@example.invalid/v1/private";
            let identity = EmbeddingSpaceIdentity::api("test-model".to_string(), endpoint, 3);
            let mut store = EmbeddingStore::new_with_identity(identity);
            store
                .insert_hashed(
                    PathBuf::from("one.md"),
                    prepared_text_hash("one"),
                    vec![1.0, 0.0, 0.0],
                )
                .unwrap();
            let bytes = store.encode_cache().unwrap();

            assert!(
                !bytes
                    .windows(endpoint.len())
                    .any(|window| window == endpoint.as_bytes())
            );
            assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
        }

        #[test]
        fn api_http_error_does_not_expose_url_key_body_or_input() {
            use std::io::{Read as _, Write as _};

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let body = r#"{"error":"provider echoed sensitive note body and api-secret"}"#;
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            });
            let base_url = format!("http://{address}/secret-url-component");
            let client = build_api_client().unwrap();

            let error = embed_batch_api(
                &client,
                &base_url,
                "test-model",
                "api-secret",
                &["sensitive note body"],
            )
            .unwrap_err()
            .to_string();

            assert!(error.contains("HTTP status 400"));
            assert!(!error.contains("secret-url-component"));
            assert!(!error.contains("api-secret"));
            assert!(!error.contains("sensitive note body"));
            server.join().unwrap();
        }

        #[test]
        fn api_transport_error_does_not_expose_secret_url() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                drop(stream);
            });
            let base_url = format!("http://{address}/secret-url-component");
            let client = build_api_client().unwrap();

            let error = embed_batch_api(
                &client,
                &base_url,
                "test-model",
                "api-secret",
                &["sensitive note body"],
            )
            .unwrap_err()
            .to_string();

            assert!(error.contains("embedding API"));
            assert!(!error.contains("secret-url-component"));
            assert!(!error.contains("api-secret"));
            assert!(!error.contains("sensitive note body"));
            server.join().unwrap();
        }

        #[test]
        fn read_env_with_fallback_primary_wins() {
            with_env_lock(|| {
                unsafe {
                    std::env::set_var("TEST_PRIMARY_KEY_A", "primary_value");
                    std::env::set_var("TEST_FALLBACK_KEY_A", "fallback_value");
                }
                let result = read_env_with_fallback("TEST_PRIMARY_KEY_A", "TEST_FALLBACK_KEY_A");
                assert_eq!(result, Some("primary_value".to_string()));
                unsafe {
                    std::env::remove_var("TEST_PRIMARY_KEY_A");
                    std::env::remove_var("TEST_FALLBACK_KEY_A");
                }
            });
        }

        #[test]
        fn read_env_with_fallback_uses_fallback() {
            with_env_lock(|| {
                unsafe {
                    std::env::remove_var("TEST_PRIMARY_KEY_B");
                    std::env::set_var("TEST_FALLBACK_KEY_B", "fallback_value");
                }
                let result = read_env_with_fallback("TEST_PRIMARY_KEY_B", "TEST_FALLBACK_KEY_B");
                assert_eq!(result, Some("fallback_value".to_string()));
                unsafe {
                    std::env::remove_var("TEST_FALLBACK_KEY_B");
                }
            });
        }

        #[test]
        fn read_env_with_fallback_returns_none_when_both_missing() {
            with_env_lock(|| {
                unsafe {
                    std::env::remove_var("TEST_PRIMARY_KEY_C");
                    std::env::remove_var("TEST_FALLBACK_KEY_C");
                }
                let result = read_env_with_fallback("TEST_PRIMARY_KEY_C", "TEST_FALLBACK_KEY_C");
                assert_eq!(result, None);
            });
        }

        #[test]
        fn read_env_with_fallback_ignores_empty_primary() {
            with_env_lock(|| {
                unsafe {
                    std::env::set_var("TEST_PRIMARY_KEY_D", "  ");
                    std::env::set_var("TEST_FALLBACK_KEY_D", "valid");
                }
                let result = read_env_with_fallback("TEST_PRIMARY_KEY_D", "TEST_FALLBACK_KEY_D");
                assert_eq!(result, Some("valid".to_string()));
                unsafe {
                    std::env::remove_var("TEST_PRIMARY_KEY_D");
                    std::env::remove_var("TEST_FALLBACK_KEY_D");
                }
            });
        }

        #[test]
        fn parse_usize_env_valid() {
            with_env_lock(|| {
                unsafe {
                    std::env::set_var("TEST_DIM_VALID", "384");
                }
                assert_eq!(parse_usize_env("TEST_DIM_VALID"), Some(384));
                unsafe {
                    std::env::remove_var("TEST_DIM_VALID");
                }
            });
        }

        #[test]
        fn parse_usize_env_invalid() {
            with_env_lock(|| {
                unsafe {
                    std::env::set_var("TEST_DIM_INVALID", "not_a_number");
                }
                assert_eq!(parse_usize_env("TEST_DIM_INVALID"), None);
                unsafe {
                    std::env::remove_var("TEST_DIM_INVALID");
                }
            });
        }

        #[test]
        fn parse_usize_env_missing() {
            with_env_lock(|| {
                unsafe {
                    std::env::remove_var("TEST_DIM_MISSING");
                }
                assert_eq!(parse_usize_env("TEST_DIM_MISSING"), None);
            });
        }
    }
}
