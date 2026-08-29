"""Shared benchmark library: config, MCP client, retrieval metrics, server runner.

Everything here is configured by environment variable so the harness can be
pointed at any vault, model and binary. Nothing about one particular corpus is
baked in.

Required:
  BENCH_VAULT      absolute path to the vault to index
  BENCH_BINARY     path to the obsidian-mcp binary under test

Optional (defaults shown):
  BENCH_OUT            ./bench-out    logs, stats and results
  BENCH_QUERIES        $BENCH_OUT/queries.json
  BENCH_HTTP_PORT      37900          port the server under test listens on
  BENCH_PROXY_PORT     11500          counting proxy (proxy.py); 0 disables it
  BENCH_UPSTREAM       http://127.0.0.1:11434   inference server behind the proxy
  BENCH_MODEL          snowflake-arctic-embed2:latest
  BENCH_DIM            1024
  BENCH_API_KEY        ollama         value is ignored by Ollama
  BENCH_TOP_K          8
  BENCH_READY_TIMEOUT  3600           seconds to wait for the semantic index
"""
import io
import json
import math
import os
import platform
import subprocess
import sys
import time
import urllib.error
import urllib.request

# ───────────────────────────── configuration ─────────────────────────────

OUT = os.path.abspath(os.environ.get("BENCH_OUT", "bench-out"))
VAULT = os.environ.get("BENCH_VAULT", "")
BINARY = os.environ.get("BENCH_BINARY", "")
HTTP_PORT = int(os.environ.get("BENCH_HTTP_PORT", "37900"))
PROXY_PORT = int(os.environ.get("BENCH_PROXY_PORT", "11500"))
UPSTREAM = os.environ.get("BENCH_UPSTREAM", "http://127.0.0.1:11434")
MODEL = os.environ.get("BENCH_MODEL", "snowflake-arctic-embed2:latest")
DIM = os.environ.get("BENCH_DIM", "1024")
API_KEY = os.environ.get("BENCH_API_KEY", "ollama")
K = int(os.environ.get("BENCH_TOP_K", "8"))
READY_TIMEOUT = int(os.environ.get("BENCH_READY_TIMEOUT", "3600"))

QUERIES_PATH = os.environ.get("BENCH_QUERIES") or os.path.join(OUT, "queries.json")
STATS_FILE = os.path.join(OUT, "stats.json")

os.makedirs(OUT, exist_ok=True)


def embed_base():
    """The endpoint the server under test should embed against."""
    if PROXY_PORT:
        return "http://127.0.0.1:%d/v1" % PROXY_PORT
    return UPSTREAM.rstrip("/") + "/v1"


def require_config():
    missing = [n for n, v in (("BENCH_VAULT", VAULT), ("BENCH_BINARY", BINARY)) if not v]
    if missing:
        sys.exit("Set %s. See bench/README.md." % " and ".join(missing))
    if not os.path.isdir(VAULT):
        sys.exit("BENCH_VAULT is not a directory: %s" % VAULT)
    if not os.path.exists(QUERIES_PATH):
        sys.exit("No query set at %s (set BENCH_QUERIES). See bench/README.md."
                 % QUERIES_PATH)


def load_queries():
    return json.load(io.open(QUERIES_PATH, encoding="utf-8"))


# ───────────────────────────── MCP client ─────────────────────────────

