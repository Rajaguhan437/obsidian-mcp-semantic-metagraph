# Benchmark methodology and results

> **Scope of this evidence.** One vault: **416 Obsidian notes, ~2.9 MB**, prose-like
> personal notes with descriptive headings and many short, precisely-titled
> sections. **76 queries. One embedding model** (snowflake-arctic-embed2 via
> Ollama). Every number below is from that corpus.
>
> This is evidence, not a universal claim. Several findings were measurably
> corpus-dependent and are flagged as such. Re-measure before adopting any
> default here. No claim is made that this is the best Obsidian MCP server, or
> that these results generalise.

## Methodology

### Corpus

An isolated byte-copy of the vault, so production was never touched and every
configuration saw identical input. All variants used the same corpus, the same
queries, the same gold labels, and the same embedding model.

### Query set

76 queries, built in two passes:

1. **Authored from real content.** Agents read a spread of actual notes across six
   content domains and wrote queries a vault owner might realistically type,
   recording the gold note and a verbatim evidence snippet.
2. **Adversarially verified.** A second pass re-opened every gold file on disk and
   rejected queries that were unanswerable from the note, too generic (many notes
   answered equally well), trivially matching the title, or pointing at a
   non-existent path.

**84 authored, 76 survived.** Every surviving gold path was verified present in
the corpus.

Query types were mixed deliberately: factual (21), conceptual (23), paraphrase
(17, written to avoid the note's own vocabulary), and casual/typo (15, mimicking
how the vault owner actually types — *"wat books i hv abandoned n wich 1 am
currently readng"*).

### Strata

Aggregate scores hide the effects that matter, so results are reported six ways:

| stratum | n | what it isolates |
|---|---|---|
| overall | 76 | headline |
| **deep content** | 17 | answers located **past word 400** — the only stratum the truncation fix can move |
| casual / typo | 15 | robustness to real typing |
| paraphrase | 17 | vocabulary independence |
| low lexical overlap | 27 | *computed*, not authored: token overlap below 0.55 |
| exact keyword | 25 | computed: overlap at or above 0.75 |

The deep-content stratum was constructed by locating each answer's evidence
snippet inside its gold note and recording its **word offset**. Without it, an
effect of 0.552 → 0.941 would have been largely invisible in the aggregate and
unattributable to the change.

### Measurement

- **Cost accounting** used a counting reverse proxy in front of the inference
  server, so embedding calls and batch sizes are measured rather than inferred.
- **Readiness** was established by querying the semantic index and requiring a
  real answer. Inferring readiness from "traffic went quiet" produced two entirely
  invalid runs early on — the server answers with a lexical-only fallback while
  warming up.
- **Significance** where quoted comes from 10,000 paired bootstrap resamples.
- Every configuration was measured at its **best** setting. An early comparison
  was invalidated by a `lexical_prefetch` flag that re-ranked only BM25's top 50;
  disabling it moved configurations by up to 0.30 nDCG — more than any
  architectural change in the project.

Full account of what went wrong, including four bugs in the harness itself:
[PROJECT_LOG.md](PROJECT_LOG.md) and [METHODOLOGY.md](METHODOLOGY.md).

## Results

### Headline, nDCG@10

| stratum | upstream | chunks only | **this fork** |
|---|---|---|---|
| overall | 0.834 | 0.900 | **0.939** |
| deep content | 0.552 | 0.941 | **0.941** |
| casual / typo | **0.930** | 0.875 | **0.975** |
| paraphrase | 0.714 | 0.765 | **0.794** |
| low lexical overlap | 0.818 | 0.817 | **0.857** |
| exact keyword | 0.793 | 0.960 | **0.985** |

| overall metric | upstream | chunks only | this fork |
|---|---|---|---|
| R@1 | 0.789 | 0.855 | **0.908** |
| R@5 | 0.868 | 0.934 | **0.961** |
| R@8 | 0.882 | 0.934 | **0.961** |
| MRR | 0.822 | 0.888 | **0.932** |

**The middle column is the important one.** Chunking alone beat upstream overall
while *losing* to it on typo-heavy queries (0.930 → 0.875). Reporting only the
aggregate would have hidden that regression, and the summary arm — the fix — would
never have been found.

### Cost

| | upstream | chunks only | this fork |
|---|---|---|---|
| vectors | 407 | 8,297 | 8,709 |
| indexing time | 36 s | 331 s | 402 s |
| embedding calls | 14 | 525 | 551 |
| peak RSS | 27 MB | 68 MB | 70 MB |

The summary arm costs +5% vectors and +21% indexing time over chunks alone.
Warm restart reuses the cache: **3.2 s with zero documents re-embedded**.

### Embedding model comparison

Best configuration per model, on this corpus:

| model | nDCG | R@1 | signal margin | VRAM |
|---|---|---|---|---|
| **snowflake-arctic-embed2** + `query:` prefix | **0.706** | **0.842** | **0.189** | 633 MB |
| arctic-embed2, no prefix | 0.675 | 0.816 | 0.173 | 633 MB |
| nomic-embed-text-v2-moe + prefix | 0.658 | 0.803 | 0.142 | 559 MB |
| bge-m3 | 0.630 | 0.776 | 0.115 | 633 MB |

"Signal margin" is mean gold similarity minus mean similarity of the top-50
non-relevant chunks. bge-m3's noise floor sits at 0.506 against 0.621 for relevant
text, so it cannot support a similarity threshold. Its multilingual training is
also wasted here — the corpus is effectively monolingual English. **Leaderboard
rank did not predict the winner on this corpus.**

### Parameter sweeps

**Summary weight** — 36 configurations. Quality is flat across **[1.20, 1.30]**
and collapses at **1.32** (deep content 0.941 → 0.919, paraphrase 0.794 → 0.772).
The default sits mid-plateau at 1.25 rather than at the marginally higher-scoring
edge, because a wide safe operating range is worth more than 0.005 for a default.

**Chunk size** — 600/120, 1000/200, 1500/150, 2000/300 tested. 1000/200 holds
deep content at 0.941 across *every* summary weight; 2000/300 only reaches it once
tuned. The wider safe range decided it.

**Section packing** — halves the index (8,263 → 4,076 vectors, −50.7% storage and
embedding calls, query scoring 1.47 ms → 0.70 ms) with **no aggregate quality
loss**. Rejected anyway: the per-query diff showed 73 of 76 queries unchanged, and
the one meaningful change was a **regression** — a natural query fell from 2nd to
3rd because packing dissolved the 459-character section that answered it into a
neighbouring list. This corpus's median section-bound chunk is 248 characters, and
those short titled sections *are* the answers. **A vault of long prose would
likely benefit; it ships as an option.**

**Hybrid fusion** — 28 configurations (weighted RRF, z-score sum, unit-normalised
sum, bounded lexical bonus), union candidate generation, per-arm weights. None
beat semantic-only. Decisive diagnostic: BM25 ranked **0 of 76** queries that
semantics missed, while it could spoil 19. Semantic beat BM25 even on the
exact-keyword stratum, **1.000 vs 0.891**. **This is the most corpus-dependent
result in the project.**

## Reproducing

The harness is not vendored in this repository — it is specific to the
development environment and would not run elsewhere unchanged. What it does is
fully described above, and reproducing it requires:

1. A vault, and a query set built by the two-pass process described.
2. Strata computed from the corpus (lexical overlap and answer word-offset are
   both derived, not authored).
3. A counting proxy in front of your inference server for cost figures.
4. Positive readiness checks before every run.

If reproducing on your own vault, expect different constants. The **methods**
transfer; the **numbers** do not.
