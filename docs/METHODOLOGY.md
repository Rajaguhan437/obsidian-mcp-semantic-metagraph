# Evaluating a retrieval system without fooling yourself

Transferable lessons from building this fork. Written to be useful on other
projects, not just this one. The project-specific record lives in
`PROJECT_LOG.md`.

Every failure mode below actually happened here and produced a confident, wrong
answer before it was caught.

---

## 1. Measure the incumbent's ceiling before comparing anything

The original task was "which of three embedding models is best". Three indexes
had been built and compared, with inconclusive results.

The first useful measurement was not a model score. It was: **for how many test
queries does the answer note exist in the index at all?** Answer: 14-25%. The
indexes were near-empty and built over *different corpora*. Every model
comparison run on them was noise.

**Rule:** before comparing systems, verify each one can physically produce the
right answer. A ceiling check is cheap and frequently ends the investigation.

---

## 2. Build the query set adversarially, then stratify it

A benchmark is only as good as its gold labels.

- **Author from real content.** Queries were written by agents that had actually
  read the notes, not invented from titles.
- **Verify adversarially.** A second pass re-opened every gold file and rejected
  queries that were unanswerable, too generic, trivially title-matching, or whose
  gold path did not exist. 84 authored, **76 survived**.
- **Stratify by what discriminates.** Aggregate nDCG hides everything. The strata
  that earned their keep here:
  - *deep content* — answers past the point a truncating system can see
  - *paraphrase* — deliberately avoids the note's vocabulary
  - *casual/typo* — written the way the user actually types
  - *lexical overlap* — computed, not authored, splitting keyword-findable from
    genuinely semantic
  
  The deep-content stratum was the entire reason the project succeeded: it was
  the only place the fix showed up cleanly (0.552 -> 0.941) while the aggregate
  moved far less.

**Rule:** design at least one stratum that *only* the hypothesis under test can
move. If no such stratum exists, you cannot attribute a win.

---

## 3. The self-deception failure modes

### 3.1 A system that degrades silently instead of failing

The server answered queries with a lexical-only fallback while its embedding
runtime warmed up. The harness inferred readiness from "the embedding traffic
went quiet". Result: an entire benchmark run measured pure BM25 while appearing
to measure hybrid retrieval.

**Tell:** the numbers matched a known baseline *to three decimal places*. A
coincidence that clean is never a coincidence.

**Rule:** wait for a **positive** readiness signal — query the thing and require
a real answer. Never infer readiness from absence of activity.

### 3.2 Silent fallbacks in the system under test

`load_for_space(...).ok()` swallowed cache-load errors, so a rejected cache
quietly re-embedded the entire corpus — indistinguishable from a slow first run.

**Rule:** when a component can degrade rather than fail, make it log loudly
before you benchmark it. Then read the log.

### 3.3 Counters that survive their reset

Cost accounting reset a stats *file* while the proxy kept counting in memory, so
every run reported the sum of all previous runs ("406 notes, 10,257 embeddings").

**Rule:** reset state at the source that owns it, and sanity-check magnitudes
against something you can derive independently.

### 3.4 A configuration flag that silently caps the ceiling

The search API had `lexical_prefetch`, which re-ranks only BM25's top 50. Any
note BM25 missed was unrecoverable regardless of semantics. That one flag was
worth **0.30 nDCG** — larger than every architectural change in the project
combined.

**Rule:** before concluding "architecture X is worse", verify you measured it at
its *best* configuration. Sweep the obvious knobs first; a candidate-generation
bottleneck will masquerade as a model quality difference.

---

## 4. Trust failing tests over your own explanation

Six fixtures failed after an architectural change. The tempting reading was "old
tests encode the old contract". That was true of one of them. The other five were
reporting **real defects**:

- a cache-retention function keyed on the wrong identifier, wiping the entire
  cache on every startup
- a scoring function that returned `0.0` for every note, silently reducing hybrid
  search to lexical-only

Both would have shipped. Neither was visible in any aggregate metric.