class Mcp:
    """Minimal MCP client over the Streamable HTTP transport."""

    def __init__(self, url):
        self.url = url
        self.sid = None

    def _post(self, payload, timeout=300):
        body = json.dumps(payload).encode()
        req = urllib.request.Request(self.url, body, method="POST")
        req.add_header("Content-Type", "application/json")
        req.add_header("Accept", "application/json, text/event-stream")
        if self.sid:
            req.add_header("mcp-session-id", self.sid)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            if not self.sid:
                self.sid = r.headers.get("mcp-session-id")
            raw = r.read().decode("utf-8", "replace")
        for line in raw.splitlines():
            line = line.strip()
            if line.startswith("data: "):
                line = line[6:]
            if line.startswith("{"):
                try:
                    return json.loads(line)
                except Exception:
                    pass
        return None

    def init(self):
        self._post({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                               "clientInfo": {"name": "bench", "version": "1"}}})
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def tools(self):
        r = self._post({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        return [t["name"] for t in (r or {}).get("result", {}).get("tools", [])]

    def call(self, name, args, timeout=300):
        r = self._post({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                        "params": {"name": name, "arguments": args}}, timeout=timeout)
        if not r or "result" not in r:
            return None
        return "\n".join(item.get("text", "") for item in r["result"].get("content", []))


def parse_hits(txt):
    """Ranked note paths from a search tool's JSON payload, slash-normalised."""
    if not txt:
        return []
    try:
        d = json.loads(txt)
    except Exception:
        return []
    items = d if isinstance(d, list) else (
        d.get("results") or d.get("hits") or d.get("matches") or [])
    out = []
    for it in items:
        if isinstance(it, dict):
            p = it.get("path") or it.get("note_path") or it.get("file")
            if p:
                out.append(str(p).replace("\\", "/"))
    return out


# ───────────────────────────── metrics ─────────────────────────────

def dcg(rels):
    return sum(r / math.log2(i + 2) for i, r in enumerate(rels))


def score_run(results):
    """results: iterable of (query_record, ranked_note_paths) -> per-query rows.

    A query record needs `gold_rel`: gold paths, vault-relative and
    slash-separated, in the same form parse_hits returns.
    """
    rows = []
    for q, got in results:
        gold = set(q["gold_rel"])
        rank = next((i for i, p in enumerate(got) if p in gold), None)
        rels = [1.0 if p in gold else 0.0 for p in got[:10]]
        ideal = ([1.0] * min(len(gold), 10) + [0.0] * 10)[:10]
        rows.append({"rank": rank,
                     "ndcg": (dcg(rels) / dcg(ideal)) if dcg(ideal) > 0 else 0.0,
                     "q": q})
    return rows


def agg(rows, sel=None):
    """Aggregate rows, optionally filtered by a predicate over the query record."""
    r = [x for x in rows if sel is None or sel(x["q"])]
    n = len(r)
    if n == 0:
        return None

    def rec(k):
        return sum(1 for x in r if x["rank"] is not None and x["rank"] < k) / n

    return {"n": n,
            "R@1": rec(1), "R@5": rec(5), "R@8": rec(8),
            "MRR": sum((1.0 / (x["rank"] + 1)) if x["rank"] is not None else 0.0
                       for x in r) / n,
            "nDCG": sum(x["ndcg"] for x in r) / n}


# ───────────────────────────── proxy helpers ─────────────────────────────

def proxy_stats():
    try:
        return json.load(io.open(STATS_FILE, encoding="utf-8"))
    except Exception:
        return {"calls": 0, "items": 0, "batch_sizes": [], "errors": 0}


def reset_proxy():
    """Reset the counting proxy at its source.

    Rewriting stats.json resets nothing: the proxy holds its counters in memory
    and the file is only a mirror. Clearing the file produced cumulative totals
    that read as a single run.
    """
    if not PROXY_PORT:
        return
    try:
        urllib.request.urlopen("http://127.0.0.1:%d/__reset" % PROXY_PORT,
                               timeout=20).read()
    except Exception as e:
        print("WARN: proxy reset failed:", e)


# ───────────────────────── process helpers ─────────────────────────

def _is_windows():
    return platform.system() == "Windows"


def peak_memory(name_fragment):
    """Peak resident memory of matching processes in bytes; 0 if unavailable."""
    try:
        import psutil  # optional dependency
        return max((p.memory_info().rss for p in psutil.process_iter(["name"])
                    if name_fragment.lower() in (p.info["name"] or "").lower()),
                   default=0)
    except ImportError:
        pass
    try:
        if _is_windows():
            cmd = ("(Get-Process | Where-Object {$_.ProcessName -like '*"
                   + name_fragment + "*'} | Measure-Object WorkingSet64 -Maximum).Maximum")
            out = subprocess.run(["powershell", "-NoProfile", "-Command", cmd],
                                 capture_output=True, text=True, timeout=60).stdout.strip()
            return int(out) if out.isdigit() else 0
        out = subprocess.run(["ps", "-eo", "rss,comm"], capture_output=True,
                             text=True, timeout=60).stdout
        vals = [int(l.split()[0]) * 1024 for l in out.splitlines()[1:]
                if name_fragment in l and l.split()]
        return max(vals, default=0)
    except Exception:
        return 0


def kill_stale(name="obsidian-mcp"):
    try:
        if _is_windows():
            cmd = ("Get-Process " + name
                   + " -ErrorAction SilentlyContinue | Stop-Process -Force")
            subprocess.run(["powershell", "-NoProfile", "-Command", cmd],
                           capture_output=True, timeout=60)
        else:
            subprocess.run(["pkill", "-f", name], capture_output=True, timeout=60)
    except Exception:
        pass
    time.sleep(2)


def server_env(extra=None, data_dir=None, port=None):
    """Environment for the server under test."""
    env = dict(os.environ)
    env.update({
        "OBSIDIAN_VAULT_PATH": VAULT,
        "OBSIDIAN_EMBEDDINGS": "true",
        "OBSIDIAN_EMBEDDING_PROVIDER": "api",
        "OBSIDIAN_EMBEDDING_API_BASE": embed_base(),
        "OBSIDIAN_EMBEDDING_API_MODEL": MODEL,
        "OBSIDIAN_EMBEDDING_API_KEY": API_KEY,
        "OBSIDIAN_EMBEDDING_DIM": str(DIM),
        "OBSIDIAN_MCP_DATA": data_dir or os.path.join(OUT, "data"),
        "OBSIDIAN_WATCH": "false",
        "OBSIDIAN_HTTP_PORT": str(port or HTTP_PORT),
        "OBSIDIAN_LOG_LEVEL": "info",
    })
    env.update(extra or {})
    return env


def wait_until_ready(port, deadline_s=None):
    """Block until the semantic index answers; return seconds elapsed, or None.

    Readiness is probed POSITIVELY. Do not infer it from the proxy going quiet:
    while the embedding runtime warms up, the server still answers
    search_semantic through a lexical-only fallback, which silently turns the
    whole measurement into a pure-BM25 run. Two complete runs were lost to this
    before the probe existed; the tell was results matching a BM25 baseline to
    three decimal places.
    """
    t0 = time.time()
    deadline = t0 + (deadline_s or READY_TIMEOUT)
    probe = Mcp("http://127.0.0.1:%d/mcp" % port)
    while time.time() < deadline:
        time.sleep(5)
        try:
            if probe.sid is None:
                probe.init()
            raw = probe._post({"jsonrpc": "2.0", "id": 99, "method": "tools/call",
                               "params": {"name": "search_semantic",
                                          "arguments": {"query": "readiness probe",
                                                        "top_k": 1,
                                                        "lexical_prefetch": False}}},
                              timeout=120)
        except Exception:
            probe.sid = None
            continue
        if raw and "error" not in raw:
            return time.time() - t0
    return None


def run_server(label, extra_env=None, port=None, data_dir=None, binary=None):
    """Start the server and wait for a live semantic index.

    Returns (proc, log_handle, seconds_to_ready).
    """
    port = port or HTTP_PORT
    kill_stale()
    log = io.open(os.path.join(OUT, "%s.log" % label), "w", encoding="utf-8")
    reset_proxy()
    proc = subprocess.Popen([binary or BINARY, "--http"],
                            env=server_env(extra_env, data_dir, port),
                            stdout=log, stderr=subprocess.STDOUT)
    elapsed = wait_until_ready(port)
    if elapsed is None:
        stop_server(proc, log)
        raise RuntimeError("%s: semantic index never became ready" % label)
    time.sleep(5)  # let the final cache persist settle
    return proc, log, elapsed


def stop_server(proc, log=None):
    try:
        proc.terminate()
        proc.wait(timeout=20)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass
    if log:
        log.close()
