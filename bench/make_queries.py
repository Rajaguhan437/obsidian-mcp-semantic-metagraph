"""Turn an authored query set into a stratified one the evaluation can use.

You write the queries and gold answers; this adds the derived fields the strata
depend on -- most importantly `beyond_400`, which marks queries whose answer
evidence sits past word 400 of its gold note. That stratum is the decisive one:
it is exactly the content a whole-note representation truncated at 400 words
cannot see, so it separates a chunking change from noise.

  python bench/make_queries.py authored.json -o bench-out/queries.json

Input: a JSON list of records with at least

  {"query": "...", "qtype": "factual", "gold_paths": ["Notes/Alpha.md"],
   "answer_evidence": "a sentence copied from the gold note"}

`qtype` is free-form; `paraphrase` and `casual` are the two the default strata
name. `answer_evidence` is what makes `beyond_400` possible -- without it the
query still works but lands outside the deep stratum.

Output adds: gold_rel, lex, gold_words, evidence_word, long_note, beyond_400.
See bench/README.md for the full schema.
"""
import argparse
import io
import json
import os
import re

import benchlib as B

TOK = re.compile(r"[a-z0-9]{3,}")


def norm(p, prefix):
    p = str(p).replace("\\", "/")
    return p[len(prefix):] if prefix and p.startswith(prefix) else p


def evidence_word_pos(body, evidence):
    """Approximate word index where the evidence appears, or None.

    Tries progressively shorter prefixes of the evidence, then falls back to a
    distinctive long token, because notes get edited after the gold is written.
    """
    ev = re.sub(r"[*_`>#\[\]]", "", evidence or "").strip()
    if not ev:
        return None
    toks = ev.split()
    for probe_len in (12, 8, 5, 4, 3):
        if len(toks) < probe_len:
            continue
        probe = " ".join(toks[:probe_len])
        idx = body.find(probe)
        if idx == -1:
            idx = body.lower().find(probe.lower())
        if idx != -1:
            return len(body[:idx].split())
    for t in sorted(set(toks), key=len, reverse=True)[:6]:
        if len(t) < 6:
            break
        idx = body.lower().find(t.lower())
        if idx != -1:
            return len(body[:idx].split())
    return None


def build(queries, vault, prefix):
    out, missing = [], set()
    for q in queries:
        gold = [norm(g, prefix) for g in q["gold_paths"]]
        deepest, longest, gold_tokens = None, 0, set()
        for g in gold:
            fp = os.path.join(vault, g.replace("/", os.sep))
            if not os.path.exists(fp):
                missing.add(g)
                continue
            body = io.open(fp, encoding="utf-8", errors="replace").read()
            longest = max(longest, len(body.split()))
            gold_tokens |= set(TOK.findall(body.lower()))
            pos = evidence_word_pos(body, q.get("answer_evidence", ""))
            if pos is not None:
                deepest = pos if deepest is None else max(deepest, pos)

        qt = set(TOK.findall(q["query"].lower()))
        lex = len(qt & gold_tokens) / max(len(qt), 1)
        out.append({**q,
                    "gold_rel": gold,
                    "lex": round(lex, 3),
                    "gold_words": longest,
                    "evidence_word": deepest,
                    "long_note": longest > 400,
                    "beyond_400": deepest is not None and deepest > 400})
    return out, missing


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("input", help="authored query JSON")
    ap.add_argument("-o", "--output", default=None,
                    help="default: $BENCH_QUERIES, else $BENCH_OUT/queries.json")
    ap.add_argument("--vault", default=B.VAULT, help="default: $BENCH_VAULT")
    ap.add_argument("--strip-prefix", default="",
                    help="leading path segment to strip from gold_paths, "
                         "e.g. 'My Vault/' when gold was authored with it")
    args = ap.parse_args()

    if not args.vault or not os.path.isdir(args.vault):
        raise SystemExit("Set BENCH_VAULT or pass --vault (got: %r)" % args.vault)

    queries = json.load(io.open(args.input, encoding="utf-8"))
    out, missing = build(queries, args.vault, args.strip_prefix)

    dest = args.output or B.QUERIES_PATH
    os.makedirs(os.path.dirname(os.path.abspath(dest)), exist_ok=True)
    io.open(dest, "w", encoding="utf-8").write(
        json.dumps(out, ensure_ascii=False, indent=1))
    print("wrote %s (%d queries)" % (dest, len(out)))

    if missing:
        print()
        print("WARNING: %d gold path(s) not found under the vault. Those queries "
              "cannot be scored." % len(missing))
        for m in sorted(missing)[:10]:
            print("   missing:", m)
        print("If gold was authored with a leading folder, pass --strip-prefix.")

    located = sum(1 for q in out if q["evidence_word"] is not None)
    print()
    print("%-36s %s" % ("STRATUM", "count"))
    print("-" * 48)
    for label, pred in (
        ("exact-keyword (lex >= 0.75)", lambda q: q["lex"] >= 0.75),
        ("low-overlap (lex < 0.55)", lambda q: q["lex"] < 0.55),
        ("paraphrase", lambda q: q.get("qtype") == "paraphrase"),
        ("casual/typo", lambda q: q.get("qtype") == "casual"),
        ("long note (gold > 400 words)", lambda q: q["long_note"]),
        ("ANSWER BEYOND WORD 400", lambda q: q["beyond_400"]),
    ):
        print("%-36s %d" % (label, sum(1 for q in out if pred(q))))
    print("%-36s %d / %d" % ("  (evidence located)", located, len(out)))

    if not any(q["beyond_400"] for q in out):
        print()
        print("NOTE: no query lands past word 400, so the deep-content stratum "
              "is empty. Either the notes are short or answer_evidence is "
              "missing -- a chunking change will look like noise either way.")


if __name__ == "__main__":
    main()
