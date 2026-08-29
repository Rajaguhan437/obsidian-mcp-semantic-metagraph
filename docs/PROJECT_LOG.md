# Project log

Durable record of this work, written so the project survives conversation
compaction. Everything here is reconstructed from measurements taken during the
work, not from memory. Keep it updated as phases land.

**Last updated:** 2026-08-29, end of Phase 5 (pre-publication).

---

## 0. Where everything lives

| What | Path |
|---|---|
| The fork | `D:\obsidian-mcp-fork` (branch `chunk-level-retrieval`, remote `upstream` -> lstpsche/obsidian-mcp) |
| Rust toolchain | `D:\rust` — set `RUSTUP_HOME`, `CARGO_HOME`; **C: is nearly full, keep Rust off it** |
| Build target dir | `CARGO_TARGET_DIR=D:\rust\target2` |
| Benchmark harness | `C:\Users\Admin\rag-bench` (scripts) |
| Benchmark data | `D:\rag-bench` (isolated vault copy, queries, graph baselines, binaries in `bin/`) |
| Upstream clones | `C:\Users\Admin\rag-bench\upstream` (Rust), `C:\Users\Admin\rag-bench\omcs` (Python) |
| Production RAG | `obsidian-mcp-search` on `127.0.0.1:8848`, launcher `C:\Users\Admin\.config\obsidian-mcp-search\serve.cmd` |

**Always build with `--features embeddings-api`.** The default feature set
compiles with no embedding support at all.

**Never write benchmark state to the session scratchpad** — a temp cleaner wiped
it mid-project and destroyed a corpus of embeddings.

---

## 1. Where this started

The user runs a local stack: Qwen3.6-35B via LM Studio (`:1234`) for generation,
Ollama (`:11434`) for embeddings, on an **RTX 3060 Laptop with 6 GB VRAM**. The
Obsidian vault is at `D:\Personal\Obsidian Vault` — 512 markdown files, of which
`Spartan Vault` (416 notes, ~2.9 MB) holds the real content.

Original question: **which of three embedding models to use** — bge-m3,
snowflake-arctic-embed2, nomic-embed-text-v2-moe. (The user said "bge-large";
what was actually installed was bge-m3.)

### Vault characteristics that shaped every later decision

- Effectively **monolingual English plus heavy emoji** (2,596 emoji chars across
  260 notes; only 3 Tamil and 172 Arabic characters). Multilingual model strength
  is therefore wasted here.
- **Answers live in short, precisely-titled sections.** Median section-bound chunk
  is 248 characters. This is why section packing hurt (see Phase 2).
- 41% of notes exceed 400 words; 73.8% of all text sits past the 400-word mark.

---

## 2. Embedding model benchmark

Built a corpus of 413 notes / 4,494 chunks, and a query set of **76 queries**
authored by agents that read real notes, then adversarially verified against the
files (84 authored, 76 survived). Every gold path verified present.

nDCG@10, best configuration for each model:

| config | nDCG | R@1 | signal margin |
|---|---|---|---|
| **arctic-embed2 + `query:` prefix** | **0.706** | **0.842** | **0.189** |
| arctic-embed2, no prefix | 0.675 | 0.816 | 0.173 |
| nomic-v2-moe + prefix | 0.658 | 0.803 | 0.142 |
| nomic-v2-moe, no prefix | 0.643 | 0.763 | 0.138 |
| bge-m3 | 0.630 | 0.776 | 0.115 |

**bge-m3 finished last.** Its noise floor sits at 0.506 cosine versus 0.621 for
relevant text — it cannot support a similarity threshold. VRAM is identical
(633 MB vs 633 MB; nomic 559 MB), so nothing is bought by the weaker model.

**Decision: snowflake-arctic-embed2, with the `query:` prefix.**

---

## 3. Faults found in the Obsidian Local LLM Hub plugin

The user's original RAG was in this plugin. It was structurally broken:

- **`gJ = 50`** — a hard cap of 50 files per sync (`C.slice(0, 50)`); the rest are
  silently deferred. 416 notes need ~9 sync clicks.
- **Empty `targetFolders` means "whole vault", not "no filter"** — two of the
  three indexes were never scoped to Spartan Vault and burned slots on
  `Templates/` and agent scaffolding.
- **The `sentence` chunker splits only on `/[。.]\s/`.** A note with no `". "`
  becomes ONE chunk — worst case 380,276 characters. 92 notes affected.
- **No query/document prefixes** are sent, which arctic, nomic and Qwen all expect.

Measured ceiling of the existing indexes: **14-19 of 76 queries (18-25%) even had
their answer note present.** The user's inconclusive model comparison was
comparing three near-empty indexes built over different corpora.

---

## 4. MCP server selection (for Hermes, the real RAG host)

Two candidates:

| | lstpsche/obsidian-mcp | KORThomasJeong/obsidian-mcp-search |
|---|---|---|
| Language / size | Rust, 31,369 LOC | Python, 1,466 LOC |
| Stars, activity | 25★, active to Aug 2026 | 0★, 4 days of commits |
| Tools | 19 incl. full CRUD | 4, read-only |
| Chunking | **one vector per note, body cut at 400 words** | heading-aware, 1500/150 |
| Fusion | `alpha*BM25 + (1-alpha)*semantic`, alpha 0.25 | RRF k=60 |

Chose **obsidian-mcp-search** for production at the time, because lstpsche's
400-word truncation hid 73.8% of the vault's text from semantic search.

### Deployed production setup (still running)

- `uv tool install "obsidian-mcp-search[server,openai,sqlite-vec]"`
- 416 notes / 6,542 chunks, arctic via Ollama, MCP at `127.0.0.1:8848/mcp`
- Registered in Hermes as `obsidian` (4 tools)
- Live scores: R@1 0.776, R@5 0.921, R@8 0.934, MRR 0.841

**Two hacks it depends on:**
1. **e5 alias.** It applies `query: `/`passage: ` prefixes only when the model name
   contains "e5" (`e5_prefix()`). Arctic's native query prefix *is* `query: `, so
   the model is aliased `ollama cp snowflake-arctic-embed2:latest
   arctic-embed2-e5:latest`. Worth R@1 0.816 -> 0.855. **Never rename it.**
2. **Local patch.** Its `OpenAIEmbed._encode` sent every chunk in one HTTP request,
   which kills a local Ollama runner. Patched to batch with retry.
   **A `uv tool upgrade` silently reverts this.**

---

## 5. The fork: why lstpsche was revisited

Investigation of the 400-word cap found **no stated rationale anywhere** — not in
the code, not in the 65 KB `CLAUDE.md` that documents comparable constants, not in
`docs/`. Most likely a stale guard for the default `bge-small-en-v1.5` (512
tokens); the tell is that the cap is **model-independent**, applying equally to
models with 8192-token contexts.

Two further defects found:
- The cache key was hashed over the **already-truncated** text, so edits past word
  400 never invalidated the cache and the note was never re-embedded.
- Notes whose frontmatter parses to non-mapping (e.g. `---` then `## title:`,
  where `#` is a YAML comment) were **dropped entirely** — from both the BM25 and
  semantic indexes, **and from the link graph**. 10 of 416 notes affected.

Also found: **Tantivy indexes the full untruncated body** (`ts.f_body => body`),
so only the vector side was blind. An earlier claim that 73.8% was "invisible"
overstated the system; it was invisible to *semantic* search only. But fusion
weights semantic at 0.75, so the dominant signal was still computed from a
truncated document.

Two gifts: **heading byte offsets already existed** but were discarded by the
public wrapper, making heading-aware chunking nearly free; and Tantivy has zero
embedding dependencies, so BM25 was fully separable.

---

## 6. Phase 0 — remove the self-updater

Removed `src/upgrade/` (3,061 LOC of launchd/systemd/cargo-install), the
`--__build-info` handshake, and `tests/upgrade_*.rs`. **-3,257 lines.** A fork must
not carry an updater that reinstalls the *original's* crates.io package.

