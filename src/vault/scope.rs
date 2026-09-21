//! Named retrieval scopes: which slice of the vault a query is allowed to see.
//!
//! A scope is a set of include globs over vault-relative paths. Semantic search
//! restricts its candidate set to the notes a scope matches, which makes one
//! vault behave as several independent indexes without embedding anything twice.
//!
//! **Why filtering is equivalent to separate indexes here, and not a shortcut.**
//! A semantic score is `cosine(query_vector, note_vector)`. Nothing in the
//! ranking path is corpus-relative: `collapse_to_notes` scores each note on its
//! own, the summary arm applies a constant weight, and `rank_detailed` is a
//! plain sort. Remove a note from the candidate set and no surviving note's
//! score changes. So the top-k of a scope is exactly the top-k a separate index
//! over those notes would return — identical results, one set of vectors, one
//! reconcile loop.
//!
//! That equivalence is specific to the semantic arm. BM25 *is* corpus-relative
//! (idf depends on document frequency across the index), so the same claim must
//! not be made for the lexical path.
//!
//! Glob semantics are deliberately identical to `.obsidian-mcp/ignore`: the same
//! compiler, the same trailing-`/` normalization, the same match against the
//! vault-relative path. One mental model for both files.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{VaultError, VaultResult};

use super::exclude::ExcludeSet;

/// The implicit scope covering every indexed note. Cannot be redefined.
pub const ALL_SCOPE: &str = "all";

/// How deep `+other` references may nest before we call it a cycle.
const MAX_REFERENCE_DEPTH: usize = 16;

/// One named slice of the vault.
#[derive(Debug)]
struct Scope {
    /// Fully resolved patterns, including those pulled in by `+other`.
    patterns: Vec<String>,
    /// The same patterns compiled. `ExcludeSet` is a glob matcher; here its
    /// verdict is read as "included", which is why the call sites below say
    /// `is_excluded` and mean the opposite.
    globs: ExcludeSet,
}

/// Every scope defined for a vault, resolved and compiled.
#[derive(Debug, Default)]
pub struct ScopeSet {
    scopes: BTreeMap<String, Scope>,
}

/// One scope block as it was written, before `+other` references are resolved.
#[derive(Default)]
struct RawScope {
    patterns: Vec<String>,
    references: Vec<String>,
}

impl ScopeSet {
    /// Parse and compile scope definitions from one or more file contents.
    ///
    /// Passing several strings merges them: blocks with the same name
    /// accumulate, exactly as the two `ignore` locations merge.
    ///
    /// An unknown or circular `+other` reference is a hard error rather than a
    /// skipped line. A scope that silently resolves to fewer paths than it was
    /// written to cover would return short result sets that look like genuine
    /// absence of matches, and nothing downstream could tell the difference.
    pub fn build(sources: &[String]) -> VaultResult<Self> {
        let raw = parse_sources(sources)?;

        let mut scopes = BTreeMap::new();
        for name in raw.keys() {
            let mut written = Vec::new();
            let mut visiting = Vec::new();
            resolve_into(name, &raw, &mut written, &mut visiting)?;

            if written.is_empty() {
                return Err(VaultError::Other(format!(
                    "scope '{name}' matches no paths: it has no patterns of its own \
                     and none of the scopes it includes do either"
                )));
            }

            // The compiled set is the source of truth for what a scope covers,
            // because it is what actually matches — and because these patterns
            // are handed verbatim to the semantic daemon, which has no scopes
            // file to re-derive them from. Reporting the text as written would
            // show `Notes/` while `Notes/**` did the matching, and the two
            // would drift the moment the normalization rule changed.
            let globs = ExcludeSet::build_strict(written)?;
            let mut patterns = globs.patterns().to_vec();
            patterns.sort();
            patterns.dedup();

            scopes.insert(name.clone(), Scope { patterns, globs });
        }

        Ok(Self { scopes })
    }

    /// True when no scopes are configured, so every query sees the whole index.
    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    /// Defined scope names, sorted. Does not include the implicit `all`.
    pub fn names(&self) -> Vec<&str> {
        self.scopes.keys().map(String::as_str).collect()
    }

    /// True when `name` is defined, or is the implicit `all`.
    pub fn is_known(&self, name: &str) -> bool {
        name == ALL_SCOPE || self.scopes.contains_key(name)
    }

