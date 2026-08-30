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

**Start here** — [Install](#install) · [Configuration](#configuration) · [Dashboard](#status-dashboard) · [Tools](#tools)

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
| **Semantic retrieval** | meaning-based questions, paraphrase, vague recall | `search_semantic`, `note_related` |
| **Lexical search** | exact strings, identifiers, terminology, regex | `search_text`, `search_regex`, `search_tags`, `search_frontmatter` |
| **Graph navigation** | relationships between notes | `note_links`, `vault_broken_links`, `vault_orphans` |
| **Note operations** | read, create, edit, move, patch | `note_*`, `vault_*`, `periodic_*` |

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

**Available on every path except hybrid.** The daemon reports provenance too:
its IPC protocol used to carry note-level hits only, so a daemon-served
`search_semantic` silently returned none of these fields while the in-process
path returned them in full — the store had the information and nothing asked for
it. It now travels over the wire, resolved by the same code both sides, so a
result describes itself identically however it was served.

The exception is the experimental hybrid path, where a blended rank is not
attributable to a single representation. It omits the fields rather than
guessing, and **absence means "unknown", never `"chunk"`** — branch on
`match_type` being present, not on whether a passage came back.

The three fields are additive on the wire and default to absent, so a client
built against this version still works with a daemon that predates it; it simply
sees no provenance, as before.

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

| variable | default | options / notes |
|---|---|---|
| `OBSIDIAN_EMBEDDINGS` | `false` | `true` \| `false`. Runtime switch for semantic search. **The compile-time half is separate**: build with `--features embeddings` or `embeddings-api`, or this does nothing. |
| `OBSIDIAN_EMBEDDING_PROVIDER` | inferred from features | `local` (fastembed, in-process ONNX) \| `api` (any OpenAI-compatible endpoint). |
| `OBSIDIAN_EMBEDDINGS_MODEL` | `BAAI/bge-small-en-v1.5` | HuggingFace model for the **local** provider. Ignored by the API provider, which uses `OBSIDIAN_EMBEDDING_API_MODEL`. |
| `OBSIDIAN_EMBEDDING_API_BASE` | `https://api.openai.com/v1` | Any OpenAI-compatible `/v1` endpoint — Ollama is `http://localhost:11434/v1`. Falls back to `OPENAI_BASE_URL`. |
| `OBSIDIAN_EMBEDDING_API_MODEL` | — | Model name at that endpoint. Falls back to `OPENAI_MODEL`. |
| `OBSIDIAN_EMBEDDING_API_KEY` | — | **Required** for the API provider, even when the endpoint ignores it (Ollama accepts any value). Falls back to `OPENAI_API_KEY`. |
| `OBSIDIAN_EMBEDDING_DIM` | probed at startup | Set explicitly to skip the probe request. Must be > 0. |
| `OBSIDIAN_EMBEDDING_QUERY_PREFIX` | `"query: "` | **Set both empty for prefix-free models such as bge-m3.** |
| `OBSIDIAN_EMBEDDING_DOC_PREFIX` | `"passage: "` | Applied unconditionally — unlike servers that infer it from the model name. |
| `OBSIDIAN_EMBEDDING_TLS_VERIFY` | `true` | `true` \| `false`. Only turn off against a host you control. |
| `OBSIDIAN_EMBEDDING_CA_CERT` | none | PEM path, for an endpoint behind a private CA. |
| `OBSIDIAN_EMBED_BATCH` | `16` | Chunks per provider request; large batches overrun local inference servers. |
| `FASTEMBED_CACHE_DIR` | under the semantic home | Where the **local** provider caches downloaded model files. |

Asymmetric models (Arctic, E5, Nomic, Qwen) expect these prefixes; sending none
silently costs accuracy. Here the query prefix alone was worth nDCG 0.675 → 0.706.

### Server

| variable | default | options / notes |
|---|---|---|
| `OBSIDIAN_VAULT_PATH` | **required** | Absolute path to the vault root. Can instead be passed as the first CLI argument. |
| `OBSIDIAN_TRANSPORT` | `stdio` | `stdio` \| `http`. `--http` on the command line wins over this. |
| `OBSIDIAN_HTTP_HOST` | `127.0.0.1` | Bind address. Anything other than loopback exposes the vault to your network — there is no authentication. |
| `OBSIDIAN_HTTP_PORT` | `37842` | |
| `OBSIDIAN_WATCH` | `true` | `true` \| `false`. Re-index on filesystem change. Setting it `false` also disables the semantic daemon. |
| `OBSIDIAN_TANTIVY` | `true` | `true` \| `false`. BM25 index; `search_text` and `search_regex` need it. |
| `OBSIDIAN_TOOLS` | `full` | A profile (`full` \| `core` \| `read` \| `minimal`), a comma-separated allow-list of tool names, or a `!`-prefixed deny-list. See [Tools](#tools). |
| `OBSIDIAN_EXCLUDE_PATHS` | none | Comma-separated globs. A trailing `/` expands to `/**`, so `Archive/` is enough. Merged with the vault's `.obsidian-mcp/ignore` file. |
| `OBSIDIAN_MCP_DATA` | `{vault}/.obsidian-mcp` | Move the index and cache off the vault. |
| `OBSIDIAN_LOG_LEVEL` | `info` | Any `tracing` filter: `error` \| `warn` \| `info` \| `debug` \| `trace`, or a per-module directive such as `obsidian_mcp::vault=debug`. Logs go to stderr. |

### Semantic runtime and daemon

The daemon holds the vector index in its own process so it survives a server
restart. It is spawned automatically; these only matter if you want to change
that.

| variable | default | options / notes |
|---|---|---|
| `OBSIDIAN_SEMANTIC_MODE` | `auto` | `auto` \| `local` \| `daemon`. `auto` uses the daemon when one is reachable and falls back in-process; `local` never uses it; `daemon` refuses to fall back. All three report [retrieval provenance](#retrieval-provenance) identically — the daemon carries it over IPC. |
| `OBSIDIAN_SEMANTIC_MODEL` | `BAAI/bge-small-en-v1.5` | Identity label the daemon records and matches against. It does **not** select the API model — that is `OBSIDIAN_EMBEDDING_API_MODEL`. A client whose label differs is refused, which is what stops two models sharing one index. |
| `OBSIDIAN_SEMANTIC_PREFETCH` | `50` | Candidates fetched before filtering. Clamped to `[1, 1000]`. |
| `OBSIDIAN_SEMANTIC_HOME` | `%APPDATA%/obsidian-semantic`, `$XDG_STATE_HOME/obsidian-semantic` | Where the daemon keeps manifests, sockets and per-vault indexes. |
| `OBSIDIAN_SEMANTIC_DAEMON_PATH` | sibling of the binary | Override the daemon executable. |
| `OBSIDIAN_SEMANTIC_ENDPOINT` | derived from the home path | Unix socket path or Windows named pipe. |
| `OBSIDIAN_SEMANTIC_CONNECT_TIMEOUT_MS` | `2000` | Clamped to `[100, 60000]`. |
| `OBSIDIAN_SEMANTIC_CONNECT_RETRIES` | `2` | Clamped to `[0, 10]`. |
| `OBSIDIAN_SEMANTIC_RETRY_BACKOFF_MS` | `250` | Clamped to `[50, 60000]`. |
| `OBSIDIAN_SEMANTIC_DAEMON_DOWNLOAD_URL` | none | Fetch the daemon binary rather than resolving a sibling. |
| `OBSIDIAN_SEMANTIC_ALPHA` | `0.25` | Alias for `OBSIDIAN_HYBRID_ALPHA`; this one is read first. |

### Changing configuration

Everything is environment variables — there is no config file. Set them in
whatever launches the server.

**One-off, current shell:**

```bash
OBSIDIAN_VAULT_PATH=/path/to/vault OBSIDIAN_TOOLS=read obsidian-mcp --http
```

```powershell
$env:OBSIDIAN_VAULT_PATH = "D:\Vault"
$env:OBSIDIAN_TOOLS = "read"
obsidian-mcp --http
```

**Persistently** — put them in a launcher script next to the binary, which also
records *why* each value is set:

```bash
#!/usr/bin/env bash
export OBSIDIAN_VAULT_PATH="$HOME/Vault"
export OBSIDIAN_EMBEDDINGS=true
export OBSIDIAN_EMBEDDING_PROVIDER=api
export OBSIDIAN_EMBEDDING_API_BASE=http://localhost:11434/v1
export OBSIDIAN_EMBEDDING_API_MODEL=snowflake-arctic-embed2:latest
export OBSIDIAN_EMBEDDING_API_KEY=ollama       # Ollama ignores it, but one is required
export OBSIDIAN_TOOLS=read
exec obsidian-mcp --http
```

**In an MCP client**, if it launches the server over stdio, use its `env` block:

```jsonc
{
  "mcpServers": {
    "obsidian": {
      "command": "obsidian-mcp",
      "env": { "OBSIDIAN_VAULT_PATH": "/path/to/vault", "OBSIDIAN_TOOLS": "read" }
    }
  }
}
```

**Confirm what actually took effect** rather than assuming — under HTTP the
server reports its own resolved configuration:

```bash
curl -s http://127.0.0.1:37842/api/info | jq '.config, .embeddings'
```

Two of these are read at *startup only*, so a change needs a restart: the tool
filter and the exclusion patterns. Changing `OBSIDIAN_EXCLUDE_PATHS` also
changes the set of indexed notes, so the next start re-embeds the difference.

> **The daemon inherits the environment of whatever spawned it.** If you change
> an embedding or exclusion variable, restart the *server*; the daemon it
> launches picks the new value up from there. A daemon left running from an
> earlier server keeps the old one.

`obsidian-mcp --help` prints the full list as the binary sees it, which is the
authority if this table and the binary ever disagree.

## Status dashboard

Under HTTP transport the server also serves a status page at **`/dashboard`** —
no bundler, no CDN, no assets, just one embedded file.

It answers what otherwise needs a hand-rolled MCP handshake to ask: whether the
semantic index is ready or still warming, how many notes are indexed versus
pending, which embedding model and endpoint are actually in use, which paths are
excluded, and which tools are exposed — each with its parameters, types and
required flags.

The tool list is built from the same router the request path dispatches against,
so it shows what clients are genuinely served rather than what the configuration
merely asked for. When a tool filter is active, hidden tools are absent here too.

| endpoint | purpose |
|---|---|
| `/dashboard` | the page |
| `/api/info` | the same data as JSON — config, index state, daemon state, tool schemas |
| `/health` | liveness plus index readiness, for scripts |

`embeddings_ready` is the field worth gating on: a warming index still answers,
and that is the one failure mode that resembles success.

**It also reports what the *daemon* believes.** The daemon keeps its own index of
the same vault, and when the two were configured differently they disagreed in
silence — the server indexed 476 notes, the daemon 507, and the only symptom was
semantic results from folders that had been excluded. Neither number is wrong on
its own; only the pair shows the fault. So `/api/info` carries both and states
the comparison outright:

```jsonc
"daemon": {
  "in_use": true,
  "total_notes": 476,
  "indexed_notes": 471,
  "model_name": "snowflake-arctic-embed2:latest",
  "agrees_with_server": true      // false ⇒ a configuration fault, not a retrieval one
}
```

When it is `false` the page shows a banner naming both counts, rather than
leaving the disagreement to be inferred from an odd search result.

## Tools

27 tools. Full reference: [docs/TOOLS.md](docs/TOOLS.md).

**Search** — `search_semantic` · `search_text` · `search_regex` · `search_tags` · `search_frontmatter`

**Relate** — `note_related` (nearest by meaning, each flagged linked or not) · `note_links` (both directions at once) · `vault_broken_links` · `vault_orphans`

**Read** — `note_read` · `note_read_many` · `note_metadata` · `note_frontmatter` · `note_patch_targets`

**Write** — `note_create` · `note_write` · `note_insert` · `note_patch` · `note_frontmatter_edit` · `note_move` · `note_delete`

**Periodic** — `periodic_get` · `periodic_list` · `periodic_create`

**Vault** — `vault_list` · `vault_info` · `open_in_obsidian`

Two rules shape that list, and both exist because an agent — not a person — is
reading it.

**One tool does one thing.** No tool multiplexes behaviours behind an `action`
or `type` parameter. That is partly legibility, since a tool whose return shape
depends on an argument is hard to reason about, and partly correctness:
`OBSIDIAN_TOOLS` filters by tool *name*, so a tool that both reads and writes
cannot be filtered — admitting it for its read half admits its write half. That
is not hypothetical. `frontmatter` bundled get/set/remove and sat in the `read`
profile, which meant a "read-only" server would happily rewrite a note's
frontmatter; `periodic` bundled get/list/create, so excluding one write cost you
both reads. A test now asserts the `read` profile contains nothing that can
write.

**Descriptions say when, not just what.** Each one names the sibling to prefer
in the cases it does not cover, because with five ways to search a vault, the
choice is most of the work and a wrong pick returns an empty result rather than
an error. The server's own instructions carry the same decision procedure.

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
| `OBSIDIAN_TOOLS` never enforced | **a read-only server still executed every write tool** |
| `read` profile admitted tools that write | **`frontmatter` set/remove reachable on a read-only server** |
| Daemon ignored `OBSIDIAN_EXCLUDE_PATHS` | excluded folders absent from `search_text`, still returned by `search_semantic` |
| Self-updater reinstalling the upstream package | removed — 3,257 lines |

> The tool-filter fix is worth spelling out, because the failure was silent and
> the consequences are not retrieval quality but data integrity.
> `#[tool_handler]` defaults its router to `Self::tool_router()`, which builds a
> **fresh** router on every request. The disabled set applied in `new` lives on
> `self.tool_router`, so the handler never saw it: the filter parsed, logged and
> stored correctly while `tools/list` advertised all 20 tools and `call_tool`
> dispatched them. A server started with `OBSIDIAN_TOOLS=read` would delete a
> note on request. The fix is `#[tool_handler(router = self.tool_router)]`.
>
> The pre-existing tests passed throughout, because they asserted on
> `server.tool_router` — the field, which really was disabled — rather than on
> what a client is served. The regression tests added here drive the wire
> protocol instead, and fail without the fix.
>
> Fixing enforcement then exposed a second hole underneath it. `OBSIDIAN_TOOLS`
> matches on tool *names*, but two tools multiplexed reading and writing behind
> an `action` parameter, so a name-based filter could not separate them.
> `frontmatter` (get/set/remove) was in the `read` profile, which meant a
> read-only server would still rewrite a note's frontmatter on request;
> `periodic` (get/list/create) was excluded entirely, so blocking one write cost
> two reads. Both are now split into single-purpose tools, and
> `read_profile_admits_nothing_that_can_write` asserts the invariant that makes
> the filter meaningful.

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
- **`note_links` output is not deterministically ordered** (hash-map iteration),
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

### Built on

| | |
|---|---|
| **[rmcp](https://crates.io/crates/rmcp)** | The official Rust MCP SDK — protocol, tool router, and both transports. |
| **[Tantivy](https://github.com/quickwit-oss/tantivy)** | BM25 full-text index behind `search_text` and `search_regex`. |
| **[fastembed-rs](https://github.com/Anush008/fastembed-rs)** | In-process ONNX embeddings for the `local` provider. |
| **[notify](https://github.com/notify-rs/notify)** | Filesystem watching for incremental re-indexing. |
| **[schemars](https://github.com/GREsau/schemars)** | Derives the JSON Schemas published for every tool's inputs and outputs. |
| **[Snowflake Arctic Embed 2.0](https://huggingface.co/Snowflake/snowflake-arctic-embed-l-v2.0)** | The embedding model the reported numbers were measured with, served locally through **[Ollama](https://ollama.com)**. |

### Given back

- **[lstpsche/obsidian-mcp#25](https://github.com/lstpsche/obsidian-mcp/issues/25)**
  — `OBSIDIAN_TOOLS` is not enforced. Found here, reported upstream with a
  reproduction and the one-line fix, since it affects any deployment relying on
  a read-only profile.

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