**Daemon deliberately NOT removed.** Measuring first showed 71 external
references, **38 of them in `src/tools/search.rs`** — the retrieval path. That
needs its own gate (Phase 4), not a cleanup commit.

---

## 7. Phase 1 — regression tests and fixture migration

Migrated every stale fixture rather than deleting or weakening it. Doing so
surfaced **two real bugs**:

1. **`retain_paths()` wiped the entire cache on every startup.** It kept entries
   whose *key* was in the set of note paths, but entries are keyed by chunk
   (`note\0<idx>`), so nothing matched. Every restart re-embedded the whole vault
   — indistinguishable from a slow first run.
2. **`score_for()` never received its fix.** A `git apply` carrying it aborted on
   an unrelated conflict. It still used `store.get(note)`, which never matches a
   chunk key and returned 0.0, silently zeroing the semantic half of the fusion.
   The `method is never used` warning is what exposed it.

Also stopped `.ok()` from swallowing cache-load failures.

Added `seed_note_chunks()` and `prepared_chunk_texts()` test helpers that mirror
production, so fixtures cannot drift from real hashing/chunking again, and
`note_content_hash()` so both sides share one definition.

`concurrent_readers_observe_only_complete_atomic_cache_snapshots` is ignored **on
Windows only** — verified failing identically at upstream `fea2e1f`. Windows
denies the atomic replace while a reader holds the cache open. Still runs on
Linux/macOS.

---

## 8. Phase 2 — the summary vector (current default)

Chunking alone regressed the strata the original was good at. The fix keeps the
original representation as a second, weighted arm:

```
sem(note) = max( max_i cos(q, chunk_i),  w_sum * cos(q, summary) )
summary   = title + ALL headings + first 400 words
w_sum     = 1.25
```

`max` is deliberate and load-bearing: it is monotone, so a summary can rescue a
note but **never dilute** one whose answer lives in a single chunk. Weighted sum
reintroduces exactly the failure mode being fixed.

### Validated defaults (measured, not chosen)

| setting | value | why |
|---|---|---|
| chunker | section-bound | packing measurably hurts this corpus |
| target / overlap | 1000 / 200 | widest safe operating range |
| `w_sum` | 1.25 | flat across [1.20, 1.30], **collapses at 1.32** (deep .941 -> .919) |
| prefixes | `query: ` / `passage: ` | benchmarked best |
| packing | **off** | halves the index but costs retrieval here |

`EMBEDDING_INPUT_VERSION` 2 -> 3 so older caches rebuild automatically.
Summary stored at reserved key `note\0s`.

### Phase 2 results (the current control)

| stratum | original | chunks-only (B2) | **Phase 2** |
|---|---|---|---|
| overall | 0.834 | 0.900 | **0.939** |
| deep (past word 400) | 0.552 | 0.941 | **0.941** |
| casual/typo | 0.930 | 0.875 | **0.975** |
| paraphrase | 0.714 | 0.765 | **0.794** |
| low-overlap | 0.818 | 0.817 | **0.857** |
| exact-keyword | 0.793 | 0.960 | **0.985** |

Overall R@1 0.908, R@5 0.961, R@8 0.961, MRR 0.932.
Cost: 8,709 vectors (8,297 chunks + 412 summaries), 402 s index, 70 MB peak,
147 ms median query (dominated by the Ollama query embedding).
**Warm restart: 3.2 s, 0 documents re-embedded** — proof the cache fix works.

Known consequence: `score_for()` can now **exceed 1.0** (a perfect summary match
scores 1.25). It is a ranking score, not a cosine. Any future score-space fusion
must normalise first.

---

## 9. Phase 3 — hybrid retrieval: rejected as default

28 fusion configurations tested against the Phase-2 control, all with union
candidate generation, per-arm weights, and calibrated scores. Constraints
honoured: no BM25-gated candidates, no unweighted RRF, no min-max normalisation.

