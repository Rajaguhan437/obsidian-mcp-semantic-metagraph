# Benchmark harness

The evaluation used to derive this fork's defaults. It ships **without a
corpus**: the development vault is a private set of personal notes, and the
query set embeds note paths and quoted note text, so neither can be published.
You point the harness at your own vault and author your own queries.

That is the honest arrangement, but be clear about what it means: **you cannot
reproduce the numbers in [../docs/BENCHMARKS.md](../docs/BENCHMARKS.md)**. You
can run the same procedure on your own corpus and get numbers that are valid
for it. Retrieval results do not transfer between corpora, which is the whole
reason the defaults here are stated as corpus-dependent.

## What's here

| file | purpose |
|---|---|
| `benchlib.py` | config, MCP client, metrics, server runner — everything else imports this |
| `run_eval.py` | the main evaluation; reports nDCG/R@k/MRR per stratum, with optional A/B compare |
| `make_queries.py` | turns an authored query set into a stratified one |
| `proxy.py` | counting reverse proxy — exact embedding call and batch counts |
| `chunk_sweep.py` | offline sweep of chunk size / overlap / summary weight, with a Pareto front |
| `graph_baseline.py` | capture the full link graph (backlinks, outgoing, broken, orphans) |
| `graph_diff.py` | compare two graph captures; exits non-zero on any change |

## Setup

```bash
pip install -r bench/requirements.txt

export BENCH_VAULT=/path/to/a/vault/copy
export BENCH_BINARY=/path/to/obsidian-mcp
export BENCH_OUT=./bench-out
```

Full configuration is documented at the top of `benchlib.py`. The defaults
assume Ollama on `127.0.0.1:11434` with `snowflake-arctic-embed2`.

**Use a copy of your vault, not the vault itself.** Runs start a server that
indexes the whole tree, and experiments delete index data between
configurations.

## Authoring a query set

Write a JSON list. Each record needs a query, its gold note(s), and a sentence
copied verbatim from the gold note:

```json
[
  {
    "query": "how did we decide the retry budget for the ingest worker",
    "qtype": "paraphrase",
    "gold_paths": ["Engineering/Ingest worker design.md"],
    "answer_evidence": "we settled on five attempts with exponential backoff"
  }
]
```

- **`qtype`** is free-form. `paraphrase` and `casual` are the two the default
  strata name; `casual` is where you put typo-ridden, half-remembered phrasing.
- **`gold_paths`** are vault-relative. If you authored them with a leading
  folder, pass `--strip-prefix`.
- **`answer_evidence`** is the important one. It is how `make_queries.py`
  locates *where in the note* the answer lives, which produces the
  `beyond_400` stratum — queries whose evidence sits past word 400. That
  stratum is what separates a real chunking improvement from noise, because it
  is exactly the text a whole-note representation truncated at 400 words cannot
  see. Without it, a chunking change looks like measurement noise.

Then:

```bash
python bench/make_queries.py authored.json -o bench-out/queries.json
```

It prints the stratum counts and warns if no query lands past word 400, or if a
gold path does not exist under the vault.

Derived fields it adds: `gold_rel`, `lex` (query/gold token overlap, which
drives the low-overlap and exact-keyword strata), `gold_words`,
`evidence_word`, `long_note`, `beyond_400`.

**Aim for enough queries that a stratum is not two notes wide.** The set behind
this fork's numbers was 76 queries, and several strata were still small enough
that a single query moved a decimal place.

## Running

Start the counting proxy first if you want embedding-cost numbers:

```bash
python bench/proxy.py
```

Then:

```bash
python bench/run_eval.py --label baseline
python bench/run_eval.py --label candidate --compare bench-out/results/baseline.json
```

Pass server settings through `--env`, repeatable:

```bash
python bench/run_eval.py --label wider --env OBSIDIAN_CHUNK_CHARS=1500
```

### Sweeping parameters cheaply

`chunk_sweep.py` never starts the server. It chunks in Python, embeds each
configuration once, caches the vectors, and scores every `(config, w_sum)` pair
as matrix arithmetic — so adding a `w_sum` afterwards costs nothing.

```bash
python bench/chunk_sweep.py --configs 800x160,1000x200 --wsums 0,1.0,1.2,1.3
```

### Checking the graph is untouched

Retrieval changes should not perturb the link graph. Prove it rather than
assuming it:

```bash
python bench/graph_baseline.py --binary ./before -o bench-out/g_before.json
python bench/graph_baseline.py --binary ./after  -o bench-out/g_after.json
python bench/graph_diff.py bench-out/g_before.json bench-out/g_after.json
```

`graph_diff.py` exits non-zero on any change, so it can gate a refactor.

## Things that will silently give you wrong numbers

Every one of these produced a wrong result during this project.

- **Inferring readiness from silence.** While the embedding runtime warms up,
  the server still answers `search_semantic` — through a lexical-only fallback.
  Two complete runs were measured as pure BM25 before a positive readiness
  probe existed. The tell was results matching a BM25 baseline to three
  decimals. `benchlib.wait_until_ready` probes positively; keep it that way.
- **`lexical_prefetch: true`.** The legacy path re-ranks only the top 50 BM25
  hits, so anything BM25 misses is unrecoverable. Worth up to 0.30 nDCG here —
  enough to make the wrong architecture look like the winner. `run_eval.py`
  sets it false.
- **Resetting the proxy by rewriting `stats.json`.** The counters live in the
  proxy's memory; the file is only a mirror. Reset via `/__reset`.
- **Comparing runs where anything else moved.** Same vault, same queries, same
  gold, same model, same prefixes. If you changed two things, you measured
  neither.
- **Reading a peak as a setting.** Prefer a plateau. If a metric swings sharply
  between neighbouring parameter values, that is noise.
- **Quoting an aggregate when the delta is small.** Diff per query. A per-query
  diff reversed one decision in this project that the aggregate had settled the
  other way.
