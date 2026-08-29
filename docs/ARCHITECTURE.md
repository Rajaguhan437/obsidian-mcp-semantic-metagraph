# Architecture

Three independent layers over a vault read directly from disk: a **lexical
index**, a **semantic index**, and a **link graph**. They are separate on purpose
— an agent picks the one that fits its question rather than getting one blended
ranking.

```
                     ┌────────────── MCP tool surface (19 tools) ──────────────┐
                     │                                                          │
  search_semantic ───┤ semantic:  chunks + summary, weighted max                │
  search_text        │ lexical:   Tantivy BM25, full body, field boosts         │
  search_regex   ────┤                                                          │
  search_metadata    │ graph:     LinkResolver + backlink maps                  │
  wikilinks      ────┤                                                          │
  note_* / vault_*   │ vault:     VaultIndex, incremental on file events        │
                     └──────────────────────────────────────────────────────────┘
```

## Semantic layer

### What gets embedded

Per note: **N chunks + 1 summary**.

**Chunks** (`src/vault/chunker.rs`) are heading-scoped. The body is split at
heading boundaries; each section is packed to a ~1000-character target with 200
overlap, and oversized sections are split at the best available boundary
(paragraph break, then sentence end, then newline, then space). Each chunk is
embedded as:

```
{title}
{H1 > H2 > H3}
{chunk text}
```

Guarantees the chunker holds regardless of input: code fences are tracked so `#`
inside a fence is never a heading; no chunk exceeds a hard ceiling; the walk
always makes forward progress even if overlap exceeds the target; and splits
never land mid-character in UTF-8.

**The summary** is `title + all headings + first 400 words` — upstream's original
`prepare_embed_text` output, preserved deliberately.

### Storage

Both live in the existing `HashMap<PathBuf, EmbeddingEntry>` store, keyed with a
NUL separator that cannot occur in a real path:

```
notes/alpha.md\0 0     chunk 0
notes/alpha.md\0 1     chunk 1
notes/alpha.md\0 s     summary
```

This was chosen over restructuring the store because it preserves the on-disk
cache format, its magic/schema versioning, its integrity checks, and the daemon
protocol — none of which needed to change. The cost is that every site comparing
a key to a note path must resolve one to the other; missing two such sites caused
fixes #2 and #3 in [FIXES.md](FIXES.md).

`EMBEDDING_INPUT_VERSION` is part of the cache identity, so bumping it (now 3)
invalidates older caches automatically rather than silently mixing
representations.

### Scoring

```
score(note) = max( max_i cos(q, chunk_i),  w_sum · cos(q, summary) )
```

**Why `max` and not a weighted sum.** Max is monotone: adding the summary can only
raise a note's score, never lower it. A weighted sum averages a note's best chunk
against a summary that may not mention the answer at all — reintroducing exactly
the dilution the chunking was meant to fix. This was derived from the failure
mode, not found by sweeping.

**Why `w_sum > 1`.** `max` over ~20 chunks is systematically higher than a single
summary score, so the two arms are not on equal footing; the weight compensates.
This is a correction, not a tuned constant — normalising for chunk count would be
more principled. Measured plateau is [1.20, 1.30]; at 1.32 deep-content retrieval
degrades sharply.

Note this makes `score_for` a **ranking score, not a cosine** — it can exceed 1.0.
Anything blending it with another arm must call `blend_score_for()`, which
rescales to [0,1] monotonically.

### Indexing

Reconciliation batches 32 notes, chunks them, and hashes the **full body plus
chunk config** to decide whether re-embedding is needed. Chunk texts are flattened
across the batch and sent to the provider in bounded sub-batches
(`OBSIDIAN_EMBED_BATCH`, default 16) with exponential-backoff retry — a 32-note
batch is hundreds of chunks, and sending them in one request overruns local
inference servers.

A note's chunks are committed atomically: old chunks are removed first, so a note
that shrinks cannot leave orphaned vectors.

## Lexical layer

Tantivy, in RAM, rebuilt at startup, with **zero embedding dependencies** — which
is why the two layers could be reasoned about independently. One document per
note, `en_stem` tokenizer, field boosts: title 5.0, tags 4.0, headings 3.0,
frontmatter 2.0, body 1.0. The body is indexed **in full** — it was never subject
to the 400-word truncation.

Exposed directly as `search_text`, `search_regex`, `search_metadata`.

## Graph layer

`LinkResolver` maps link targets to paths (by stem and by path, case- and
Unicode-normalisation-insensitive). `backlinks: HashMap<PathBuf, HashSet<PathBuf>>`
is maintained incrementally on create, modify, rename and delete, with a full
rebuild as fallback.

`WikiLink` preserves `raw`, `target`, `heading` (`[[note#heading]]`), `block_ref`
(`[[note#^blockid]]`), `alias` (`[[note|alias]]`) and `line`.

`wikilinks` answers four queries — **backlinks**, **outgoing**, **broken**,
**orphans** — with `OrphanStatus` distinguishing notes with no links at all from
notes whose only links are broken.

The graph is independent of retrieval: no graph code references the embedding
layer, and vice versa. That independence is what made the retrieval rework safe,
and it is verified by a captured graph baseline compared before and after every
phase.

## Retrieval paths

Three, selected by the caller:

| path | how | when |
|---|---|---|
| **semantic** (default) | `search_semantic`, `lexical_prefetch=false` | meaning-based questions |
| **lexical** | `search_text` / `search_regex` / `search_metadata` | exact strings, identifiers |
| **hybrid** (opt-in) | `OBSIDIAN_LEXICAL_WEIGHT > 0`, or legacy `lexical_prefetch=true` | see below |

Hybrid is **off by default** because it did not beat semantic-only on the
development corpus — BM25 ranked nothing semantics missed. When enabled it uses
**union candidate generation** (semantic scores the whole store; lexical
contributes its own hits) rather than the legacy BM25 gate, which could never
recover a note BM25 missed. Both arms are calibrated by their own maximum with
the floor left at zero — not min-max, which lifts every query's weakest candidate
to 0 and hands irrelevant documents a flat bonus.

The legacy `lexical_prefetch=true` path is retained for compatibility. Note it
re-ranks only the top `DEFAULT_PREFETCH_COUNT = 50` BM25 hits; on the development
corpus that cap cost up to 0.30 nDCG.

## Daemon

`obsidian-semanticd` is a **transport and process-lifecycle wrapper around this
same engine**, not a second implementation — `daemon/vault_context.rs` holds an
`EmbeddingRuntime` and calls the same `semantic_scores_for_paths` and
`score_for`, so it inherits every change here automatically.

Its purpose is one shared model and model cache across several clients (this
server plus Obsidian plugin clients), over JSON-RPC IPC. That value is real with a
**local** (fastembed) provider and largely redundant with an **API** provider,
where the inference server is already the shared model host.

`OBSIDIAN_SEMANTIC_MODE` selects `local`, `daemon`, or `auto` (try daemon, fall
back to local). It is retained because removing it would break the local
multi-client case while gaining nothing for the API case. Protocol and layout:
[semantic-runtime/](semantic-runtime/).
