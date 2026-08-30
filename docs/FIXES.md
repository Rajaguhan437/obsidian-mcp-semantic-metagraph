# Significant fixes over upstream

Every fix below has a regression test. Ordered by impact.

Upstream base: `lstpsche/obsidian-mcp` v2.5.0 (`fea2e1f`).

---

## 1. Whole-note embedding truncated at 400 words

**Upstream:** `prepare_embed_text` embedded `"{title}\n{headings}\n{body}"` with
the body cut to `MAX_BODY_WORDS = 400`, one vector per note. On a 416-note vault,
**170 notes (41%) exceeded that, and 73.8% of all text sat past the cut.**

The cap carried no stated rationale anywhere — not in the code, not in the 65 KB
`CLAUDE.md` that documents comparable constants, not in `docs/`. Most likely a
stale guard for the default `bge-small-en-v1.5` (512 tokens); the tell is that it
was **model-independent**, applying equally to models with 8192-token contexts.

Note the lexical index was never truncated (`ts.f_body => body`), so this affected
semantic search only — but hybrid weighted semantic at 0.75, so the dominant
signal came from a truncated document.

**Fix:** heading-aware chunking (`src/vault/chunker.rs`), plus a summary vector
preserving the original representation. **Deep-content nDCG 0.552 → 0.941.**

**Tests:** `long_note_is_not_truncated`, `breadcrumbs_track_heading_hierarchy`,
`hash_inside_code_fence_is_not_a_heading`, `no_chunk_exceeds_hard_max`,
`utf8_multibyte_is_never_split_mid_char`, `chunking_always_progresses`.

## 2. Cache wiped on every startup

**Introduced by the chunk-level change, caught by a failing fixture.**
`retain_paths()` kept entries whose *key* appeared in the set of note paths — but
entries are keyed by chunk (`note\0<idx>`), so **nothing ever matched** and the
entire cache was discarded at every start, silently re-embedding the whole vault.
Indistinguishable from a slow first run.

**Fix:** resolve each key back to its note before the membership test.

**Test:** `has_note_counts_notes_not_chunks`, plus warm-restart verification —
3.2 s with zero documents re-embedded.

## 3. Per-note scoring returned 0.0 for every note

`score_for()` did `store.get(note_path)`, which never matches a chunk key, so it
returned `0.0` for everything. That **silently zeroed the semantic half** of
`alpha*BM25 + (1-alpha)*semantic`, reducing hybrid search to pure BM25.

Detected because benchmark results matched a pure-BM25 baseline to three decimal
places — and confirmed by a `method is never used` warning showing the intended
fix had never been applied (a patch had aborted on an unrelated conflict).

**Fix:** `best_score_for_note()` walks the note's chunks.

**Test:** `best_score_for_note_finds_chunks_not_just_whole_note_keys`.

## 4. Notes with non-mapping frontmatter dropped entirely

A note beginning `---` then `## title:` is a YAML *comment*, so its frontmatter
parsed to `null`. Upstream treated that as a hard error and **skipped the whole
note** — removing it from the lexical index, the semantic index, **and the link
graph**.

Because the link resolver is built from indexed notes only, links pointing at
those notes resolved to nothing and were reported **broken**, and their own
outgoing links never existed. On the development vault: **10 notes lost, 42 graph
edges destroyed** (10 falsely-broken inbound links, 32 invisible outgoing links).

**Fix:** non-mapping frontmatter degrades to an empty mapping with a warning; the
note stays indexed.

**Test:** `non_mapping_frontmatter_keeps_note_in_the_graph` — asserts the note is
indexed, its outgoing links exist, inbound links resolve into backlinks, and no
link to it is reported broken.

## 5. Content hash computed after truncation

The cache key was `SHA-256` of the *already-truncated* embed text. Editing
anything past word 400 left the hash unchanged, so **the note was never
re-embedded**.

**Fix:** `note_content_hash()` covers the full body plus the chunk configuration,
and is shared by production and test fixtures so they cannot drift apart.

## 6. Semantic score escaped [0,1] and unbalanced both hybrid blends