**Nothing beat the control.** Best ties at 0.944 (z-score sum and bounded lexical
bonus, both at `w_lex = 0.10`); everything else degrades monotonically.

The decisive test — does BM25 rank anything semantics misses?

| | |
|---|---|
| semantic misses top-8 | **3 / 76** |
| BM25 misses top-8 | 22 / 76 |
| **queries BM25 could rescue** | **0** |
| queries BM25 could spoil | 19 |
| gold at rank 1 | semantic **70/76**, BM25 34/76 |
| exact-keyword stratum | BM25 0.891, **semantic 1.000** |

BM25's contribution is a **strict subset** of what the semantic system already
retrieves. This is structural, not a tuning failure. The mechanism: the summary
vector is `title + all headings + first 400 words` — precisely the fields Tantivy
boosts hardest (title 5.0, headings 3.0). Phase 2 already embeds the lexical
signal into semantic space.

**Decision:** semantic-only remains the default. Tantivy is retained for its own
explicit tools. Hybrid ranking ships as opt-in `OBSIDIAN_LEXICAL_WEIGHT`,
default 0.

### Shipped agent-facing architecture

1. **Semantic retrieval** (`search_semantic`) - default path, meaning-based.
2. **Explicit lexical search** (`search_text`, `search_regex`, `search_metadata`)
   - first-class, for exact strings, identifiers and terminology.
3. **Graph tools** (`wikilinks` with backlinks/outgoing/broken/orphans, plus
   `note_inspect`, `frontmatter`) - relationships.
4. **Experimental hybrid ranking** - `OBSIDIAN_LEXICAL_WEIGHT`, default 0.

Implementation notes for the opt-in path: candidate generation is a **union**
(semantic scores the whole store, lexical contributes its own hits), so a note
BM25 never surfaces stays reachable - deliberately not the upstream BM25-gated
re-rank. Both arms are **unit-calibrated** (divided by their own maximum, floor
left at zero) rather than min-max normalised, because min-max lifts the weakest
candidate of every query to 0 and hands irrelevant documents a flat bonus - and
because the semantic arm is not a cosine, reaching w_sum = 1.25.

Verified: with the setting absent the default path returns **76/76 identical
rankings** to Phase 2; with it at 0.10, 54/76 rankings change, so the path is
genuinely wired and genuinely inert by default.

---

## 10. Bugs found in the measurement harness itself

Recorded because each produced a confident, wrong answer first:

1. **Readiness inferred from proxy silence.** The server answers with a
   lexical-only fallback while the embedding runtime warms up, so a whole run
   silently became pure BM25. The tell: results matched BM25 to three decimals.
2. **Cumulative cost counters.** Resetting the stats *file* while the proxy counted
   in memory reported every variant's traffic summed.
3. **`lexical_prefetch: true`** re-ranks only BM25's top 50 — a candidate
   generation bottleneck. That one flag cost A 0.534 -> 0.834 and B1 0.522 -> 0.890.
4. **A NUL byte injected into Rust source** by shell escaping, which turned the
   file binary.

---

## 11. Graph layer — first-class, verified preserved

Zero cross-references between the removed code and the graph layer. `wikilinks`
keeps all four modes (`backlinks`, `outgoing`, `broken`, `orphans`); `WikiLink`
keeps `heading` / `block_ref` / `alias` / `line`; `LinkResolver`, backlink maps
and incremental maintenance are untouched. All 19 MCP tools present.

Graph totals, verified identical across Phase 0/1/2 (order-insensitively —
`wikilinks` output is **not** deterministically ordered):

| metric | upstream | phases 0-2 |
|---|---|---|
| resolved edges | 487 | **497** |
| backlink edges | 487 | **497** |
| notes with outgoing | 131 | **138** |
| broken links | 288 | **310** |
| orphans | 23 | 23 |

