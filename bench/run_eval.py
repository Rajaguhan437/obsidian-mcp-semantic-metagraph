"""Run the retrieval evaluation against one binary and report by stratum.

  python bench/run_eval.py --label mine
  python bench/run_eval.py --label mine --compare bench-out/results/base.json

Queries run with lexical_prefetch=false. That is deliberate: the legacy
prefetch path re-ranks only the top DEFAULT_PREFETCH_COUNT=50 BM25 hits, so a
note BM25 misses can never be recovered. On the development corpus that cap was
worth up to 0.30 nDCG, and leaving it on makes a semantic change look far less
effective than it is. Turn it on only when you are deliberately measuring that
path.

Results are written to $BENCH_OUT/results/<label>.json so a later run can be
compared against them with --compare.
"""
import argparse
import io
import json
import os
import shutil
import time

import benchlib as B

# Stratification. `beyond_400` and `lex` are produced by make_queries.py; see
# bench/README.md for the query-set schema.
STRATA = [
    ("overall", lambda q: True),
    ("deep (past w400)", lambda q: q.get("beyond_400")),
    ("casual/typo", lambda q: q.get("qtype") == "casual"),
    ("paraphrase", lambda q: q.get("qtype") == "paraphrase"),
    ("low-overlap", lambda q: q.get("lex", 1.0) < 0.55),
    ("exact-keyword", lambda q: q.get("lex", 0.0) >= 0.75),
]


def evaluate(label, extra_env, fresh_index):
    queries = B.load_queries()
    data_dir = os.path.join(B.OUT, "data-%s" % label)
    if fresh_index and os.path.exists(data_dir):
        shutil.rmtree(data_dir, ignore_errors=True)
    os.makedirs(data_dir, exist_ok=True)

    proc, log, index_time = B.run_server(label, extra_env, data_dir=data_dir)
    mem = B.peak_memory("obsidian-mcp")
    stats = B.proxy_stats()
    print("indexed in %.1fs | %d embedding calls | %d vectors | %.0f MB"
          % (index_time, stats["calls"], stats["items"], mem / 1048576), flush=True)

    mcp = B.Mcp("http://127.0.0.1:%d/mcp" % B.HTTP_PORT)
    mcp.init()
    got, lat = [], []
    for q in queries:
        t = time.time()
        try:
            txt = mcp.call("search_semantic",
                           {"query": q["query"], "top_k": B.K,
                            "lexical_prefetch": False})
            got.append(B.parse_hits(txt))
        except Exception:
            got.append([])
        lat.append((time.time() - t) * 1000)

    B.stop_server(proc, log)
    return {"label": label, "index_time": index_time, "mem": mem, "stats": stats,
            "got": got, "median_query_ms": sorted(lat)[len(lat) // 2]}


def rows(queries, got):
    return B.score_run(list(zip(queries, [[h.replace("\\", "/") for h in g] for g in got])))


def report(queries, run, base=None):
    mine = rows(queries, run["got"])
    theirs = rows(queries, base["got"]) if base else None

    width = 78 if not base else 92
    print()
    print("=" * width)
    print("RETRIEVAL  —  %d queries, same vault / gold / model" % len(queries))
    print("=" * width)
    if base:
        print("%-22s %10s %10s %10s   %s"
              % ("stratum", base["label"], run["label"], "delta", "n"))
    else:
        print("%-22s %10s   %s" % ("stratum", "nDCG", "n"))
    print("-" * width)
    for name, sel in STRATA:
        a = B.agg(mine, sel)
        if a is None:
            continue
        if base:
            b = B.agg(theirs, sel)
            print("%-22s %10.3f %10.3f %+10.3f   %d"
                  % (name, b["nDCG"], a["nDCG"], a["nDCG"] - b["nDCG"], a["n"]))
        else:
            print("%-22s %10.3f   %d" % (name, a["nDCG"], a["n"]))

    print()
    overall_mine = B.agg(mine)
    overall_base = B.agg(theirs) if base else None
    print("%-22s %10s%s" % ("overall metric", run["label"],
                            ("%10s" % base["label"]) if base else ""))
    print("-" * width)
    for k in ("R@1", "R@5", "R@8", "MRR", "nDCG"):
        line = "%-22s %10.3f" % (k, overall_mine[k])
        if overall_base:
            line += "%10.3f" % overall_base[k]
        print(line)

    print()
    print("%-22s %10s%s" % ("cost", run["label"], ("%10s" % base["label"]) if base else ""))
    print("-" * width)
    rowsx = [("index seconds", "index_time", "%10.1f"),
             ("median query ms", "median_query_ms", "%10.1f")]
    for label, key, fmt in rowsx:
        line = ("%-22s " + fmt) % (label, run.get(key) or 0)
        if base:
            line += fmt % (base.get(key) or 0)
        print(line)
    for label, key in (("embedding calls", "calls"), ("vectors embedded", "items")):
        line = "%-22s %10d" % (label, run["stats"].get(key, 0))
        if base:
            line += "%10d" % base["stats"].get(key, 0)
        print(line)
    line = "%-22s %10.0f" % ("peak MB", (run.get("mem") or 0) / 1048576)
    if base:
        line += "%10.0f" % ((base.get("mem") or 0) / 1048576)
    print(line)
    print()
    print("One corpus, one model, one query set. These numbers describe this "
          "vault, not retrieval in general.")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--label", default="run", help="name for this run's outputs")
    ap.add_argument("--compare", help="path to a previous run's JSON to diff against")
    ap.add_argument("--env", action="append", default=[], metavar="K=V",
                    help="extra env for the server, repeatable "
                         "(e.g. --env OBSIDIAN_CHUNK_CHARS=1200)")
    ap.add_argument("--keep-index", action="store_true",
                    help="reuse an existing index instead of rebuilding it")
    args = ap.parse_args()

    B.require_config()
    extra = dict(kv.split("=", 1) for kv in args.env)

    run = evaluate(args.label, extra, fresh_index=not args.keep_index)

    results_dir = os.path.join(B.OUT, "results")
    os.makedirs(results_dir, exist_ok=True)
    dest = os.path.join(results_dir, "%s.json" % args.label)
    json.dump(run, io.open(dest, "w", encoding="utf-8"))
    print("wrote", dest)

    base = None
    if args.compare:
        base = json.load(io.open(args.compare, encoding="utf-8"))
    report(B.load_queries(), run, base)


if __name__ == "__main__":
    main()
