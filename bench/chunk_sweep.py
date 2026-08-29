"""Offline sweep of chunk size, overlap and the summary weight.

This never starts the MCP server. It reads the vault, chunks it in Python,
embeds each configuration once against the inference server directly, caches
the vectors, and then scores every (config, w_sum) pair as pure matrix
arithmetic. Sweeping offline first is what made it affordable to explore the
space before writing any Rust -- and the Rust implementation later reproduced
these predictions closely.

Representation under test:

    sem(note) = max( max_i cos(q, chunk_i),  w_sum * cos(q, summary) )

where summary is the whole-note text a truncating implementation would use:
title + all headings + first MAX_BODY_WORDS words. w_sum = 0 disables the
summary arm, giving pure chunks.

`max` rather than a weighted sum is deliberate: max is monotone, so adding the
summary can only raise a note's score. A weighted sum averages a note's best
chunk against a summary that may not mention the answer at all, reintroducing
the dilution chunking exists to fix.

  python bench/chunk_sweep.py
  python bench/chunk_sweep.py --configs 800x160,1000x200 --wsums 0,1.0,1.2,1.3

Vectors are cached under $BENCH_OUT/sweep, so re-running to add a w_sum costs
nothing -- w_sum is applied after embedding.
"""
import argparse
import io
import json
import math
import os
import re
import time
import urllib.request

try:
    import numpy as np
except ImportError:  # the rest of the harness is standard-library only
    raise SystemExit("chunk_sweep.py needs numpy: pip install -r bench/requirements.txt")

import benchlib as B

MAX_BODY_WORDS = int(os.environ.get("BENCH_SUMMARY_WORDS", "400"))
DOC_PREFIX = os.environ.get("BENCH_DOC_PREFIX", "passage: ")
QUERY_PREFIX = os.environ.get("BENCH_QUERY_PREFIX", "query: ")
EMBED_BATCH = int(os.environ.get("BENCH_EMBED_BATCH", "16"))

STRATA = [
    ("overall", lambda q: True),
    ("deep (past w400)", lambda q: q.get("beyond_400")),
    ("casual/typo", lambda q: q.get("qtype") == "casual"),
    ("paraphrase", lambda q: q.get("qtype") == "paraphrase"),
    ("low-overlap", lambda q: q.get("lex", 1.0) < 0.55),
    ("exact-keyword", lambda q: q.get("lex", 0.0) >= 0.75),
]

STATS = {"calls": 0, "items": 0}
HEAD = re.compile(r"^(#{1,6})\s+(.+)$", re.M)