    /// The resolved glob patterns behind a scope, for diagnostics and for
    /// handing to the semantic daemon, which has no scopes file of its own.
    pub fn patterns(&self, name: &str) -> Option<&[String]> {
        self.scopes.get(name).map(|scope| scope.patterns.as_slice())
    }

    /// Whether a vault-relative path falls inside a scope.
    ///
    /// `all` contains everything. An undefined name contains nothing — callers
    /// should reject it via [`Self::require`] before asking.
    pub fn contains(&self, name: &str, path: &Path) -> bool {
        if name == ALL_SCOPE {
            return true;
        }
        self.scopes
            .get(name)
            .is_some_and(|scope| scope.globs.is_excluded(path))
    }

    /// Reject an unknown scope name with a message that lists the real ones.
    pub fn require(&self, name: &str) -> VaultResult<()> {
        if self.is_known(name) {
            return Ok(());
        }
        let mut known = vec![ALL_SCOPE.to_string()];
        known.extend(self.names().into_iter().map(str::to_string));
        Err(VaultError::UnknownScope {
            name: name.to_string(),
            known: known.join(", "),
        })
    }

    /// Narrow a set of candidate paths to those inside `name`.
    ///
    /// Returns the input untouched for `all`, so the common case allocates
    /// nothing beyond the move.
    pub fn narrow(&self, name: &str, paths: HashSet<PathBuf>) -> VaultResult<HashSet<PathBuf>> {
        self.require(name)?;
        if name == ALL_SCOPE {
            return Ok(paths);
        }
        Ok(paths
            .into_iter()
            .filter(|path| self.contains(name, path))
            .collect())
    }
}

/// Split every source into `[name]` blocks, merging blocks that repeat.
fn parse_sources(sources: &[String]) -> VaultResult<BTreeMap<String, RawScope>> {
    let mut raw: BTreeMap<String, RawScope> = BTreeMap::new();

    for source in sources {
        let mut current: Option<String> = None;

        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if let Some(header) = trimmed
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
            {
                let name = header.trim();
                if name.is_empty() {
                    return Err(VaultError::Other(
                        "scopes file has a block with an empty name: []".into(),
                    ));
                }
                if name == ALL_SCOPE {
                    return Err(VaultError::Other(format!(
                        "scopes file defines '[{ALL_SCOPE}]', which is reserved: \
                         '{ALL_SCOPE}' always means every indexed note and cannot be narrowed"
                    )));
                }
                raw.entry(name.to_string()).or_default();
                current = Some(name.to_string());
                continue;
            }

            let Some(name) = current.as_ref() else {
                return Err(VaultError::Other(format!(
                    "scopes file has '{trimmed}' before any [name] block — \
                     every pattern must belong to a scope"
                )));
            };

            let entry = raw.entry(name.clone()).or_default();
            if let Some(reference) = trimmed.strip_prefix('+') {
                let reference = reference.trim();
                if reference.is_empty() {
                    return Err(VaultError::Other(format!(
                        "scope '{name}' has a '+' with no scope name after it"
                    )));
                }
                entry.references.push(reference.to_string());
            } else {
                entry.patterns.push(trimmed.to_string());
            }
        }
    }

    Ok(raw)
}

/// Flatten one scope's own patterns plus everything its `+` references reach.
fn resolve_into(
    name: &str,
    raw: &BTreeMap<String, RawScope>,
    out: &mut Vec<String>,
    visiting: &mut Vec<String>,
) -> VaultResult<()> {
    if visiting.iter().any(|seen| seen == name) {
        visiting.push(name.to_string());
        return Err(VaultError::Other(format!(
            "scopes file has a circular include: {}",
            visiting.join(" -> ")
        )));
    }
    if visiting.len() >= MAX_REFERENCE_DEPTH {
        return Err(VaultError::Other(format!(
            "scope '{name}' nests '+' includes more than {MAX_REFERENCE_DEPTH} deep"
        )));
    }

    let Some(scope) = raw.get(name) else {
        let known = raw.keys().cloned().collect::<Vec<_>>().join(", ");
        return Err(VaultError::Other(format!(
            "scope '{name}' is included with '+{name}' but is never defined \
             (defined scopes: {known})"
        )));
    };

    visiting.push(name.to_string());
    out.extend(scope.patterns.iter().cloned());
    for reference in &scope.references {
        resolve_into(reference, raw, out, visiting)?;
    }
    visiting.pop();

    Ok(())
}

