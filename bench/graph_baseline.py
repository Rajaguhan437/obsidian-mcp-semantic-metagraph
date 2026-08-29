"""Capture a complete graph baseline (backlinks, outgoing, broken, orphans).

Embeddings are disabled: the graph layer does not use them, so the capture is
fast and deterministic.

  python bench/graph_baseline.py --binary ./obsidian-mcp -o bench-out/graph_before.json

Capture one of these BEFORE a retrieval change and one after, then compare with
graph_diff.py. Retrieval refactors are supposed to leave the link graph
untouched; this is what turns "should be unaffected" into a checked claim. It
also catches the reverse -- a note that was silently dropped from the index
disappears from the graph too, taking its inbound and outbound edges with it.

`wikilinks` output is not deterministically ordered (hash-map iteration), so
graph_diff.py compares counts and sets rather than sequences.
"""
import argparse
import io
import json
import os
import subprocess
import time

import benchlib as B


def call(mcp, name, args):
    try:
        txt = mcp.call(name, args, timeout=180)
        return json.loads(txt) if txt else None
    except Exception as e:
        return {"__error__": str(e)}


def notes_on_disk(vault):
    out = []
    for dp, _dn, fn in os.walk(vault):
        for f in sorted(fn):
            if f.endswith(".md"):
                out.append(os.path.relpath(os.path.join(dp, f), vault).replace("\\", "/"))
    return out


def totals_from(data):
    resolved = unresolved = notes_with_outgoing = 0
    for _n, o in data["outgoing"].items():
        items = o if isinstance(o, list) else (
            (o or {}).get("links", []) if isinstance(o, dict) else [])
        if not isinstance(items, list):
            items = []
        if items:
            notes_with_outgoing += 1
        for l in items:
            if isinstance(l, dict):
                if l.get("resolved_path"):
                    resolved += 1
                else:
                    unresolved += 1

    backlink_edges = 0
    for _n, b in data["backlinks"].items():
        for s in (b if isinstance(b, list) else []):
            if isinstance(s, dict):
                backlink_edges += len(s.get("links", []) or [])

    broken = data["broken"] if isinstance(data["broken"], list) else []
    orphans = data["orphans"] if isinstance(data["orphans"], list) else []
    return {"resolved_outgoing_edges": resolved,
            "unresolved_outgoing_edges": unresolved,
            "backlink_edges": backlink_edges,
            "notes_with_outgoing": notes_with_outgoing,
            "broken_links": len(broken),
            "orphans": len(orphans)}


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("-o", "--output", required=True)
    ap.add_argument("--binary", default=B.BINARY, help="default: $BENCH_BINARY")
    ap.add_argument("--vault", default=B.VAULT, help="default: $BENCH_VAULT")
    ap.add_argument("--port", type=int, default=B.HTTP_PORT + 1)
    args = ap.parse_args()

    if not args.binary or not args.vault:
        raise SystemExit("Set BENCH_BINARY and BENCH_VAULT, or pass --binary/--vault.")

    B.kill_stale()
    env = dict(os.environ)
    env.update({"OBSIDIAN_VAULT_PATH": args.vault,
                "OBSIDIAN_EMBEDDINGS": "false",
                "OBSIDIAN_WATCH": "false",
                "OBSIDIAN_HTTP_PORT": str(args.port),
                "OBSIDIAN_LOG_LEVEL": "warn",
                "OBSIDIAN_MCP_DATA": os.path.join(B.OUT, "graphdata")})
    log = io.open(os.path.join(B.OUT, "graph_%d.log" % args.port), "w", encoding="utf-8")
    proc = subprocess.Popen([args.binary, "--http"], env=env,
                            stdout=log, stderr=subprocess.STDOUT)

    mcp = B.Mcp("http://127.0.0.1:%d/mcp" % args.port)
    for _ in range(60):
        time.sleep(2)
        try:
            mcp.init()
            if mcp.tools():
                break
        except Exception:
            mcp.sid = None
    else:
        B.stop_server(proc, log)
        raise SystemExit("server never answered tools/list")
    print("connected", flush=True)

    notes = notes_on_disk(args.vault)
    print("notes on disk: %d" % len(notes), flush=True)

    data = {"notes_on_disk": len(notes), "backlinks": {}, "outgoing": {}}
    for i, n in enumerate(notes):
        data["backlinks"][n] = call(mcp, "wikilinks", {"query": "backlinks", "path": n})
        data["outgoing"][n] = call(mcp, "wikilinks", {"query": "outgoing", "path": n})
        if i % 100 == 0:
            print("  %d/%d" % (i, len(notes)), flush=True)

    data["broken"] = call(mcp, "wikilinks", {"query": "broken"})
    data["orphans"] = call(mcp, "wikilinks", {"query": "orphans"})
    data["vault_info"] = call(mcp, "vault_info", {})
    data["totals"] = totals_from(data)
    print(json.dumps(data["totals"], indent=1), flush=True)

    json.dump(data, io.open(args.output, "w", encoding="utf-8"),
              ensure_ascii=False, indent=1, sort_keys=True)
    print("saved", args.output)
    B.stop_server(proc, log)


if __name__ == "__main__":
    main()
