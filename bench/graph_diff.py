"""Compare two graph baselines captured by graph_baseline.py.

  python bench/graph_diff.py before.json after.json

Exits non-zero if anything changed, so it can gate a refactor in CI. A
retrieval change should produce no diff at all; when it does produce one, the
per-note lines below say exactly which notes gained or lost edges.

Comparison is order-insensitive on purpose -- `wikilinks` iterates a hash map,
so result order varies between runs and means nothing.
"""
import argparse
import io
import json
import sys


def outs(d, n):
    v = (d["outgoing"] or {}).get(n)
    return v if isinstance(v, list) else []


def bls(d, n):
    v = (d["backlinks"] or {}).get(n)
    return v if isinstance(v, list) else []


def backlink_count(items):
    return sum(len(s.get("links", []) or []) for s in items if isinstance(s, dict))


def broken_pairs(d):
    out = set()
    for it in (d.get("broken") or []):
        if isinstance(it, dict):
            out.add((str(it.get("source_path")), str(it.get("target"))))
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("before")
    ap.add_argument("after")
    ap.add_argument("--limit", type=int, default=40,
                    help="max per-note lines to print (default 40)")
    args = ap.parse_args()

    A = json.load(io.open(args.before, encoding="utf-8"))
    B = json.load(io.open(args.after, encoding="utf-8"))

    print("=" * 92)
    print("GRAPH DIFF   %s  ->  %s" % (args.before, args.after))
    print("=" * 92)
    differs = False
    for k in sorted(A["totals"]):
        a, b = A["totals"][k], B["totals"].get(k, 0)
        flag = "" if a == b else ("  %+d" % (b - a))
        if a != b:
            differs = True
        print("  %-28s %8d -> %8d%s" % (k, a, b, flag))

    notes = sorted(set(A["outgoing"]) | set(B["outgoing"]))
    gained_out, changed_bl = [], []
    for n in notes:
        oa, ob = len(outs(A, n)), len(outs(B, n))
        if oa != ob:
            gained_out.append((n, oa, ob))
        ca, cb = backlink_count(bls(A, n)), backlink_count(bls(B, n))
        if ca != cb:
            changed_bl.append((n, ca, cb))

    print("\nNOTES WHOSE OUTGOING LINK COUNT CHANGED: %d" % len(gained_out))
    for n, a, b in gained_out[:args.limit]:
        print("   %+3d  %-72s %d -> %d" % (b - a, n[:72], a, b))

    print("\nNOTES WHOSE BACKLINK COUNT CHANGED: %d" % len(changed_bl))
    for n, a, b in changed_bl[:args.limit]:
        print("   %+3d  %-72s %d -> %d" % (b - a, n[:72], a, b))

    ba, bb = broken_pairs(A), broken_pairs(B)
    fixed, new = ba - bb, bb - ba
    print("\nBROKEN LINKS REPAIRED (were broken, now resolve): %d" % len(fixed))
    for s, t in sorted(fixed)[:args.limit]:
        print("   %-60s -> %s" % (s[:60], t[:40]))
    print("\nBROKEN LINKS NEWLY VISIBLE: %d" % len(new))
    by_src = {}
    for s, t in new:
        by_src.setdefault(s, []).append(t)
    for s in sorted(by_src)[:args.limit]:
        print("   %-64s %d links" % (s[:64], len(by_src[s])))

    print("\nORPHANS  %d -> %d" % (A["totals"]["orphans"], B["totals"]["orphans"]))

    differs = differs or gained_out or changed_bl or fixed or new
    print("\n%s" % ("GRAPH CHANGED" if differs else "NO GRAPH DIFFERENCES"))
    sys.exit(1 if differs else 0)


if __name__ == "__main__":
    main()