/// Read and merge scope definitions from both config locations.
///
/// Mirrors [`super::exclude::load_ignore_patterns`]: `{mcp_home}/scopes` is the
/// vault-portable definition, `{mcp_data}/scopes` the machine-specific one.
pub fn load_scope_sources(mcp_home: &Path, mcp_data: &Path) -> Vec<String> {
    let mut sources = Vec::new();

    if let Ok(content) = std::fs::read_to_string(mcp_home.join("scopes")) {
        sources.push(content);
    }

    if mcp_data != mcp_home
        && let Ok(content) = std::fs::read_to_string(mcp_data.join("scopes"))
    {
        sources.push(content);
    }

    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(source: &str) -> ScopeSet {
        ScopeSet::build(&[source.to_string()]).expect("scopes should compile")
    }

    fn err(source: &str) -> String {
        ScopeSet::build(&[source.to_string()])
            .expect_err("scopes should fail to compile")
            .to_string()
    }

    // ── parsing ──

    #[test]
    fn empty_source_defines_nothing() {
        let scopes = build("");
        assert!(scopes.is_empty());
        assert!(scopes.names().is_empty());
    }

    #[test]
    fn comments_and_blanks_are_ignored() {
        let scopes = build("# a comment\n\n[knowledge]\n\n# another\nNotes/\n");
        assert_eq!(scopes.names(), vec!["knowledge"]);
        assert_eq!(scopes.patterns("knowledge").unwrap(), &["Notes/**"]);
    }

    #[test]
    fn trailing_slash_normalizes_like_ignore_patterns() {
        let scopes = build("[knowledge]\nMy Notes Vault/\n");
        assert_eq!(
            scopes.patterns("knowledge").unwrap(),
            &["My Notes Vault/**"]
        );
    }

    #[test]
    fn folder_names_with_spaces_match() {
        let scopes = build("[knowledge]\nMy Notes Vault/\n");
        assert!(scopes.contains("knowledge", Path::new("My Notes Vault/Sparks/note.md")));
        assert!(!scopes.contains("knowledge", Path::new("Agent Vault/note.md")));
    }

    #[test]
    fn a_repeated_block_accumulates() {
        let scopes = build("[knowledge]\nA/\n\n[other]\nB/\n\n[knowledge]\nC/\n");
        let patterns = scopes.patterns("knowledge").unwrap();
        assert_eq!(patterns, &["A/**", "C/**"]);
    }

    #[test]
    fn blocks_merge_across_sources() {
        let scopes = ScopeSet::build(&["[knowledge]\nA/\n".into(), "[knowledge]\nB/\n".into()])
            .expect("scopes should compile");
        assert_eq!(scopes.patterns("knowledge").unwrap(), &["A/**", "B/**"]);
    }

    #[test]
    fn duplicate_patterns_collapse() {
        let scopes = build("[knowledge]\nA/\nA/\n");
        assert_eq!(scopes.patterns("knowledge").unwrap(), &["A/**"]);
    }

    // ── references ──

    #[test]
    fn a_reference_unions_another_scope() {
        let scopes = build("[a]\nA/\n\n[b]\nB/\n\n[both]\n+a\n+b\n");
        assert_eq!(scopes.patterns("both").unwrap(), &["A/**", "B/**"]);
        assert!(scopes.contains("both", Path::new("A/note.md")));
        assert!(scopes.contains("both", Path::new("B/note.md")));
        assert!(!scopes.contains("both", Path::new("C/note.md")));
    }

    #[test]
    fn a_reference_composes_with_own_patterns() {
        let scopes = build("[a]\nA/\n\n[wide]\n+a\nC/\n");
        assert_eq!(scopes.patterns("wide").unwrap(), &["A/**", "C/**"]);
    }

    #[test]
    fn nested_references_resolve_transitively() {
        let scopes = build("[a]\nA/\n\n[b]\n+a\nB/\n\n[c]\n+b\nC/\n");
        assert_eq!(scopes.patterns("c").unwrap(), &["A/**", "B/**", "C/**"]);
    }

    #[test]
    fn an_unknown_reference_is_an_error_not_a_narrower_scope() {
        let message = err("[both]\n+knowledge\n");
        assert!(message.contains("is never defined"), "{message}");
    }

    #[test]
    fn a_direct_cycle_is_rejected() {
        let message = err("[a]\n+a\n");
        assert!(message.contains("circular include"), "{message}");
    }

    #[test]
    fn an_indirect_cycle_is_rejected() {
        let message = err("[a]\n+b\n\n[b]\n+a\n");
        assert!(message.contains("circular include"), "{message}");
    }

    #[test]
    fn a_plus_with_no_name_is_rejected() {
        let message = err("[a]\n+\n");
        assert!(message.contains("no scope name"), "{message}");
    }

    // ── malformed input ──

    #[test]
    fn a_pattern_before_any_block_is_rejected() {
        let message = err("Notes/\n[a]\nA/\n");
        assert!(message.contains("before any [name] block"), "{message}");
    }

    #[test]
    fn an_empty_block_name_is_rejected() {
        let message = err("[]\nA/\n");
        assert!(message.contains("empty name"), "{message}");
    }

    #[test]
    fn redefining_all_is_rejected() {
        let message = err("[all]\nA/\n");
        assert!(message.contains("reserved"), "{message}");
    }

    #[test]
    fn a_scope_that_resolves_to_nothing_is_rejected() {
        let message = err("[empty]\n");
        assert!(message.contains("matches no paths"), "{message}");
    }

    // ── the implicit `all` scope ──

    #[test]
    fn all_is_known_even_with_no_scopes_file() {
        let scopes = ScopeSet::default();
        assert!(scopes.is_known(ALL_SCOPE));
        assert!(scopes.require(ALL_SCOPE).is_ok());
    }

    #[test]
    fn all_contains_every_path() {
        let scopes = build("[a]\nA/\n");
        assert!(scopes.contains(ALL_SCOPE, Path::new("anything/at/all.md")));
    }

    #[test]
    fn all_narrows_nothing() {
        let scopes = build("[a]\nA/\n");
        let paths = HashSet::from([PathBuf::from("A/x.md"), PathBuf::from("B/y.md")]);
        let narrowed = scopes.narrow(ALL_SCOPE, paths.clone()).unwrap();
        assert_eq!(narrowed, paths);
    }

    // ── narrowing and rejection ──

    #[test]
    fn narrow_keeps_only_matching_paths() {
        let scopes = build("[a]\nA/\n");
        let paths = HashSet::from([
            PathBuf::from("A/x.md"),
            PathBuf::from("A/deep/y.md"),
            PathBuf::from("B/z.md"),
        ]);
        let narrowed = scopes.narrow("a", paths).unwrap();
        assert_eq!(
            narrowed,
            HashSet::from([PathBuf::from("A/x.md"), PathBuf::from("A/deep/y.md")])
        );
    }

    #[test]
    fn an_unknown_scope_is_rejected_and_lists_the_real_ones() {
        let scopes = build("[knowledge]\nA/\n\n[agent]\nB/\n");
        let message = scopes
            .require("knowlege")
            .expect_err("typo should be rejected")
            .to_string();
        assert!(message.contains("knowlege"), "{message}");
        assert!(message.contains("knowledge"), "{message}");
        assert!(message.contains("agent"), "{message}");
        assert!(message.contains(ALL_SCOPE), "{message}");
    }

    #[test]
    fn narrow_rejects_an_unknown_scope_rather_than_returning_everything() {
        let scopes = build("[a]\nA/\n");
        let paths = HashSet::from([PathBuf::from("A/x.md")]);
        assert!(scopes.narrow("nope", paths).is_err());
    }

    #[test]
    fn an_undefined_scope_contains_nothing() {
        let scopes = build("[a]\nA/\n");
        assert!(!scopes.contains("nope", Path::new("A/x.md")));
    }

    // ── loading ──

    #[test]
    fn load_scope_sources_reads_both_locations() {
        let home = tempfile::TempDir::new().unwrap();
        let data = tempfile::TempDir::new().unwrap();
        std::fs::write(home.path().join("scopes"), "[a]\nA/\n").unwrap();
        std::fs::write(data.path().join("scopes"), "[b]\nB/\n").unwrap();

        let sources = load_scope_sources(home.path(), data.path());
        let scopes = ScopeSet::build(&sources).unwrap();
        assert_eq!(scopes.names(), vec!["a", "b"]);
    }

    #[test]
    fn load_scope_sources_reads_one_location_once() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("scopes"), "[a]\nA/\n").unwrap();

        let sources = load_scope_sources(dir.path(), dir.path());
        assert_eq!(sources.len(), 1);
    }

    #[test]
    fn load_scope_sources_missing_file_is_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(load_scope_sources(dir.path(), dir.path()).is_empty());
    }
}
