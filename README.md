# obsidian-mcp-semantic-metagraph

> A fork of **[lstpsche/obsidian-mcp](https://github.com/lstpsche/obsidian-mcp)**, incorporating retrieval ideas from
> **[KORThomasJeong/obsidian-mcp-search](https://github.com/KORThomasJeong/obsidian-mcp-search)**. See [Attribution](#attribution).

An MCP server giving AI agents semantic search, lexical search, and graph
navigation over an Obsidian vault — reading the vault directly from disk, with no
Obsidian plugin and no REST API.

This is a **fork of [lstpsche/obsidian-mcp](https://github.com/lstpsche/obsidian-mcp)**
that rebuilds the semantic retrieval layer. Upstream embedded one vector per note
from a body truncated to 400 words; this fork indexes the whole note as
heading-aware chunks and keeps a note-level summary vector alongside them.

On the vault it was developed against that moved retrieval nDCG from **0.834 to
0.939**, and on queries whose answer sits past the truncation point, from
**0.552 to 0.941**.

> **Read this before trusting those numbers.** They come from **one vault (416
> notes), 76 queries, and one embedding model**. That is evidence from a specific
> corpus, not a general claim. Several results were measurably corpus-dependent,
> and one change that looked clearly beneficial on paper made things worse here.
> See [Known limitations](#known-limitations) and [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

---

## What it gives an agent

Four capabilities, deliberately kept separate rather than merged behind one
ranking function:

| | for | tools |
|---|---|---|
| **Semantic retrieval** | meaning-based questions, paraphrase, vague recall | `search_semantic` |
| **Lexical search** | exact strings, identifiers, terminology, regex | `search_text`, `search_regex`, `search_metadata` |
| **Graph navigation** | relationships between notes | `wikilinks`, `note_inspect`, `frontmatter` |
| **Note operations** | read, create, edit, move, patch | `note_*`, `vault_*`, `periodic` |

Keeping lexical search as its *own tool* rather than blending it into semantic
ranking was a measured decision — see
[Why hybrid ranking is off](#why-hybrid-ranking-is-off-by-default).

## Retrieval architecture

Each note contributes **chunks plus one summary**:

- **Chunks** — heading-aware, ~1000 characters with 200 overlap, each carrying a
  breadcrumb (`Title > H1 > H2`) so the embedding keeps its structural context.
  Code fences are respected, so `#` inside a fenced block is never mistaken for a
  heading, and no chunk can exceed a hard ceiling whatever the note looks like.
- **Summary** — `title + all headings + first 400 words`. This is upstream's
  original representation, kept deliberately.

Scoring combines them:

```
score(note) = max( max_i cos(q, chunk_i),  w_sum * cos(q, summary) )
```

`max` rather than a weighted sum is the load-bearing choice. Max is **monotone**,
so the summary can rescue a note but can never dilute one whose answer lives in a
single chunk — exactly the failure a weighted sum reintroduces.

Why keep a summary at all? Chunking alone *regressed* what whole-note embedding
was good at: typo-heavy queries fell 0.930 → 0.875 and title-shaped lookups
suffered. The summary arm recovered them (to 0.975) without giving up any of the
deep-content gain.

Detail: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Install

Requires Rust. Embedding backends are **not** enabled by default:

```bash
# API-backed embeddings (Ollama, OpenAI, LM Studio, vLLM — any OpenAI-compatible endpoint)
cargo install --path . --features embeddings-api    # installs the `obsidian-mcp` binary

# or local in-process embeddings (fastembed)
cargo install --path . --features embeddings
```

Building with neither feature yields a server with lexical and graph tools but no
semantic search.

**Platform note for `--features embeddings`.** This backend links a prebuilt ONNX
Runtime that requires **glibc >= 2.38** (it references the `__isoc23_*` symbols).
Ubuntu 24.04+, Debian 13+ and Fedora 39+ satisfy this. On older systems the build
fails at link time with `undefined symbol: __isoc23_strtoll` — observed on Ubuntu
22.04 (glibc 2.35); verified working on Ubuntu 26.04 (glibc 2.43).

`--features embeddings-api` carries no such constraint: it is pure Rust over
rustls and builds anywhere Rust does. Prefer it if you already run Ollama, LM
Studio or any OpenAI-compatible endpoint.

### Quick start with Ollama

```bash
ollama pull snowflake-arctic-embed2

export OBSIDIAN_VAULT_PATH="/path/to/vault"
export OBSIDIAN_EMBEDDINGS=true
export OBSIDIAN_EMBEDDING_PROVIDER=api
export OBSIDIAN_EMBEDDING_API_BASE=http://localhost:11434/v1
export OBSIDIAN_EMBEDDING_API_MODEL=snowflake-arctic-embed2:latest
export OBSIDIAN_EMBEDDING_API_KEY=ollama      # Ollama ignores the value
export OBSIDIAN_EMBEDDING_DIM=1024

obsidian-mcp                # stdio transport
obsidian-mcp --http         # streamable HTTP on :37842
```

First index of ~400 notes takes a few minutes; later starts reuse the cache and
are near-instant.

## Configuration

### Retrieval

| variable | default | notes |
|---|---|---|
| `OBSIDIAN_SUMMARY_WEIGHT` | `1.25` | Weight of the summary arm. Tested plateau **1.20–1.30**; above 1.32 deep-content retrieval degrades measurably. `0` disables the arm. |
| `OBSIDIAN_CHUNK_CHARS` | `1000` | Chunk target size. |
| `OBSIDIAN_CHUNK_OVERLAP` | `200` | Overlap between chunks. |
| `OBSIDIAN_CHUNK_PACKING` | `false` | Merge adjacent small sections up to the target. Halves the index; **measurably hurt retrieval** here. |
| `OBSIDIAN_LEXICAL_WEIGHT` | `0` | Experimental hybrid ranking. Off by default — see [below](#why-hybrid-ranking-is-off-by-default). |
| `OBSIDIAN_HYBRID_ALPHA` | `0.25` | BM25 weight in the legacy `lexical_prefetch` re-rank path. |

### Embeddings

| variable | default | notes |
|---|---|---|
| `OBSIDIAN_EMBEDDINGS` | `false` | Master switch for semantic search. |
| `OBSIDIAN_EMBEDDING_PROVIDER` | inferred | `local` (fastembed) or `api`. |
| `OBSIDIAN_EMBEDDING_API_BASE` | OpenAI | Any OpenAI-compatible `/v1` endpoint. |
| `OBSIDIAN_EMBEDDING_API_MODEL` | — | Model name at that endpoint. |
| `OBSIDIAN_EMBEDDING_API_KEY` | — | Falls back to `OPENAI_API_KEY`. |
| `OBSIDIAN_EMBEDDING_DIM` | probed | Set explicitly to skip a probe request. |
| `OBSIDIAN_EMBEDDING_QUERY_PREFIX` | `"query: "` | **Set both empty for prefix-free models such as bge-m3.** |
| `OBSIDIAN_EMBEDDING_DOC_PREFIX` | `"passage: "` | |
| `OBSIDIAN_EMBED_BATCH` | `16` | Chunks per provider request; large batches overrun local inference servers. |

Asymmetric models (Arctic, E5, Nomic, Qwen) expect these prefixes; sending none
silently costs accuracy. Here the query prefix alone was worth nDCG 0.675 → 0.706.

### Server

| variable | default |
|---|---|
| `OBSIDIAN_VAULT_PATH` | required |
| `OBSIDIAN_TRANSPORT` | `stdio` |
| `OBSIDIAN_HTTP_PORT` / `_HOST` | `37842` / `127.0.0.1` |
| `OBSIDIAN_WATCH` | `true` |
| `OBSIDIAN_TANTIVY` | `true` |
| `OBSIDIAN_SEMANTIC_MODE` | `auto` — `auto` \| `local` \| `daemon` |
| `OBSIDIAN_MCP_DATA` | `{vault}/.obsidian-mcp` |
| `OBSIDIAN_EXCLUDE_PATHS` | none |
| `OBSIDIAN_TOOLS` | `full` |

## Tools

19 tools. Reference: [docs/TOOLS.md](docs/TOOLS.md).

**Search** — `search_semantic` · `search_text` · `search_regex` · `search_metadata`
**Graph** — `wikilinks` (backlinks / outgoing / broken / orphans)
**Read** — `note_read` · `note_read_many` · `note_inspect` · `frontmatter`
**Write** — `note_create` · `note_write` · `note_insert` · `note_patch` · `note_move` · `note_delete`
**Navigate** — `vault_list` · `vault_info` · `periodic` · `open_in_obsidian`

## Graph capabilities

The link graph is a first-class layer, independent of retrieval. `wikilinks`
answers four questions: **backlinks**, **outgoing**, **broken**, and **orphans**
(distinguishing notes with no links at all from notes whose only links are
broken). Links keep full fidelity — heading fragments (`[[note#heading]]`), block
references (`[[note#^blockid]]`), aliases (`[[note|alias]]`), and line numbers.
Backlink maps update incrementally on create, modify, rename and delete.

A frontmatter fix here also **repaired the graph**: notes whose frontmatter parsed
to a non-mapping were previously dropped from the index entirely, so links
pointing at them were falsely reported broken and their own outgoing links never
existed. On the development vault that restored 42 edges.

## Why hybrid ranking is off by default

Adding BM25 to semantic ranking is the obvious next step. It was tested across 28
fusion configurations — weighted RRF, z-score sum, unit-normalised sum, and a
bounded lexical bonus — with union candidate generation and per-arm weighting.

**None beat semantic-only.** The decisive measurement was not the sweep but one
diagnostic: *how many queries does BM25 rank correctly that semantics misses?*
**Zero of 76** — while it could have spoiled 19. Its contribution was a strict
subset.

The mechanism explains it: the summary vector already embeds title and all
headings, precisely the fields BM25 boosts hardest. The lexical signal was
already inside the semantic space. Semantic beat BM25 even on the exact-keyword
stratum, **1.000 vs 0.891**.

This is the most corpus-dependent finding in the project. A vault heavy in rare
proper nouns, code identifiers or exact-string lookups would plausibly show the
opposite, so the knob exists: set `OBSIDIAN_LEXICAL_WEIGHT` above 0. When enabled
it uses union candidate generation (never a BM25 gate) and calibrates each arm by
its own maximum. Values near 0.10 were least harmful here; above ~0.25 quality
degrades sharply.

If you want literal matching, prefer asking for it directly with `search_text` —
clearer than burying it in a blend.

## Why the daemon is retained

The `obsidian-semanticd` daemon and its client are kept, not removed.

It is a **transport and process-lifecycle wrapper around the same retrieval
engine**, not a parallel implementation — it calls the same
`semantic_scores_for_paths` and `score_for`, so it inherits every improvement in
this fork automatically. Its purpose is one shared model and one model cache
across several clients (this server plus Obsidian plugin clients).

That value is real but conditional: with a **local** (fastembed) provider it
avoids loading the model once per client; with an **API** provider it is largely
redundant, because the inference server is already the shared model host.
Removing it would break the local multi-client case to gain nothing for the API
case, so it stays — inert unless `OBSIDIAN_SEMANTIC_MODE` selects it.

## Benchmark results

Same vault, queries, gold labels and embedding model throughout. nDCG@10.

| stratum | upstream | chunks only | **this fork** |
|---|---|---|---|
| overall | 0.834 | 0.900 | **0.939** |
| deep content (answer past word 400) | 0.552 | 0.941 | **0.941** |
| casual / typo | **0.930** | 0.875 | **0.975** |
| paraphrase | 0.714 | 0.765 | **0.794** |
| low lexical overlap | 0.818 | 0.817 | **0.857** |
| exact keyword | 0.793 | 0.960 | **0.985** |

Overall R@1 0.908, R@5 0.961, MRR 0.932.

Note the middle column: **chunking alone lost to upstream on typo-heavy queries.**
Reporting only the aggregate would have hidden that, and the fix would never have
been found. Methodology and full results: [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Significant fixes over upstream

| fix | impact |
|---|---|
| Chunk-level retrieval replacing the 400-word truncation | deep-content nDCG 0.552 → 0.941 |
| Cache retention keyed on the wrong identifier | the entire cache was wiped on **every startup**, silently re-embedding the vault |
| Per-note scoring missing chunk keys | returned `0.0` for every note, silently reducing hybrid search to lexical-only |
| Content hash computed post-truncation | edits past word 400 never invalidated the cache, so the note was never re-embedded |
| Non-mapping frontmatter dropped the note entirely | 10 of 416 notes lost from **both indexes and the link graph**; 42 edges restored |
| Unbatched embedding requests | an entire corpus in one request overruns local inference servers |
| No query/document prefixes | asymmetric models silently underperformed |
| Semantic score exceeding `[0,1]` after weighting | unbalanced both hybrid blends; `alpha` stopped meaning what it says |
| Cache-load errors swallowed by `.ok()` | a rejected cache silently re-embedded everything |
| Self-updater reinstalling the upstream package | removed — 3,257 lines |

Each has a regression test. Detail: [docs/FIXES.md](docs/FIXES.md).

## Known limitations

- **Single-corpus evidence.** One vault, 416 notes, 76 queries, one embedding
  model. Re-measure on your own corpus before trusting any default here.
- **Corpus-dependent defaults.** `OBSIDIAN_CHUNK_PACKING=false` and
  `OBSIDIAN_LEXICAL_WEIGHT=0` suit a vault of short, precisely-titled sections. A
  vault of long prose, or one heavy in identifiers, may want the opposite.
- **Upstream still wins somewhere.** In a chunks-only configuration upstream's
  whole-note representation beat this fork on typo-heavy queries. The summary arm
  exists because of that, and the row stays in the table.
- **`w_sum > 1.0` is a correction, not a tuned constant.** `max` over ~20 chunks
  is systematically higher than a single summary score, so the arms are not on
  equal footing. Normalising for chunk count would be more principled.
- **The summary arm costs storage** — roughly +5% vectors and +21% indexing time.
- **`wikilinks` output is not deterministically ordered** (hash-map iteration),
  which matters for snapshot testing.
- **One test is ignored on Windows** — a pre-existing upstream failure where
  Windows denies an atomic file replace while a reader holds the file open.
  Confirmed to run and pass on Linux (Ubuntu 26.04, glibc 2.43), which is what
  establishes it as a platform limitation rather than a defect.
- **Section packing is implemented but unproven elsewhere.** It halved the index
  here at a small measured quality cost; no other corpus has been tested.
- **The daemon is covered by tests but not by benchmarks.** Its 8 integration
  tests (Unix-socket IPC, per-vault isolation, watcher sync, concurrent clients,
  error recovery) pass on Linux. They are `#[cfg(all(unix, feature =
  "embeddings"))]`, so they cannot run on Windows at all. The retrieval
  *benchmarks* were all run through `local` mode.

## Attribution

- **[lstpsche/obsidian-mcp](https://github.com/lstpsche/obsidian-mcp)** — the base
  of this fork. The vault index, Tantivy lexical search, link graph, tool surface,
  embedding cache with its integrity checks, and the semantic daemon are all its
  work. MIT licensed. Its original README is kept at
  [docs/UPSTREAM_README.md](docs/UPSTREAM_README.md).
- **[KORThomasJeong/obsidian-mcp-search](https://github.com/KORThomasJeong/obsidian-mcp-search)**
  — not vendored, but its design directly informed this fork: heading-aware
  chunking with breadcrumbs, returning the matched passage with its heading path,
  and separating embedded text from returned text. MIT licensed.

Ideas from the second project were reimplemented, not copied; no code was taken
from it.

## Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — retrieval, graph, daemon
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md) — methodology and full results
- [docs/FIXES.md](docs/FIXES.md) — every significant fix and its regression test
- [docs/TOOLS.md](docs/TOOLS.md) — tool reference
- [docs/PROJECT_LOG.md](docs/PROJECT_LOG.md) — how this was built, including what went wrong
- [docs/METHODOLOGY.md](docs/METHODOLOGY.md) — transferable evaluation lessons
- [CONTRIBUTING.md](CONTRIBUTING.md) — building, testing, and the platform gotchas
- [bench/README.md](bench/README.md) — the evaluation harness, and running it on
  your own vault

The harness ships **without a corpus**: the development vault is a private set of
personal notes and the query set quotes their contents. You can run the same
procedure on your own vault, but you cannot reproduce the exact numbers reported
here — which is consistent with the defaults being corpus-dependent.

## Naming

The package is `obsidian-mcp-semantic-metagraph`; the binary it installs is
`obsidian-mcp`, unchanged from upstream so existing MCP client configuration
keeps working. The MCP server announces itself as
`obsidian-mcp-semantic-metagraph` so a client can tell which implementation
answered.

**If you have upstream installed, note that `cargo install` writes the same
binary name into `~/.cargo/bin` and the later install wins.** Install only one,
or rename the binary after installing.

## License

MIT, as upstream. Upstream's copyright is retained in [LICENSE](LICENSE); the
fork's copyright is added beneath it, as MIT requires.
