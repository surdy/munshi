#!/usr/bin/env python3
"""Local read-only dashboard for munshi's session-archiving backlog.

Serves GET /            -> index.html (read from disk each request)
       GET /api/data    -> fresh JSON snapshot (cached ~30s)

All data sources are read-only:
  - `munshi status --json`, `munshi archive-upload status --json`,
    `munshi summary-delivery status --json`
  - the backlog-driver log (time series)
  - a COPY of ~/.munshi/munshi.db (never the live DB)
  - `# ` H1 titles from ~/munshi-summaries markdown
"""
import json
import os
import re
import shutil
import sqlite3
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
MUNSHI = "/Users/surdy/.local/bin/munshi"
STATE_DIR = "/Users/surdy/.munshi"
LIVE_DB = os.path.join(STATE_DIR, "munshi.db")
DB_COPY = os.path.join(HERE, "munshi-copy.db")
SUMMARIES_DIR = "/Users/surdy/munshi-summaries"
DRIVER_LOG = ("/private/tmp/claude-503/-Users-surdy-repos-patwari/"
              "88733b7c-e66d-4c45-946b-8b1c2ecf87c6/scratchpad/backlog-driver.log")
BIND = ("127.0.0.1", 8877)
CACHE_TTL_S = 30

_cache = {"at": 0.0, "data": None}
_lock = threading.Lock()


def _run(cmd, timeout=25):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)


def _munshi_json(args, errors, label):
    try:
        p = _run([MUNSHI] + args)
        if p.returncode != 0:
            errors.append({"source": label, "message": (p.stderr or p.stdout).strip()[:400]})
            return None
        return json.loads(p.stdout)
    except Exception as e:  # noqa: BLE001
        errors.append({"source": label, "message": str(e)[:400]})
        return None


# ---------------------------------------------------------------- driver log

MARKER_RE = re.compile(r"^\[(done|stalled|timeout)\b")
ROUND_RE = re.compile(r"^\[round (\d+) (\d{2}):(\d{2}):(\d{2})\]\s*(.*)$")
KV_RE = re.compile(r"([a-z-]+)=(\d+)")


def parse_driver_log(errors):
    out = {"alive": False, "pids": [], "log_mtime_ms": None,
           "epochs": [], "current_epoch_index": None}
    try:
        p = _run(["pgrep", "-f", "backlog-driver.sh"], timeout=10)
        pids = [int(x) for x in p.stdout.split()] if p.returncode == 0 else []
        out["alive"] = bool(pids)
        out["pids"] = pids
    except Exception as e:  # noqa: BLE001
        errors.append({"source": "pgrep", "message": str(e)[:200]})

    if not os.path.exists(DRIVER_LOG):
        errors.append({"source": "driver-log", "message": "log file not found"})
        return out
    out["log_mtime_ms"] = int(os.path.getmtime(DRIVER_LOG) * 1000)

    epochs = [{"end_marker": None, "rows": []}]
    try:
        with open(DRIVER_LOG, "r", errors="replace") as f:
            for line in f:
                line = line.strip()
                if MARKER_RE.match(line):
                    epochs[-1]["end_marker"] = line[:200]
                    epochs.append({"end_marker": None, "rows": []})
                    continue
                m = ROUND_RE.match(line)
                if not m:
                    continue
                rnd = int(m.group(1))
                hh, mm, ss = int(m.group(2)), int(m.group(3)), int(m.group(4))
                sess_part, _, up_part = m.group(5).partition("|")
                row = {"round": rnd, "time": f"{hh:02d}:{mm:02d}:{ss:02d}",
                       "tod_s": hh * 3600 + mm * 60 + ss}
                for k, v in KV_RE.findall(sess_part):
                    row[k.replace("-", "_")] = int(v)
                for k, v in KV_RE.findall(up_part):
                    row["up_" + k.replace("-", "_")] = int(v)
                row["queued"] = row.get("interrupted", 0) + row.get("observed", 0)
                epochs[-1]["rows"].append(row)
    except Exception as e:  # noqa: BLE001
        errors.append({"source": "driver-log", "message": str(e)[:400]})

    # relative elapsed seconds within each epoch (handles midnight wrap)
    for ep in epochs:
        prev, base, day = None, None, 0
        for row in ep["rows"]:
            t = row["tod_s"] + day
            if prev is not None and t < prev:
                day += 86400
                t = row["tod_s"] + day
            if base is None:
                base = t
            row["t_s"] = t - base
            prev = t
    out["epochs"] = epochs
    # current epoch = last one (rows after the final marker), even if empty
    out["current_epoch_index"] = len(epochs) - 1
    return out


