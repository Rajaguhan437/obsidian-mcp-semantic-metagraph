//! Per-vault daemon runtime context (index, semantic state, watcher).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use notify_debouncer_mini::Debouncer;

use crate::error::{VaultError, VaultResult};
use crate::models::NoteMetadata;
use crate::vault::exclude::ExcludeSet;
use crate::vault::index::VaultIndex;
use crate::vault::tantivy_index::TantivyIndex;

#[cfg(has_embeddings)]
use crate::vault::embedding_runtime::{EmbeddingRuntime, EmbeddingRuntimeStatus};
#[cfg(has_embeddings)]
use crate::vault::embeddings::Embedder;

use super::watcher;

#[cfg(has_embeddings)]
pub(crate) type EmbeddingLoaderFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = VaultResult<Arc<dyn Embedder>>> + Send + 'static>,
>;

pub struct VaultContext {
    vault_id: String,
    vault_root: PathBuf,
    model_name: String,
    index: Arc<RwLock<VaultIndex>>,
    tantivy: Arc<TantivyIndex>,
    /// Held so the watcher filters incoming changes the same way the initial
    /// build did. Rebuilding it empty there would quietly re-admit excluded
    /// notes on the first edit inside an excluded folder.
    exclude: Arc<ExcludeSet>,
    #[cfg(has_embeddings)]
    embedding_runtime: EmbeddingRuntime,
    watcher: Mutex<Option<Debouncer<notify::RecommendedWatcher>>>,
}

/// Exclusion patterns for a vault held by the daemon.
///
/// This used to be hardcoded empty, which made the daemon's view of a vault
/// disagree with the server's. Under `OBSIDIAN_SEMANTIC_MODE=auto` the daemon
/// answers semantic queries while the server answers lexical ones, so excluded
/// folders were absent from `search_text` and still present in
/// `search_semantic` — the kind of split-brain that reads as a retrieval
/// oddity rather than a configuration bug.
///
/// The sources are the same two `Vault::open` uses: the vault's `ignore` file
/// and `OBSIDIAN_EXCLUDE_PATHS`. The daemon inherits its environment from the
/// server that spawned it, so the two agree by construction.
///
/// Caveat worth knowing: a daemon is keyed by vault path alone, so two servers
/// pointed at one vault with *different* exclusions would share whichever set
/// was registered first.
fn exclusion_patterns(vault_root: &Path) -> Vec<String> {
    let mcp_home = vault_root.join(".obsidian-mcp");
    let mut patterns = crate::vault::exclude::load_ignore_patterns(&mcp_home, &mcp_home);

    patterns.extend(
        std::env::var("OBSIDIAN_EXCLUDE_PATHS")
            .unwrap_or_default()
            .split(',')
            .map(|raw| raw.trim().to_string())
            .filter(|raw| !raw.is_empty()),
    );

    patterns.sort();
    patterns.dedup();
    patterns
}

impl VaultContext {
    pub(crate) async fn open(
        vault_id: String,
        vault_root: PathBuf,
        model_name: String,
        state_dir: PathBuf,
        watch_enabled: bool,
        #[cfg(has_embeddings)] embedding_loader: EmbeddingLoaderFuture,
    ) -> VaultResult<Self> {
        std::fs::create_dir_all(&state_dir)?;

        let exclude = Arc::new(ExcludeSet::build(exclusion_patterns(&vault_root))?);
        if !exclude.is_empty() {
            tracing::info!(patterns = ?exclude.patterns(), "daemon path exclusion active");
        }

        let index = Arc::new(RwLock::new(
            VaultIndex::build(&vault_root, Arc::clone(&exclude)).await?,
        ));
        let tantivy = {
            let index_guard = index
                .read()
                .map_err(|err| VaultError::Other(format!("daemon index lock poisoned: {err}")))?;
            TantivyIndex::build(&vault_root, index_guard.notes())?
        };
        let tantivy = Arc::new(tantivy);

        #[cfg(has_embeddings)]
        let embedding_runtime = {
            let cache_migration_sources = vec![
                vault_root
                    .join(".obsidian-mcp")
                    .join("embeddings")
                    .join("embeddings.bin"),
                vault_root
                    .join(".obsidian")
                    .join("obsidian-mcp")
                    .join("embeddings.bin"),
            ];
            EmbeddingRuntime::spawn_with_cache_sources(
                vault_root.clone(),
                Arc::clone(&index),
                state_dir.join("embeddings.bin"),
                cache_migration_sources,
                embedding_loader,
            )
        };

        let context = Self {
            vault_id,
            vault_root,
            model_name,
            index,
            tantivy,
            exclude,
            #[cfg(has_embeddings)]
            embedding_runtime,
            watcher: Mutex::new(None),
        };

        if watch_enabled {
            context.ensure_watcher()?;
        }

        Ok(context)
    }