**Rule:** migrate fixtures, never weaken them. If a test fails after your change,
the null hypothesis is that **you** broke something. Prove otherwise by opening
the code, not by relaxing the assertion.

Corollary: a `never used` compiler warning is a finding. That is how a fix that
had silently failed to apply was discovered.

---

## 5. Diff per query, not per average

An aggregate showed a change costing 0.009 nDCG — dismissible as noise. The
per-query diff showed the truth: of 76 queries, **73 were unchanged, 2 regressed,
1 improved**, and the meaningful regression was a natural query dropping from
2nd to 3rd place because the change had dissolved the very section that answered
it.

That reframed a "statistically insignificant difference" into a concrete,
explicable quality loss — and reversed the decision.

**Rule:** when the aggregate delta is small, the per-query diff is not optional.
Small aggregates are usually *few large changes*, not *many small ones*.

---

## 6. Choose on the Pareto front and the plateau, not the peak

Two habits that repeatedly produced better defaults:

- **Pareto, not maximum.** Rank on all quality axes plus cost, take the
  non-dominated set, then pick by regret (worst shortfall on any single axis).
  The highest overall score is often bought by sacrificing one stratum.
- **Plateau, not peak.** A parameter swept to its best value sat one grid step
  from a cliff where a key metric collapsed. Shipping the mid-plateau value cost
  0.005 and bought a wide safe operating range.

**Rule:** if your optimum is at the edge of the grid you searched, you have not
found the optimum — extend the grid. Then ship the middle of the safe region,
not its boundary.

---

## 7. Prefer designs that need no migration

Chunk-level retrieval had to store many vectors per note in a store keyed by one
path per note. The obvious approach — restructure the store — would have touched
the on-disk format, its integrity checks, and a daemon protocol.

Instead: encode chunk identity **inside** the existing key (`note\0<index>`,
using a byte that cannot occur in a path). The store, cache format, versioning
and integrity checks all kept working untouched.

The cost: every place that compared a key to a note path had to learn to resolve
one to the other — and the two that were missed became the bugs in §4.

**Rule:** a trick that avoids a migration is usually worth it, but enumerate
every consumer of the key *before* you use it. Grep for the type, not the name.

---

## 8. Prefer monotone combinations

Combining two retrieval signals, `max(a, w*b)` beat `w*a + (1-w)*b` for a
structural reason: **max is monotone**, so adding the second signal can only
rescue a document, never dilute one the first signal already ranked correctly.
A weighted sum reintroduces exactly the failure being fixed — a document whose
answer lives in one place gets averaged down by everything else about it.

**Rule:** when adding a signal to fix a specific weakness, prefer a combination
that cannot harm the cases that already worked.

---

## 9. Test whether the new signal adds *information*, not just score

The question "does BM25 improve this?" was answered not by sweeping fusion
weights (28 configurations, all neutral-to-worse) but by a single diagnostic:

> How many queries does BM25 rank correctly that the semantic system misses?

**Zero of 76.** Its contribution was a strict subset. No weight, no fusion rule
and no fallback could have recovered value, and 19 queries were exposed to
downside. That closed the question in one measurement rather than by tuning.

**Rule:** before tuning a blend, check whether the second signal has any unique
successes. If it does not, stop.

---

## 10. Report honestly

- Separate **what was documented** from **what you inferred**. A magic constant
  with no rationale anywhere is a finding; guessing its purpose is a hypothesis,
  and the two must be labelled differently.
- **State the corpus.** Every number here comes from one vault, 76 queries, one
  embedding model. That is evidence, not a universal claim, and it belongs next
  to the results rather than in a footnote.
- **Keep the losses visible.** The original implementation still wins on
  typo-heavy queries in one configuration. Deleting that row would make the
  write-up cleaner and less true.
- **Correct in place.** An early claim that truncation made 73.8% of the corpus
  "invisible" was accurate about the vector index but overstated the system,
  since the lexical index saw everything. Say so plainly and move on.