def compute_rate(driver, status):
    """sessions/hr from the tail of the current epoch, plus a naive ETA."""
    res = {"sessions_per_hour": None, "eta_hours": None, "window_rounds": 0}
    try:
        rows = driver["epochs"][driver["current_epoch_index"]]["rows"]
    except Exception:  # noqa: BLE001
        return res
    if len(rows) < 2:
        return res
    window = rows[-7:]  # up to 6 intervals (~1h at 10-min cadence)
    dt_h = (window[-1]["t_s"] - window[0]["t_s"]) / 3600.0
    da = window[-1].get("archived", 0) - window[0].get("archived", 0)
    res["window_rounds"] = len(window)
    if dt_h > 0:
        rate = da / dt_h
        res["sessions_per_hour"] = round(rate, 2)
        queued = None
        if status and "sessions" in status:
            s = status["sessions"]
            queued = s.get("interrupted", 0) + s.get("observed", 0)
        elif rows:
            queued = rows[-1].get("queued")
        if queued is not None and rate > 0.05:
            res["eta_hours"] = round(queued / rate, 2)
    return res


# ------------------------------------------------------------------- sqlite

def copy_db(errors):
    try:
        if not os.path.exists(LIVE_DB):
            errors.append({"source": "sqlite", "message": "live db not found"})
            return False
        shutil.copyfile(LIVE_DB, DB_COPY)
        for suffix in ("-wal", "-shm"):
            src = LIVE_DB + suffix
            dst = DB_COPY + suffix
            if os.path.exists(src):
                shutil.copyfile(src, dst)
            elif os.path.exists(dst):
                os.remove(dst)
        return True
    except Exception as e:  # noqa: BLE001
        errors.append({"source": "sqlite-copy", "message": str(e)[:400]})
        return False


def project_of(name, cwd, component):
    if name:
        return name
    if component:
        return re.sub(r"-[0-9a-f]{8,}$", "", component)
    if cwd:
        base = os.path.basename(cwd.rstrip("/"))
        return base or cwd
    return "(unknown)"


def title_from_markdown(rel_path):
    if not rel_path:
        return None
    path = os.path.join(SUMMARIES_DIR, rel_path)
    try:
        with open(path, "r", errors="replace") as f:
            for i, line in enumerate(f):
                if i > 60:
                    break
                if line.startswith("# "):
                    return line[2:].strip()[:160]
    except OSError:
        return None
    return None


