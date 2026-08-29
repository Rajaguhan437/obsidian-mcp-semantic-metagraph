"""Counting reverse proxy in front of the embedding server.

Forwards everything upstream and records, per embedding request, how many
inputs it carried. This is what lets the benchmark report exact embedding call
and batch counts for each implementation instead of inferring them from logs.

Instrumenting the boundary is the point: batch sizes measured here are what
exposed that a 32-note reconcile batch was being sent as a single request of
several hundred chunks.

  python bench/proxy.py [listen_port] [stats_file]

Defaults come from BENCH_PROXY_PORT, BENCH_OUT and BENCH_UPSTREAM
(see benchlib.py). Reset counters between runs by GET/POST to /__reset --
never by rewriting the stats file, which the proxy only mirrors.
"""
import json
import os
import sys
import threading
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = os.environ.get("BENCH_UPSTREAM", "http://127.0.0.1:11434").rstrip("/")
LOCK = threading.Lock()
EMPTY = {"calls": 0, "items": 0, "batch_sizes": [], "errors": 0, "other_calls": 0}
STATS = dict(EMPTY, batch_sizes=[])
STATS_FILE = None


def _count(body):
    """Number of inputs in an embeddings request body."""
    try:
        d = json.loads(body)
    except Exception:
        return None
    for key in ("input", "prompt", "texts"):
        if key in d:
            v = d[key]
            return len(v) if isinstance(v, list) else 1
    return None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _reply(self, code, data, headers=()):
        self.send_response(code)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _proxy(self, method):
        # In-memory counters are the source of truth; the file is only a mirror.
        # A run resets through this endpoint, never by rewriting the file --
        # doing that produced cumulative totals that read as a single run.
        if self.path.startswith("/__reset"):
            with LOCK:
                STATS.update(dict(EMPTY, batch_sizes=[]))
                if STATS_FILE:
                    json.dump(STATS, open(STATS_FILE, "w"))
            self._reply(200, b"ok")
            return

        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None

        is_embed = "embed" in self.path
        if is_embed:
            n = _count(body or b"")
            with LOCK:
                STATS["calls"] += 1
                if n:
                    STATS["items"] += n
                    STATS["batch_sizes"].append(n)
        else:
            with LOCK:
                STATS["other_calls"] += 1

        req = urllib.request.Request(UPSTREAM + self.path, data=body, method=method)
        for k, v in self.headers.items():
            if k.lower() not in ("host", "content-length", "connection"):
                req.add_header(k, v)
        try:
            with urllib.request.urlopen(req, timeout=900) as r:
                data = r.read()
                keep = [(k, v) for k, v in r.headers.items()
                        if k.lower() not in ("transfer-encoding", "connection",
                                             "content-length")]
                self._reply(r.status, data, keep)
        except urllib.error.HTTPError as e:
            data = e.read()
            with LOCK:
                if is_embed:
                    STATS["errors"] += 1
            self._reply(e.code, data)
        except Exception as e:
            with LOCK:
                if is_embed:
                    STATS["errors"] += 1
            self._reply(502, json.dumps({"error": str(e)}).encode())

        if STATS_FILE:
            with LOCK:
                json.dump(STATS, open(STATS_FILE, "w"))

    def do_POST(self):
        self._proxy("POST")

    def do_GET(self):
        self._proxy("GET")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else int(
        os.environ.get("BENCH_PROXY_PORT", "11500"))
    default_stats = os.path.join(
        os.path.abspath(os.environ.get("BENCH_OUT", "bench-out")), "stats.json")
    STATS_FILE = sys.argv[2] if len(sys.argv) > 2 else default_stats
    os.makedirs(os.path.dirname(STATS_FILE), exist_ok=True)
    json.dump(STATS, open(STATS_FILE, "w"))
    print("proxy :%d -> %s   stats: %s" % (port, UPSTREAM, STATS_FILE), flush=True)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
