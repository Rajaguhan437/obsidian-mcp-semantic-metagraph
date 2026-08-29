# obsidian-mcp-semantic-metagraph

[![CI](https://github.com/Rajaguhan437/obsidian-mcp-semantic-metagraph/actions/workflows/ci.yml/badge.svg)](https://github.com/Rajaguhan437/obsidian-mcp-semantic-metagraph/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> A fork of **[lstpsche/obsidian-mcp](https://github.com/lstpsche/obsidian-mcp)**, incorporating retrieval ideas from
> **[KORThomasJeong/obsidian-mcp-search](https://github.com/KORThomasJeong/obsidian-mcp-search)**. See [Attribution](#attribution).

An MCP server giving AI agents semantic search, lexical search, and graph
navigation over an Obsidian vault — reading the vault directly from disk, with no
Obsidian plugin and no REST API.

This is a **fork of [lstpsche/obsidian-mcp](https://github.com/lstpsche/obsidian-mcp)**
that rebuilds the semantic retrieval layer. Upstream embedded one vector per note
from a body truncated to 400 words; this fork indexes the **note body** as
heading-aware chunks, so no part of it is unreachable, and keeps a note-level
summary vector alongside them. That summary is still built from the first 400
words - deliberately, as a second and coarser representation rather than the
primary one.

**The code is smaller than what it forked.** `src/` and `tests/` come to a net
**−1,986 lines** against upstream — 2,198 added, 4,184 removed, 11 files deleted
and 1 added. Most of the deletion is a self-updater that reinstalled the
*upstream* crates.io package over itself, which is actively wrong in a fork. The
repository as a whole did grow, but only in documentation and the benchmark
harness. Check it in one command:

```bash
git diff --shortstat fea2e1f..HEAD -- src/ tests/
```

On the vault it was developed against that moved retrieval nDCG from **0.834 to
0.939**, and on queries whose answer sits past the truncation point, from
**0.552 to 0.941**.

> **Read this before trusting those numbers.** They come from **one vault (416
> notes), 76 queries, and one embedding model**. That is evidence from a specific
> corpus, not a general claim. Several results were measurably corpus-dependent,
> and one change that looked clearly beneficial on paper made things worse here.
> See [Known limitations](#known-limitations) and [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

---

**Start here** — [Install](#install) · [Configuration](#configuration) · [Tools](#tools)

**How it works** — [What an agent gets](#what-it-gives-an-agent) · [Retrieval architecture](#retrieval-architecture) · [Provenance](#retrieval-provenance) · [Graph](#graph-capabilities)

**Why it is built this way** — [Hybrid ranking is off](#why-hybrid-ranking-is-off-by-default) · [The daemon is kept](#why-the-daemon-is-retained)

**Evidence** — [Benchmarks](#benchmark-results) · [Fixes over upstream](#significant-fixes-over-upstream) · [Known limitations](#known-limitations)

**Context** — [Why this exists](#why-this-exists) · [Attribution](#attribution) · [All documentation](#documentation)

---

![Architecture overview: an Obsidian vault is read from disk, parsed and split
into heading-aware chunks of about 1000 characters with 200 overlap, and indexed
as two representations per note - many chunk vectors plus one summary vector.
A query scores both arms and takes the maximum, then every hit reports which
representation ranked it along with the matching passage and its heading
path.](docs/images/architecture.png)

*The diagram abbreviates some notation. The exact scoring formula is under*
*[Retrieval architecture](#retrieval-architecture), and*
*[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) is authoritative where the two differ.*

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


## Retrieval provenance

Every semantic hit reports **which representation determined its rank** and, when
the note has chunks, the most relevant passage from that note.

Those are two different claims, and the API keeps them apart. For a **chunk win**
the passage *is* the reason the note ranked. For a **summary win** the note
ranked as a whole and the passage is supporting evidence, not the cause.
`match_type` is what distinguishes them - branch on it, not on whether a passage
is present.

```json
{
  "path": "Engineering/Ingest Worker Design.md",
  "title": "Ingest Worker Design",
  "score": 0.94,
  "match_type": "chunk",
  "best_chunk": {
    "index": 12,
    "heading_path": ["Ingest Worker Design", "Retry policy"],
    "passage": "## Retry policy

After testing we settled on five attempts...",
    "score": 0.94
  },
  "summary_score": 0.71
}
```

**`match_type` is the honest part.** The summary vector is a retrieval arm in its
own right, so a note can rank because it matched *as a whole* rather than at one
passage:

| `match_type` | what it means |
|---|---|
| `chunk` | one passage caused the ranking; `best_chunk` is that passage |
| `summary` | the whole-note vector caused it; `best_chunk` is still the note's most relevant passage, but it did **not** cause the rank |
| `note` | a legacy whole-note entry from a pre-chunking cache |

**Attribution and evidence are separate.** `best_chunk` is supplied on every hit
that has chunks — including summary wins — because withholding it would force an
agent to re-read a whole note to find what the index already knows. Reporting the
passage is not a claim that it ranked; `match_type` carries that, and
`summary_score` next to `best_chunk.score` shows how close the two arms were.

`summary_score` is a number, not text. Returning a 400-word summary per hit would
dominate the response for no retrieval benefit; use `note_read` when the whole
note is wanted.

**Every hit carries a passage; not every hit is *attributable* to one.**
Measured live over 608 hits on the development corpus: `best_chunk` was present
on **100%**, while **27.3%** had `match_type: "chunk"` — the rest ranked on their
summary. So an agent always has a passage to read; roughly a quarter of the time
that passage is also the reason the note surfaced.

That attribution share is set by `OBSIDIAN_SUMMARY_WEIGHT`, and it is why the
default is 1.20 rather than 1.25: ranking is **bit-identical** between the two
(verified live — every stratum delta +0.000) while attribution rises from 18.6%
to 27.3%. Lowering the weight further trades ranking for attribution: at `1.0`
most hits become chunk-attributable, but casual/typo retrieval drops from .975 to
.888.

**Cost.** Roughly +117% response bytes at `top_k=5` without content (1,764 →
3,836 bytes, ~518 tokens), median 206 bytes of evidence per result.

**Not available** in `daemon` mode, whose IPC protocol carries note-level hits
only, or on the experimental hybrid path, where a blended rank is not
attributable to a single representation. Both omit the fields rather than
guessing.

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
| `OBSIDIAN_SUMMARY_WEIGHT` | `1.20` | Weight of the summary arm. Ranking is **identical across [1.18, 1.28]**; above 1.32 deep-content retrieval degrades measurably. The default sits at the low end of that plateau because a lower weight lets a *chunk* win more often, and only a chunk win can attribute a result to a passage — see [Retrieval provenance](#retrieval-provenance). `0` disables the arm. |
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

20 tools. Reference: [docs/TOOLS.md](docs/TOOLS.md).

**Search** — `search_semantic` · `search_text` · `search_regex` · `search_metadata`

**Relate** — `note_related` (nearest notes by meaning, each flagged linked or not)

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

![Benchmark results across six query strata, nDCG@10. Overall 0.834 upstream,
0.900 chunks-only, 0.939 this fork. Deep content, where the answer sits past word
400, goes 0.552 to 0.941. Casual and typo-heavy queries regress to 0.875 with
chunks alone and recover to 0.975 once the summary arm is added. R@1 0.908,
R@5 0.961, R@8 0.961, MRR 0.932. 27.3% of top-8 hits are chunk-attributable and
100% carry a passage.](docs/images/benchmarks.png)

*[docs/BENCHMARKS.md](docs/BENCHMARKS.md) carries the full numbers and methodology*
*and is regenerated from live runs; the figure is a summary of it.*

Same vault, queries, gold labels and embedding model throughout. nDCG@10.
Every figure in this table is from the **live server**.

**Why some numbers elsewhere are slightly higher.** Parameter exploration - chunk
size, `w_sum`, the fusion sweep - was run offline against cached vectors using a
Python replica of the chunker. That replica is not bit-identical to the Rust
implementation and scores roughly 0.005-0.015 higher. It is why the fusion
diagnostic below reads 1.000 on the exact-keyword stratum where this table reads
0.985, and why the `w_sum` sweep reports .9442 overall where the live run reports
.939. The offset applies uniformly to every configuration being compared, so it
never changes which option wins - only the absolute value. Where the two contexts
were checked against each other directly - the share of hits attributable to a
chunk - they agreed to within 0.2 percentage points (27.5% predicted offline,
27.3% measured live).

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

![Seven upstream problems and their fixes: the 400-word truncation, a cache wiped
on every restart, content edits past the truncation never re-embedding, notes with
non-mapping frontmatter dropped from the graph, whole batches sent as one
embedding request, an uncalibrated hybrid blend, and results that could not say
which passage matched. 758 tests pass with 0 failures and 0 ignored on Ubuntu
26.04. Warm start 1.6 ms with 0 re-embeds; cold rebuild 8.4 minutes for 416 notes
and 8,263 chunks; 133 MB peak.](docs/images/engineering.png)

*Every fix below is listed with its regression test in*
*[docs/FIXES.md](docs/FIXES.md).*

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

## Why this exists

It began as a smaller question: which of three embedding models to use for a
personal Obsidian vault. Benchmarking them turned up something no model could
fix. Retrieval embedded one vector per note from a body truncated at 400 words,
and on that vault **170 of 416 notes were longer than that, with 73.8% of all
text sitting past the cut**. The answers were in the notes. The index had never
seen them.

The constant carried no rationale anywhere — not in the code, not in the
documentation, not in the commit that introduced it. Most likely a stale guard
for a 512-token model; the tell is that it applied equally to models with 8192.

Chunking fixed the deep-content case and immediately broke something else:
typo-heavy queries got *worse* than what they replaced, 0.930 down to 0.875. The
summary vector exists because of that regression, and the row showing it stays
in the table.

Most of what followed was learning not to trust the measurements. Two complete
benchmark runs were silently pure BM25, because the server answers with a
lexical-only fallback while its embedding runtime is still warming up — the tell
was results matching a BM25 baseline to three decimal places. A configuration
flag left on cost 0.30 nDCG and would have crowned the wrong architecture. A
change that halved the index for free was rejected on a per-query diff that the
aggregate had hidden.

[docs/METHODOLOGY.md](docs/METHODOLOGY.md) is that list, written down. Every
entry on it produced a wrong number here first.

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