def query_db(errors):
    out = {}
    if not copy_db(errors):
        return None
    try:
        con = sqlite3.connect(f"file:{DB_COPY}?mode=ro", uri=True, timeout=5)
        con.row_factory = sqlite3.Row
        cur = con.cursor()

        out["by_state_source"] = [dict(r) for r in cur.execute(
            "SELECT lifecycle_state AS state, source_kind AS source, count(*) AS n "
            "FROM sessions GROUP BY 1, 2")]

        rows = cur.execute(
            "SELECT origin_project_name AS name, origin_cwd AS cwd, "
            "origin_project_component AS comp, count(*) AS n FROM sessions "
            "WHERE lifecycle_state IN ('interrupted','observed') GROUP BY 1, 2, 3").fetchall()
        agg = {}
        for r in rows:
            key = project_of(r["name"], r["cwd"], r["comp"])
            agg[key] = agg.get(key, 0) + r["n"]
        out["remaining_by_project"] = sorted(agg.items(), key=lambda kv: -kv[1])

        out["archived_by_source"] = {r["source_kind"]: r["n"] for r in cur.execute(
            "SELECT source_kind, count(*) AS n FROM sessions "
            "WHERE lifecycle_state='archived' GROUP BY 1")}

        out["processing_now"] = [
            {"sid": r["source_session_id"], "source": r["source_kind"],
             "state": r["lifecycle_state"],
             "project": project_of(r["origin_project_name"], r["origin_cwd"],
                                   r["origin_project_component"])}
            for r in cur.execute(
                "SELECT source_session_id, source_kind, lifecycle_state, "
                "origin_project_name, origin_cwd, origin_project_component "
                "FROM sessions WHERE lifecycle_state IN ('processing','summary-pending') "
                "ORDER BY updated_at_ms DESC LIMIT 20")]

        out["recent_archived"] = []
        for r in cur.execute(
                "SELECT source_session_id, source_kind, origin_project_name, origin_cwd, "
                "origin_project_component, current_markdown_relative_path, updated_at_ms "
                "FROM sessions WHERE lifecycle_state='archived' "
                "ORDER BY updated_at_ms DESC LIMIT 10"):
            out["recent_archived"].append({
                "sid": r["source_session_id"], "source": r["source_kind"],
                "project": project_of(r["origin_project_name"], r["origin_cwd"],
                                      r["origin_project_component"]),
                "title": title_from_markdown(r["current_markdown_relative_path"]),
                "at_ms": r["updated_at_ms"]})

        out["recent_failures"] = [
            {"sid": r["source_session_id"], "source": r["source_kind"],
             "project": project_of(r["origin_project_name"], r["origin_cwd"],
                                   r["origin_project_component"]),
             "error_category": r["error_category"], "at_ms": r["finished_at_ms"]}
            for r in cur.execute(
                "SELECT s.source_session_id, s.source_kind, s.origin_project_name, "
                "s.origin_cwd, s.origin_project_component, p.error_category, "
                "p.finished_at_ms FROM processing_attempts p "
                "JOIN sessions s ON s.id = p.session_id WHERE p.outcome='failed' "
                "ORDER BY p.finished_at_ms DESC LIMIT 10")]

        now_ms = int(time.time() * 1000)
        span_ms = 6 * 3600 * 1000
        bin_ms = 30 * 60 * 1000
        start = ((now_ms - span_ms) // bin_ms) * bin_ms
        bins = {}
        for r in cur.execute(
                "SELECT finished_at_ms, outcome FROM processing_attempts "
                "WHERE finished_at_ms IS NOT NULL AND finished_at_ms >= ?", (start,)):
            b = (r["finished_at_ms"] // bin_ms) * bin_ms
            slot = bins.setdefault(b, {"succeeded": 0, "failed": 0,
                                       "recovered": 0, "superseded": 0})
            if r["outcome"] in slot:
                slot[r["outcome"]] += 1
        out["outcome_bins"] = [
            {"bin_start_ms": b, **bins.get(b, {"succeeded": 0, "failed": 0,
                                               "recovered": 0, "superseded": 0})}
            for b in range(start, ((now_ms // bin_ms) + 1) * bin_ms, bin_ms)]

        tail = cur.execute(
            "SELECT operation, category, recorded_at_ms FROM diagnostics "
            "ORDER BY recorded_at_ms DESC LIMIT 5").fetchall()
        out["diagnostics_tail"] = [dict(r) for r in tail]
        con.close()
    except Exception as e:  # noqa: BLE001
        errors.append({"source": "sqlite-query", "message": str(e)[:400]})
        return out or None
    return out


# ------------------------------------------------------------------ collect

def collect():
    errors = []
    status = _munshi_json(["status", "--json"], errors, "munshi status")
    uploads = _munshi_json(["archive-upload", "status", "--json"], errors,
                           "munshi archive-upload status")
    deliveries = _munshi_json(["summary-delivery", "status", "--json"], errors,
                              "munshi summary-delivery status")
    for blob in (uploads, deliveries):
        if blob and "items" in blob:
            del blob["items"]  # keep the payload small; totals are enough
    driver = parse_driver_log(errors)
    data = {
        "generated_at_ms": int(time.time() * 1000),
        "errors": errors,
        "status": status,
        "uploads": uploads,
        "deliveries": deliveries,
        "driver": driver,
        "rate": compute_rate(driver, status),
        "db": query_db(errors),
    }
    return data


def get_data():
    with _lock:
        now = time.time()
        if _cache["data"] is None or now - _cache["at"] > CACHE_TTL_S:
            _cache["data"] = collect()
            _cache["at"] = now
        return _cache["data"]


# ------------------------------------------------------------------- server

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        path = self.path.split("?", 1)[0]
        if path in ("/", "/index.html"):
            try:
                with open(os.path.join(HERE, "index.html"), "rb") as f:
                    body = f.read()
                self._send(200, "text/html; charset=utf-8", body)
            except OSError as e:
                self._send(500, "text/plain", str(e).encode())
        elif path == "/api/data":
            try:
                body = json.dumps(get_data()).encode()
                self._send(200, "application/json", body)
            except Exception as e:  # noqa: BLE001
                self._send(500, "application/json",
                           json.dumps({"error": str(e)[:400]}).encode())
        else:
            self._send(404, "text/plain", b"not found")

    def _send(self, code, ctype, body):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        print("%s - %s" % (self.address_string(), fmt % args), flush=True)


if __name__ == "__main__":
    srv = ThreadingHTTPServer(BIND, Handler)
    print(f"munshi dashboard listening on http://{BIND[0]}:{BIND[1]}", flush=True)
    srv.serve_forever()