    pub fn vault_id(&self) -> &str {
        &self.vault_id
    }

    pub fn vault_root(&self) -> &Path {
        &self.vault_root
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn watch_enabled(&self) -> VaultResult<bool> {
        let guard = self
            .watcher
            .lock()
            .map_err(|err| VaultError::Other(format!("daemon watcher lock poisoned: {err}")))?;
        Ok(guard.is_some())
    }

    pub fn ensure_watcher(&self) -> VaultResult<bool> {
        let mut guard = self
            .watcher
            .lock()
            .map_err(|err| VaultError::Other(format!("daemon watcher lock poisoned: {err}")))?;

        if guard.is_some() {
            return Ok(true);
        }

        #[cfg(has_embeddings)]
        let debouncer = watcher::start_watcher(
            self.vault_root.clone(),
            Arc::clone(&self.index),
            Some(Arc::clone(&self.tantivy)),
            self.embedding_runtime.clone(),
            Arc::clone(&self.exclude),
        )?;

        #[cfg(not(has_embeddings))]
        let debouncer = watcher::start_watcher(
            self.vault_root.clone(),
            Arc::clone(&self.index),
            Some(Arc::clone(&self.tantivy)),
            Arc::clone(&self.exclude),
        )?;

        *guard = Some(debouncer);
        Ok(true)
    }

    pub fn note_metadata(&self, path: &Path) -> VaultResult<Option<NoteMetadata>> {
        let actual_path = match self.canonical_existing_relative_path(path) {
            Ok(path) => path,
            Err(VaultError::NoteNotFound(_)) => return Ok(None),
            Err(err) => return Err(err),
        };
        let guard = self
            .index
            .read()
            .map_err(|err| VaultError::Other(format!("daemon index lock poisoned: {err}")))?;
        Ok(guard.get_note(&actual_path).cloned())
    }

    pub fn read_note(&self, path: &Path) -> VaultResult<String> {
        crate::vault::fs::read_file(&self.vault_root, path)
    }

    pub fn canonical_existing_relative_path(&self, path: &Path) -> VaultResult<PathBuf> {
        Ok(crate::vault::path::resolve_existing(&self.vault_root, path)?.relative)
    }

    pub fn search_bm25(&self, query: &str, top_k: usize) -> VaultResult<Vec<(PathBuf, f32)>> {
        self.tantivy.search(query, top_k)
    }

    #[cfg(has_embeddings)]
    pub fn search_semantic_scores(
        &self,
        query: &str,
        top_k: usize,
    ) -> VaultResult<Vec<(PathBuf, f32)>> {
        let current_paths = self.indexed_paths()?;
        self.embedding_runtime
            .query_snapshot()?
            .semantic_scores_for_paths(query, &current_paths, top_k)
    }

    /// As [`Self::search_semantic_scores`], but reports which representation
    /// matched so the caller can resolve it to a passage.
    ///
    /// The daemon exposed only note-level scores, which is why a daemon-served
    /// `search_semantic` returned no provenance while the in-process path
    /// returned it in full. The store always knew; nothing asked it.
    // `pub(crate)`, not `pub`: `NoteMatch` is a crate-internal type, and a
    // public method returning it would leak it into the public surface.
    #[cfg(has_embeddings)]
    pub(crate) fn search_semantic_hits(
        &self,
        query: &str,
        top_k: usize,
    ) -> VaultResult<Vec<(PathBuf, crate::vault::embeddings::NoteMatch)>> {
        let current_paths = self.indexed_paths()?;
        self.embedding_runtime
            .query_snapshot()?
            .semantic_hits_for_paths(query, &current_paths, top_k)
    }

    #[cfg(has_embeddings)]
    fn indexed_paths(&self) -> VaultResult<std::collections::HashSet<PathBuf>> {
        Ok(self
            .index
            .read()
            .map_err(|error| VaultError::Other(format!("daemon index lock poisoned: {error}")))?
            .notes()
            .keys()
            .cloned()
            .collect())
    }

    #[cfg(has_embeddings)]
    pub fn search_hybrid_scores(
        &self,
        query: &str,
        bm25_hits: &[(PathBuf, f32)],
        alpha: f32,
        top_k: usize,
    ) -> VaultResult<Vec<(PathBuf, f32)>> {
        let snapshot = self.embedding_runtime.query_snapshot()?;
        let query_embedding = snapshot.embed_query(query)?;
        let normalized = crate::vault::search_utils::normalize_bm25_scores(bm25_hits);
        let mut combined = normalized
            .into_iter()
            .map(|(path, normalized_bm25)| {
                let semantic = snapshot.blend_score_for(&path, &query_embedding);
                let score = alpha * normalized_bm25 + (1.0 - alpha) * semantic;
                (path, score)
            })
            .collect::<Vec<_>>();
        combined.sort_unstable_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        combined.truncate(top_k);
        Ok(combined)
    }

    #[cfg(has_embeddings)]
    pub fn embedding_status(&self) -> EmbeddingRuntimeStatus {
        self.embedding_runtime.status()
    }

    #[cfg(not(has_embeddings))]
    pub fn search_semantic_scores(
        &self,
        _query: &str,
        _top_k: usize,
    ) -> VaultResult<Vec<(PathBuf, f32)>> {
        Err(VaultError::Embedding(
            "daemon binary compiled without embeddings feature".to_string(),
        ))
    }

    #[cfg(not(has_embeddings))]
    pub fn search_hybrid_scores(
        &self,
        _query: &str,
        _bm25_hits: &[(PathBuf, f32)],
        _alpha: f32,
        _top_k: usize,
    ) -> VaultResult<Vec<(PathBuf, f32)>> {
        Err(VaultError::Embedding(
            "daemon binary compiled without embeddings feature".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `ignore` file half of `exclusion_patterns`.
    ///
    /// The `OBSIDIAN_EXCLUDE_PATHS` half is deliberately not unit-tested here:
    /// `set_var` is unsafe in this edition and the variable is process-global,
    /// so a test that sets it would race `config::Config::load` running on
    /// another thread. Its wiring is covered end-to-end instead — the daemon
    /// logs `daemon path exclusion active` with the resolved patterns.
    #[test]
    fn exclusion_patterns_read_the_vault_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_home = dir.path().join(".obsidian-mcp");
        std::fs::create_dir_all(&mcp_home).unwrap();
        std::fs::write(
            mcp_home.join("ignore"),
            "Archive/\n# comment\n\nDrafts/**\n",
        )
        .unwrap();

        let patterns = exclusion_patterns(dir.path());

        assert!(patterns.contains(&"Archive/".to_string()));
        assert!(patterns.contains(&"Drafts/**".to_string()));
        assert!(
            !patterns.iter().any(|p| p.starts_with('#')),
            "comments must not become patterns"
        );
    }

    #[test]
    fn exclusion_patterns_are_empty_for_a_vault_without_an_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(exclusion_patterns(dir.path()).is_empty());
    }
}
