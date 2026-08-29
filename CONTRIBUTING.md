# Contributing

Thanks for looking at this. It's a fork of
[lstpsche/obsidian-mcp](https://github.com/lstpsche/obsidian-mcp) focused on
passage-level semantic retrieval; see [README.md](README.md) for what changed
and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how it fits together.

## Building

Embedding backends are **not** enabled by default. Building with neither
feature gives you lexical and graph tools but no semantic search — which is a
valid configuration, and also the reason a plain `cargo build` looks
suspiciously fast.

```bash
cargo build --features embeddings-api    # pure Rust over rustls; builds anywhere
cargo build --features embeddings        # local fastembed/ONNX
```

**`--features embeddings` requires glibc ≥ 2.38.** The prebuilt ONNX Runtime
that `ort-sys` downloads references the `__isoc23_*` symbols. Ubuntu 24.04+,
Debian 13+ and Fedora 39+ are fine. On older systems the link fails with
`undefined symbol: __isoc23_strtoll` — confirmed on Ubuntu 22.04 (glibc 2.35),
confirmed working on 26.04 (glibc 2.43). Check before you build a thousand
dependencies:

```bash
ldd --version | head -1
```

`--features embeddings-api` has no such constraint. If you only need semantic
search against Ollama, LM Studio or another OpenAI-compatible endpoint, prefer
it.

## Testing

```bash
cargo test --features embeddings
```

**A bare `cargo test` under-reports.** The daemon integration tests are gated
`#[cfg(all(unix, feature = "embeddings"))]` and communicate over Unix domain
sockets, so they are invisible on Windows and skipped without the feature. Part
of `tests/integration_tests.rs` is gated the same way.

Expected on Linux with `--features embeddings`: **751 pass, 0 fail, 0 ignored**
(670 unit, 1 binary, 8 daemon integration, 72 integration).

Counts differ by platform in both directions — `src/` has Windows-gated blocks
worth 6 unit tests, and one test
(`concurrent_readers_observe_only_complete_atomic_cache_snapshots`) fails on
Windows because Windows denies an atomic file replace while a reader holds the
file open. That failure is pre-existing upstream behaviour, not a regression;
it passes on Linux and macOS. **Please run the suite on Linux before claiming a
test is broken.**

The first `--features embeddings` run downloads `BAAI/bge-small-en-v1.5`
(~133 MB) into `.fastembed_cache`. It is cached afterwards.

CI runs `cargo fmt --check`, `cargo clippy -- -D warnings`, the suite on Linux
and macOS, a Windows `cargo check --all-features`, and the daemon integration
tests on `ubuntu-latest`.

## Changing retrieval

Retrieval changes need evidence, not reasoning alone. [`bench/`](bench/README.md)
contains the harness — it ships without a corpus, because the development vault
is private, so you point it at your own vault and author your own query set.

Two rules that this project learned the hard way:

- **Verify the semantic index is actually ready before measuring.** While the
  embedding runtime warms up the server still answers `search_semantic` through
  a lexical-only fallback. Two complete benchmark runs here were silently pure
  BM25 before a positive readiness probe existed.
- **State your sample.** "One vault, 76 queries, one model" is evidence. It is
  not grounds for a claim about retrieval in general, and this project does not
  make claims like "best Obsidian MCP". If a change wins on your corpus and
  loses on another, both rows belong in the table.

`bench/README.md` has a longer list of things that silently produce wrong
numbers. Every entry on it produced a wrong number here at least once.

## Changing anything near the graph

Wikilinks, backlinks, outgoing-link discovery and orphan/broken detection are
first-class. A retrieval change is expected to leave the link graph bit-for-bit
identical. Prove it rather than assuming it:

```bash
python bench/graph_baseline.py --binary ./before -o before.json
python bench/graph_baseline.py --binary ./after  -o after.json
python bench/graph_diff.py before.json after.json     # exits non-zero on any change
```

This is not ceremony. A note dropped from the index silently disappears from
the graph too, taking its inbound and outbound edges with it — that is
[fix #4](docs/FIXES.md), which cost 42 edges before it was caught.

Note that `wikilinks` output is **not** deterministically ordered (hash-map
iteration), so compare order-insensitively.

## Style

- Match the surrounding code. The Rust here favours small focused functions and
  explicit error types over clever generics.
- **Explain non-obvious constants where they live.** The 400-word truncation
  this fork removed carried no rationale anywhere in the codebase, which is
  precisely why it survived so long. If you introduce a magic number, say why
  it has that value and what would change it.
- Every fix in [docs/FIXES.md](docs/FIXES.md) has a regression test. New fixes
  should too — several of the bugs listed there were found *by* a failing test
  that could easily have been relaxed instead of investigated.
- Don't weaken a failing test to make it pass without understanding why it
  fails. Four real bugs here were caught exactly that way.

## Pull requests

Say what you measured, on what, and what you did not measure. A PR that changes
retrieval and reports one aggregate number is hard to review; per-stratum
numbers and a per-query diff are much easier — and on a small delta, the
per-query diff is the one that tells the truth. It reversed a decision here
that the aggregate had settled the other way.

Bug fixes, platform fixes, documentation and tests are all welcome without
benchmarks.