The frontmatter fix **repaired the graph**: 10 inbound links that were falsely
reported broken now resolve, and 32 outgoing links from 7 recovered notes became
visible. Those 32 are themselves broken — they point at notes that genuinely do
not exist — so `broken_links` rising from 288 to 310 is the graph telling the
truth, not a regression.

---

## 12. Benchmark methodology

- **Corpus:** isolated byte-copy of Spartan Vault at `D:\rag-bench\vault`
  (416 notes, 2.9 MB). Production never touched.
- **Queries:** 76, agent-authored from real notes then adversarially verified
  against the files. Stratified six ways: overall, deep (answer past word 400),
  casual/typo, paraphrase, low lexical overlap, exact keyword.
- **Gold:** every path verified present in the corpus.
- **Model:** snowflake-arctic-embed2 via Ollama, identical for every variant.
- **Cost accounting:** a counting reverse proxy in front of Ollama records exact
  embedding calls and batch sizes — measured, not inferred.
- **Significance:** 10,000 paired bootstrap resamples where quoted.

### Standing caveat, to repeat in any public write-up

**One vault, 76 queries, one embedding model.** These are measurements from a
specific corpus with specific characteristics (prose-like notes, descriptive
headings, short titled sections). They are evidence, not universal claims.

---

## 13. State and what is next

Commits on `chunk-level-retrieval`:

```
65dbba4  feat(retrieval): summary vector alongside chunks, weighted max
4bba510  fix: chunk-key handling in cache retention and per-note scoring
9f7f6aa  test: regression coverage for every bug this investigation found
3179b17  refactor: remove the self-upgrade machinery
df738b3  feat(semantic): chunk-level retrieval with heading-aware chunking
fea2e1f  (upstream v2.5.0 base)
```

Tests: **672 unit, 0 fail, 1 ignored (Windows-only, pre-existing)**, plus 60
integration and 1 doc test.

- **Phase 3** — settled: semantic-only default, opt-in lexical weight.
- **Phase 4** — settled: **daemon retained**. It is a transport/lifecycle wrapper
  around the same engine (`daemon/vault_context.rs` holds an `EmbeddingRuntime`
  and calls the same `semantic_scores_for_paths` / `score_for`), so it inherits
  every improvement automatically. Its value — one shared model across clients —
  is real for the `local` provider and largely redundant for an API provider.
  Two corrections came out of it: my "38 references in the retrieval path" figure
  was wrong (10 production, 28 test), and the audit found that Phase 2 had
  unbalanced **both** pre-existing hybrid blends by letting the semantic score
  reach 1.25. Fixed with `blend_score_for()`.
- **Phase 5** — final validation and release documentation. Complete.
- **Phase 4** — daemon/client architecture. Not started. Map the 5,147 LOC and
  71 references (38 in `src/tools/search.rs`) before assuming removal is wanted.
- **Phase 5** — final validation and GitHub release prep. Not started.

Target architecture for agents: semantic retrieval for meaning, explicit lexical
tools for exact matching, graph tools for relationships, and experimental hybrid
ranking behind a flag.


---

## 14. Phase 5 — final validation

Run against the frozen Phase-4 binary.

| check | result |
|---|---|
| unit tests | 676 pass, 0 fail, 1 ignored |
| binary tests | 1 pass |
| integration tests | 60 pass |
| doc tests | 0 (none defined) |
| **total** | **737 pass, 0 fail, 1 ignored** |
| retrieval benchmark | overall .939, deep .941, casual .975, paraphrase .794, low-overlap .857, exact .985 — identical to the Phase-2 control |
| graph integrity | 0 order-insensitive diffs vs the accepted Phase 0-1 baseline |
| cache warm restart | 3.2 s, 0 documents re-embedded |
| MCP tools | 19/19 registered, 22/22 invocations pass |
| retrieval paths | semantic, lexical, hybrid all OK |
| semantic modes | `local` OK, `auto` OK; `daemon` needs the IPC endpoint, not exercised |