**Introduced by the summary arm.** With any `w_sum` above 1.0 (1.25 at the time;
1.20 today), `score_for` can exceed 1.0, but both hybrid paths blend it against a
`[0,1]` min-max BM25 score. The
`alpha` a caller set stopped meaning what it says, and a note's contribution
shifted depending on whether a chunk or its summary won — a discontinuity.

Found during the Phase 4 daemon audit, in two pre-existing sites:
`src/vault/mod.rs` (the `lexical_prefetch` re-rank) and
`src/daemon/vault_context.rs`.

**Fix:** `blend_score_for()` divides by the summary weight. The rescale is
monotone, so semantic ordering is untouched — only inter-arm balance is restored.

**Test:** `blend_rescaling_is_monotone_and_bounded`.

## 7. Entire corpus sent in one embedding request

Upstream batched at `RECONCILE_BATCH_SIZE = 32` **notes**; chunking turns that
into hundreds of chunks per request, which overruns a local inference server
(`400 dial tcp 127.0.0.1:<port>/tokenize: connection refused`).

**Fix:** `embed_in_sub_batches()` — bounded sub-batches via `OBSIDIAN_EMBED_BATCH`
(default 16) with exponential-backoff retry.

## 8. No query/document prefixes

Asymmetric models (Arctic, E5, Nomic, Qwen) expect `query:` / `passage:` prefixes;
upstream sent none. Worth nDCG 0.675 → 0.706, and R@1 0.769 → 0.821 on
low-lexical-overlap queries.

**Fix:** `OBSIDIAN_EMBEDDING_QUERY_PREFIX` / `_DOC_PREFIX`, defaulting to the
benchmarked values. Explicit configuration rather than inferring from the model
name.

**Test:** `prefixes_default_to_the_validated_configuration`.

## 9. Cache-load errors swallowed

`load_for_space(...).ok()` discarded the error, so a rejected cache silently
rebuilt the entire index.

**Fix:** the rejection is logged with its reason and the note count.

## 10. `OBSIDIAN_TOOLS` was never enforced

The most consequential fix here, because its failure mode is data loss rather
than degraded retrieval.

`#[tool_handler]` defaults its router expression to `Self::tool_router()` — a
**freshly constructed** router, rebuilt on every request. The disabled set is
applied in `ObsidianMcp::new` to `self.tool_router`, so the handler never
consulted it. `OBSIDIAN_TOOLS` parsed correctly, `disabled_tools()` computed the
right set and the count was logged, while `list_tools` still advertised all 20
tools and `call_tool` dispatched them.

Confirmed against a running server started with `OBSIDIAN_TOOLS=read`:

```
tools/call note_delete {"path":"…","confirm":true}
  → -32002  "Note not found: …"      ← a vault-layer answer: it executed
```

Had the note existed, a read-only server would have deleted it.

**Fix:** `#[tool_handler(router = self.tool_router)]`. The same call now returns
`tool not found`, rejected by the router before parameter deserialization.

**Why the existing tests missed it.** All three asserted on
`server.tool_router.has_route(...)` — the field, which genuinely *was* disabled.
Nothing exercised what a client actually receives. The bug lived exactly in the
gap between the two.

**Tests:** `disabled_tools_are_absent_from_tools_list` and
`calling_a_disabled_tool_is_rejected_before_it_runs` drive a real JSON-RPC
session over a duplex transport. Both fail without the fix while the three
field-level tests still pass — verified by reverting the attribute and re-running.
`enabled_tools_still_work_while_a_filter_is_active` guards the other direction.

Inherited from upstream, which carries the same bare `#[tool_handler]` and the
same `OBSIDIAN_TOOLS` option.

## 10b. The `read` profile admitted tools that write

Fixing enforcement made this one reachable, and it is the more interesting bug
of the two: the filter worked exactly as designed, and the design was wrong.

`OBSIDIAN_TOOLS` matches tool **names**. Two tools multiplexed reading and
writing behind an `action` parameter, so no name-based filter could separate
their halves:

| tool | actions | in `read`? | consequence |
|---|---|---|---|
| `frontmatter` | get · **set · remove** | yes | a read-only server would rewrite frontmatter |
| `periodic` | get · list · **create** | no | blocking one write cost two reads |

Confirmed against a running server started with `OBSIDIAN_TOOLS=read`:

```
tools/call frontmatter {"path":"…","action":"set","key":"probe","value":"x"}
  → -32002  "Note not found: …"      ← a vault-layer answer: it executed
```

**Fix:** split both into single-purpose tools — `note_frontmatter` /
`note_frontmatter_edit`, and `periodic_get` / `periodic_list` /
`periodic_create`. `read` is now genuinely read-only *and* regains periodic
reads it had been denied. The same pass split `note_inspect` (metadata vs patch
targets) and `search_metadata` (tags vs frontmatter) for legibility, though
neither of those mixed reads with writes.

**Invariant, not vigilance.** `read_profile_admits_nothing_that_can_write`
checks `PROFILE_READ` against a hand-maintained `MUTATING_TOOLS` list, and
`every_profile_entry_is_a_real_tool` catches a name that no longer exists —
which would otherwise widen a profile silently, since `disabled_tools` ignores
unknown names. Adding a tool now forces a decision about which side of the line
it sits on.

## 10c. The daemon ignored exclusion patterns entirely

`OBSIDIAN_EXCLUDE_PATHS` and the vault's `ignore` file were applied by
`Vault::open` — and by nothing in the daemon. `VaultContext::open` built its
index with a hardcoded empty set:

```rust
VaultIndex::build(&vault_root, Arc::new(ExcludeSet::build(vec![])?)).await?
```

Under the default `OBSIDIAN_SEMANTIC_MODE=auto` the daemon answers semantic
queries while the server answers lexical ones, so the two disagreed about what
the vault contains: `search_text` correctly omitted excluded folders and
`search_semantic` kept returning them. On a vault configured to exclude seven
folders, the server indexed 476 notes and the daemon indexed 507.

Nothing reported an error. The symptom was a semantic result from a folder the
user had excluded — which reads as a retrieval quirk, not a configuration bug,
and is only obvious if you happen to know the folder should not be there.

**Fix:** the daemon resolves the same two sources the server does, and holds the
resulting set on `VaultContext`.

The watcher had the same hardcoded empty set, in both its `has_embeddings` and
non-embeddings arms — so even a correctly-excluded index would have re-admitted
an excluded note on the first edit inside one of those folders. It now filters
with the stored set.

**Tests:** `exclusion_patterns_read_the_vault_ignore_file` and
`exclusion_patterns_are_empty_for_a_vault_without_an_ignore_file`. The env-var
source is not unit-tested on purpose — `set_var` is unsafe in this edition and
the variable is process-global, so a test that set it would race
`Config::load` on another thread. The daemon logs `daemon path exclusion active`
with its resolved patterns instead, which is what to check on a live server.

## 11. Self-updater removed

`src/upgrade/` was 3,061 lines of launchd/systemd/cargo-install machinery that
reinstalls the **upstream** crates.io package over itself — actively wrong in a
fork. Removed along with the `--__build-info` handshake it existed for.
**−3,257 lines.**

---

## Status counting, as a footnote

`indexed_notes` counted with `store.get(path)`, which never matches a chunk key,
so a fully indexed vault reported **0 notes**. Fixed with `has_note()`, which is
O(1). Test: `has_note_counts_notes_not_chunks`.

## Deliberately not fixed

- **`wikilinks` output is not deterministically ordered** (hash-map iteration).
  Harmless for use, but it means snapshot testing needs order-insensitive
  comparison.
- **One test ignored on Windows** — `concurrent_readers_observe_only_complete_atomic_cache_snapshots`
  fails identically on upstream `fea2e1f`: Windows denies the atomic replace while
  a reader holds the cache file open. Now **verified passing on Linux** (Ubuntu
  26.04, glibc 2.43), so the Windows failure is a platform constraint, not a bug
  in the cache-swap logic.