def embed(texts, tag, url, model):
    """Embed in bounded batches with backoff; returns L2-normalised rows.

    Batching is not incidental. Sending a whole corpus in one request is what
    overruns a local inference server -- it answers with a connection error
    from its own tokenizer rather than anything that looks like a size limit.
    """
    t0, out = time.time(), []
    for i in range(0, len(texts), EMBED_BATCH):
        grp = texts[i:i + EMBED_BATCH]
        for attempt in range(4):
            try:
                body = json.dumps({"model": model, "input": grp}).encode()
                req = urllib.request.Request(url, body,
                                             {"Content-Type": "application/json"})
                payload = json.loads(urllib.request.urlopen(req, timeout=900).read())
                out.extend(payload["embeddings"])
                STATS["calls"] += 1
                STATS["items"] += len(grp)
                break
            except Exception:
                if attempt == 3:
                    raise
                time.sleep(2 * (attempt + 1))
        if (i // EMBED_BATCH) % 80 == 0:
            print("   %s %d/%d" % (tag, i, len(texts)), flush=True)
    M = np.asarray(out, dtype=np.float32)
    M /= (np.linalg.norm(M, axis=1, keepdims=True) + 1e-12)
    return M, time.time() - t0


def strip_fm(t):
    if t.startswith("---"):
        e = t.find("\n---", 3)
        if e != -1:
            return t[e + 4:]
    return t


def sections(body):
    """Heading-scoped spans with breadcrumbs, ignoring '#' inside code fences."""
    out, stack, in_fence, start, crumb, off = [], [], False, 0, "", 0
    for line in body.splitlines(keepends=True):
        t = line.lstrip()
        if t.startswith("```") or t.startswith("~~~"):
            in_fence = not in_fence
            off += len(line)
            continue
        if not in_fence and t.startswith("#"):
            h = len(t) - len(t.lstrip("#"))
            rest = t[h:]
            if 1 <= h <= 6 and rest[:1].isspace():
                if off > start:
                    out.append((crumb, start, off))
                while stack and stack[-1][0] >= h:
                    stack.pop()
                stack.append((h, rest.strip()))
                crumb = " > ".join(x[1] for x in stack)
                start = off
        off += len(line)
    if len(body) > start:
        out.append((crumb, start, len(body)))
    return out


def split_point(s, limit):
    """Best split at or before `limit`: paragraph, then sentence, then space."""
    if len(s) <= limit:
        return len(s)
    w, floor = s[:limit], limit // 2
    p = w.rfind("\n\n")
    if p > floor:
        return p + 2
    for pat in (". ", ".\n", "! ", "? "):
        p = w.rfind(pat)
        if p > floor:
            return p + len(pat)
    for pat in ("\n", " "):
        p = w.rfind(pat)
        if p > floor:
            return p + 1
    return limit


def chunk(body, target, overlap):
    res = []
    for crumb, a, b in sections(body):
        raw = body[a:b]
        if not raw.strip():
            continue
        cur = 0
        while cur < len(raw):
            rem = raw[cur:]
            take = min(split_point(rem, target) or len(rem), target * 2)
            piece = rem[:take]
            if piece.strip():
                res.append((crumb, piece.strip()))
            if cur + take >= len(raw):
                break
            cur = max(cur + 1, cur + take - overlap)  # always progress
    return res


def read_corpus(vault):
    notes, bodies, titles, heads = [], [], [], []
    for dp, _dn, fn in os.walk(vault):
        for f in sorted(fn):
            if not f.endswith(".md"):
                continue
            p = os.path.join(dp, f)
            body = strip_fm(io.open(p, encoding="utf-8", errors="replace").read())
            if not body.strip():
                continue
            notes.append(os.path.relpath(p, vault).replace("\\", "/"))
            bodies.append(body)
            titles.append(os.path.splitext(f)[0])
            heads.append([m.group(2).strip() for m in HEAD.finditer(body)])
    return notes, bodies, titles, heads


def dcg(r):
    return sum(x / math.log2(i + 2) for i, x in enumerate(r))


def metrics(queries, order_fn, sel):
    rows = []
    for i, q in enumerate(queries):
        if not sel(q):
            continue
        order = order_fn(i)
        gold = set(q["gold_rel"])
        rank = next((r for r, n in enumerate(order[:10]) if n in gold), None)
        rels = [1.0 if n in gold else 0.0 for n in order[:10]]
        ideal = ([1.0] * min(len(gold), 10) + [0.0] * 10)[:10]
        rows.append((rank, dcg(rels) / dcg(ideal) if dcg(ideal) > 0 else 0.0))
    n = len(rows)
    if n == 0:
        return None

    def rec(k):
        return sum(1 for r, _ in rows if r is not None and r < k) / n

    return {"n": n, "r1": rec(1), "r5": rec(5),
            "mrr": sum((1.0 / (r + 1)) if r is not None else 0.0 for r, _ in rows) / n,
            "ndcg": sum(d for _, d in rows) / n}


def pareto_front(results):
    """Configurations not dominated on both overall nDCG and chunk count."""
    front = []
    for k, v in results.items():
        dominated = any(
            o["overall"]["ndcg"] >= v["overall"]["ndcg"] and o["chunks"] <= v["chunks"]
            and (o["overall"]["ndcg"] > v["overall"]["ndcg"] or o["chunks"] < v["chunks"])
            for ok, o in results.items() if ok != k)
        if not dominated:
            front.append(k)
    return sorted(front, key=lambda k: -results[k]["overall"]["ndcg"])


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--configs", default="600x120,1000x200,1500x150,2000x300",
                    help="comma-separated TARGETxOVERLAP in characters")
    ap.add_argument("--wsums", default="0,0.95,1.0,1.05,1.1,1.15,1.2,1.3,1.5")
    ap.add_argument("--vault", default=B.VAULT)
    args = ap.parse_args()

    if not args.vault or not os.path.isdir(args.vault):
        raise SystemExit("Set BENCH_VAULT or pass --vault.")
    configs = [tuple(int(x) for x in c.lower().split("x"))
               for c in args.configs.split(",") if c.strip()]
    wsums = [float(w) for w in args.wsums.split(",") if w.strip()]

    url = B.UPSTREAM.rstrip("/") + "/api/embed"
    cache = os.path.join(B.OUT, "sweep")
    os.makedirs(cache, exist_ok=True)
    queries = B.load_queries()

    notes, bodies, titles, heads = read_corpus(args.vault)
    print("notes: %d | model: %s" % (len(notes), B.MODEL), flush=True)

    spath = os.path.join(cache, "summary.npy")
    if os.path.exists(spath):
        S = np.load(spath)
    else:
        texts = ["%s%s\n%s\n%s" % (DOC_PREFIX, titles[i], " | ".join(heads[i]),
                                   " ".join(bodies[i].split()[:MAX_BODY_WORDS]))
                 for i in range(len(notes))]
        S, _ = embed(texts, "summary", url, B.MODEL)
        np.save(spath, S)
    print("summary vectors: %d" % len(S), flush=True)

    qpath = os.path.join(cache, "queries.npy")
    if os.path.exists(qpath) and len(np.load(qpath)) == len(queries):
        Q = np.load(qpath)
    else:
        Q, _ = embed([QUERY_PREFIX + q["query"] for q in queries], "queries", url, B.MODEL)
        np.save(qpath, Q)

    results = {}
    for target, overlap in configs:
        key = "%d_%d" % (target, overlap)
        cpath, mpath = (os.path.join(cache, "chunks_%s.npy" % key),
                        os.path.join(cache, "meta_%s.json" % key))
        if os.path.exists(cpath) and os.path.exists(mpath):
            C = np.load(cpath)
            meta = json.load(io.open(mpath, encoding="utf-8"))
            owner, c_time, calls = np.array(meta["owner"]), meta["time"], meta["calls"]
        else:
            rows, own = [], []
            for i in range(len(notes)):
                for crumb, text in chunk(bodies[i], target, overlap):
                    head = "%s\n%s" % (titles[i], crumb) if crumb else titles[i]
                    rows.append("%s%s\n%s" % (DOC_PREFIX, head, text))
                    own.append(i)
            before = STATS["calls"]
            C, c_time = embed(rows, key, url, B.MODEL)
            calls = STATS["calls"] - before
            owner = np.array(own)
            np.save(cpath, C)
            json.dump({"owner": own, "time": c_time, "calls": calls},
                      io.open(mpath, "w", encoding="utf-8"))
        print("config %s -> %d chunks, %.0fs, %d calls"
              % (key, len(C), c_time, calls), flush=True)

        Sc = Q @ S.T
        Cc = Q @ C.T
        best = np.full((len(queries), len(notes)), -2.0, dtype=np.float32)
        for qi in range(len(queries)):
            np.maximum.at(best[qi], owner, Cc[qi])

        for w in wsums:
            comb = np.maximum(best, w * Sc) if w > 0 else best

            def order_fn(i, comb=comb):
                return [notes[j] for j in np.argsort(-comb[i])[:12]]

            row = {"chunks": int(len(C)), "calls": int(calls), "time": float(c_time)}
            for name, sel in STRATA:
                m = metrics(queries, order_fn, sel)
                if m:
                    row[name] = m
            results["%s|w%.2f" % (key, w)] = row

    dest = os.path.join(B.OUT, "sweep_results.json")
    json.dump(results, io.open(dest, "w", encoding="utf-8"))
    print("\nsaved %s  (embedding calls this run: %d / items %d)"
          % (dest, STATS["calls"], STATS["items"]))

    ranked = sorted(results.items(), key=lambda kv: -kv[1]["overall"]["ndcg"])
    print("\n%-18s %8s %8s %8s %9s" % ("config|w_sum", "overall", "deep", "casual", "chunks"))
    print("-" * 56)
    for k, v in ranked[:12]:
        print("%-18s %8.3f %8.3f %8.3f %9d"
              % (k, v["overall"]["ndcg"],
                 v.get("deep (past w400)", {}).get("ndcg", float("nan")),
                 v.get("casual/typo", {}).get("ndcg", float("nan")), v["chunks"]))

    print("\nPARETO FRONT (nDCG vs index size)")
    print("-" * 56)
    for k in pareto_front(results):
        v = results[k]
        print("%-18s %8.3f %9d chunks" % (k, v["overall"]["ndcg"], v["chunks"]))
    print("\nPrefer a plateau over a peak: if a metric moves sharply between "
          "neighbouring w_sum values you are reading noise, not a setting.")


if __name__ == "__main__":
    main()