Note on tool verification: the first pass showed 6 "failures" that were entirely
**my own wrong parameter names** — every error was `missing field X`. The tools
were returning precise validation errors, which is itself evidence they are
correctly wired. Corrected parameters in `docs/TOOLS.md`.

Release documentation written: `README.md`, `docs/ARCHITECTURE.md`,
`docs/BENCHMARKS.md`, `docs/FIXES.md`, `docs/TOOLS.md`. Upstream's original
README preserved at `docs/UPSTREAM_README.md`.


---

## Phase 5b — Linux daemon verification

The 8 daemon integration tests are gated `#[cfg(all(unix, feature =
"embeddings"))]` and had **never executed** at any point in this project. They
use Unix domain sockets, so Windows cannot run them even in principle.

### Getting to a usable Linux

| distro | glibc | outcome |
|---|---|---|
| Ubuntu 22.04 | 2.35 | **build fails at link** — `undefined symbol: __isoc23_strtoll` |
| Ubuntu 26.04 | 2.43 | builds and passes |

The prebuilt ONNX Runtime that `ort-sys` downloads for the `embeddings` feature
references `__isoc23_*`, introduced in glibc 2.38. This is a property of the
*binary being linked*, not of our code, and it does not affect `embeddings-api`
(pure Rust over rustls). Recorded in the README as an install requirement.

A process note worth keeping: roughly ten minutes were burned compiling on 22.04
before running `ldd --version`. The cheap check that would have predicted the
failure was available from the start and was not run first.

### Two network faults, neither in the code

Large HTTPS transfers were being severed on this WSL instance:

1. `cargo fetch` aborted with `transfer too slow` on `static.crates.io`.
   Fixed with `~/.cargo/config.toml`: `multiplexing = false`,
   `low-speed-limit = 0`, `retry = 10`, then a fetch/build split so a network
   blip cannot kill a long compile.
2. The fastembed model download hung on an **established but idle socket** —
   `hf-hub` sets no read timeout, so it waited indefinitely. `curl` reproduced
   the same failure (`SSL_read: unexpected eof` after 47 MB), which is what
   proved the fault was the network path and not `hf-hub`.

Resolved by fetching the five model files with a resumable `curl` and assembling
the hf-hub cache directly. `ApiRepo::get()` short-circuits on a cache hit and
makes no network call at all, so the tests then ran fully offline. Layout:
`blobs/{etag}`, `snapshots/{commit}/{file}` symlinks, `refs/main`. The model's
`x-linked-etag` matched the stalled `.part` filename, which confirmed the
reconstruction before anything was run.

The model is byte-identical to what fastembed would have fetched
(`onnx/model.onnx`, 133,093,490 bytes, commit `ea104dac`).

### Results — Ubuntu 26.04, glibc 2.43, rustc 1.98.0

| target | tests | result |
|---|---|---|
| unit (lib) | 670 | pass |
| binary | 1 | pass |
| `daemon_integration_tests` | **8** | pass |
| `integration_tests` | 72 | pass |
| **total** | **751** | **0 fail, 0 ignored** |

All 8 daemon tests pass in 7.18 s: health/open-hint, Unicode-equivalent path
acceptance, path-traversal rejection, per-vault isolation, watcher
create/modify/delete sync, concurrent client attach+query, recovery after a
watcher reindex error, and the empty-query short-circuit in hybrid search.

### On comparing the counts to Windows

Windows recorded 737 pass / 1 ignored; Linux records 751 pass / 0 ignored.
**Neither is a superset.** `src/` has 24 Windows-gated blocks (6 unit tests that
cannot run on Linux), while `tests/integration_tests.rs` has
`#[cfg(all(unix, ...))]` blocks contributing 12 tests that cannot run on Windows,
plus the 8 daemon tests. Reporting this as "751 > 737, therefore better" would be
wrong.

`concurrent_readers_observe_only_complete_atomic_cache_snapshots` — ignored on
Windows and failing identically on upstream `fea2e1f` — **passes here**. That
promotes the Windows failure from an asserted platform limitation to a
demonstrated one.
