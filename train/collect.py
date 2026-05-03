#!/usr/bin/env python3
"""T1 — corpus collection harness.

Drives the unbrowser binary against a corpus of URLs in parallel.
Captures every Phase A NDJSON event from stderr to a per-site JSONL
file. Output is the input for T2 (aggregation).

See docs/probabilistic-policy.md §6 Track 2.

Usage:
  python3 train/collect.py                          # default: seed_sites.json (100), 8-way parallel
  python3 train/collect.py --smoke 3                # quick CI sanity (first 3 sites)
  python3 train/collect.py --concurrency 16         # full corpus, more parallelism
  python3 train/collect.py --corpus path/sites.json # custom corpus
  python3 train/collect.py --runs-dir custom/       # output directory
  python3 train/collect.py --only cnbc              # subset matching substring
  python3 train/collect.py --no-policy              # disable --policy=blocklist
  python3 train/collect.py --no-exec-scripts        # navigate without page-script execution
  python3 train/collect.py --legacy-matrix          # restore the old 10-site matrix mode

Output structure:
  train/runs/{timestamp}/
    _summary.json                  # aggregate report (category counts, outcomes)
    manifest.json                  # parameters used for this run
    {domain}/
      navigate.events.jsonl        # all stderr NDJSON events for this site
      result.json                  # per-site outcome + navigate result
"""
from __future__ import annotations

import argparse
import concurrent.futures as futures
import json
import os
import subprocess
import sys
import threading
import time
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlparse

REPO = Path(__file__).resolve().parents[1]

# Binary location — overridable via UNBROWSER_BIN.
DEFAULT_BIN = REPO / "target" / "release" / "unbrowser"


def bin_path() -> Path:
    env_bin = os.environ.get("UNBROWSER_BIN")
    if env_bin:
        return Path(env_bin)
    return DEFAULT_BIN


# Default per-site wall-clock budget. The binary's per-RPC budget is
# tighter (DISPATCH_BUDGET_MS) but we need extra headroom for spawn,
# stderr-flush, close, and slow-but-not-hung sites.
DEFAULT_TIMEOUT_S = 60

# Per-RPC budget passed to the binary via UNBROWSER_TIMEOUT_MS. Bigger
# than the default 30s because Forbes/Verge/CNBC can hit 30+s.
DISPATCH_BUDGET_MS = 45_000

DEFAULT_CORPUS = REPO / "train" / "corpus" / "seed_sites.json"
LEGACY_CORPUS = REPO / "train" / "corpus_v1.txt"

# Outcome categories. Every site lands in exactly one. Used by T2 to
# decide which navigations contribute to which posteriors and which
# get dropped as infrastructure-failure noise.
OUTCOMES = (
    "ok",                # navigate returned 2xx, scripts ran, blockmap present
    "non_2xx",           # binary returned a result but with status >= 400 or 0
    "challenge_blocked", # bot detection challenge surfaced in navigate result
    "exec_error",        # binary's navigate result contained an "error" field
    "parse_error",       # JSON-RPC parse failure on response
    "subprocess_crash",  # process exited unexpectedly
    "timeout",           # exceeded per-site wall clock
    "other",             # caught-and-classified-but-unknown
)


# ---------- corpus loading ---------------------------------------------------

def _load_json_corpus(path: Path) -> list[dict]:
    """Each entry: {url, category, expected_framework?, notes?}."""
    raw = json.loads(path.read_text())
    if not isinstance(raw, list):
        raise ValueError(f"{path}: top-level must be a JSON array")
    out = []
    for i, entry in enumerate(raw):
        if not isinstance(entry, dict) or "url" not in entry:
            raise ValueError(f"{path}: entry {i} missing 'url'")
        out.append(entry)
    return out


def _load_txt_corpus(path: Path) -> list[dict]:
    """One URL per line; '#' comments. Synthesized into entries with category=unknown."""
    out = []
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        out.append({"url": line, "category": "unknown", "expected_framework": None, "notes": ""})
    return out


def load_corpus(path: Path) -> list[dict]:
    if path.suffix == ".json":
        return _load_json_corpus(path)
    return _load_txt_corpus(path)


# ---------- helpers ----------------------------------------------------------

def ts_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def domain_of(url: str) -> str:
    return urlparse(url).netloc


def safe_dir_name(host: str) -> str:
    # Avoid ':' in filenames (mostly only an issue if a port shows up,
    # but cheap insurance for Windows-friendly paths too).
    return host.replace(":", "_") or "unknown"


# ---------- per-site runner --------------------------------------------------

def _spawn(binary: Path, flags: list[str], events_path: Path) -> subprocess.Popen:
    """Spawn the binary with stderr streamed directly to events_path.

    CRITICAL: stderr=open(events_path, 'w'), NOT stderr=PIPE. A noisy
    SPA emitting many script_decision/script_executed events fills the
    OS pipe buffer (~64 KB) before the binary writes its JSON-RPC
    response, blocking the child on stderr write while the parent
    blocks on stdout read. (PR #5 review HIGH.)
    """
    env = dict(os.environ)
    env["UNBROWSER_TIMEOUT_MS"] = str(DISPATCH_BUDGET_MS)
    fh = open(events_path, "w")
    p = subprocess.Popen(
        [str(binary)] + flags,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=fh,
        text=True, env=env,
    )
    # Stash the file handle on the popen so the caller can close it.
    p._events_fh = fh  # type: ignore[attr-defined]
    return p


def _classify_outcome(navigate_result: dict | None, navigate_error: dict | None) -> str:
    """Map a navigate response to one of OUTCOMES."""
    if navigate_error:
        return "exec_error"
    if not isinstance(navigate_result, dict):
        return "other"
    if navigate_result.get("challenge"):
        return "challenge_blocked"
    status = navigate_result.get("status")
    if isinstance(status, int) and status >= 200 and status < 400:
        return "ok"
    if isinstance(status, int) and status >= 400:
        return "non_2xx"
    if status == 0 or status is None:
        return "non_2xx"
    return "other"


def run_site(site: dict, *, binary: Path, out_root: Path,
             policy_blocklist: bool, exec_scripts: bool,
             timeout_s: float, retry_once: bool = True) -> dict:
    """Run one site end-to-end. Returns a per-site summary dict."""
    url = site["url"]
    host = domain_of(url)
    out_dir = out_root / safe_dir_name(host)
    out_dir.mkdir(parents=True, exist_ok=True)

    events_path = out_dir / "navigate.events.jsonl"
    result_path = out_dir / "result.json"

    flags: list[str] = []
    if policy_blocklist:
        flags.append("--policy=blocklist")

    summary = {
        "url": url,
        "host": host,
        "category": site.get("category"),
        "expected_framework": site.get("expected_framework"),
        "started_at": ts_now(),
        "outcome": "other",
        "attempts": 0,
        "wall_ms": None,
        "events_lines": 0,
        "nav_status": None,
        "nav_bytes": None,
        "navigation_id": None,
        "challenge": None,
        "error": None,
        "retried": False,
    }

    last_error: str | None = None
    nav_result: dict | None = None
    nav_error: dict | None = None

    attempt_max = 2 if retry_once else 1
    for attempt in range(1, attempt_max + 1):
        summary["attempts"] = attempt
        if attempt > 1:
            summary["retried"] = True
        # Fresh events file per attempt (overwrite — last attempt wins).
        try:
            events_path.unlink()
        except OSError:
            pass

        p = _spawn(binary, flags, events_path)
        t0 = time.perf_counter()
        nav_result = None
        nav_error = None
        outcome_from_attempt: str | None = None

        try:
            req = {
                "jsonrpc": "2.0", "id": 1, "method": "navigate",
                "params": {"url": url, "exec_scripts": bool(exec_scripts)},
            }
            try:
                p.stdin.write(json.dumps(req) + "\n")
                p.stdin.flush()
            except (BrokenPipeError, OSError) as e:
                last_error = f"stdin write failed: {e}"
                outcome_from_attempt = "subprocess_crash"
                raise

            # Read response with wall-clock budget.
            line = _read_line_with_timeout(p, timeout_s)
            wall_ms = (time.perf_counter() - t0) * 1000
            summary["wall_ms"] = round(wall_ms, 1)

            if line is None:
                last_error = f"timeout after {timeout_s}s"
                outcome_from_attempt = "timeout"
                raise TimeoutError(last_error)

            try:
                resp = json.loads(line)
            except json.JSONDecodeError as e:
                last_error = f"invalid JSON-RPC response: {e}"
                outcome_from_attempt = "parse_error"
                raise

            nav_result = resp.get("result")
            nav_error = resp.get("error")
            if isinstance(nav_result, dict):
                summary["nav_status"] = nav_result.get("status")
                summary["nav_bytes"] = nav_result.get("bytes")
                summary["navigation_id"] = nav_result.get("navigation_id")
                summary["challenge"] = nav_result.get("challenge")

            outcome = _classify_outcome(nav_result, nav_error)
            outcome_from_attempt = outcome
            summary["outcome"] = outcome
            if nav_error:
                last_error = json.dumps(nav_error)[:500]

            # Bind outcome back to navigation_id for credit assignment.
            nav_id = summary["navigation_id"]
            if nav_id:
                try:
                    rep = {
                        "jsonrpc": "2.0", "id": 2, "method": "report_outcome",
                        "params": {
                            "navigation_id": nav_id,
                            "task_class": "extract",
                            "success": outcome == "ok",
                        },
                    }
                    p.stdin.write(json.dumps(rep) + "\n")
                    p.stdin.flush()
                    _read_line_with_timeout(p, 5.0)  # best-effort drain
                except Exception:
                    pass  # outcome reporting is best-effort

            # Persist per-site result.
            result_path.write_text(json.dumps({
                "summary": summary,
                "navigate_result": nav_result,
                "navigate_error": nav_error,
            }, indent=2, default=str))

        except TimeoutError:
            pass  # outcome already set
        except Exception as e:
            last_error = last_error or f"{type(e).__name__}: {e}"
            if outcome_from_attempt is None:
                outcome_from_attempt = "subprocess_crash"
            summary["outcome"] = outcome_from_attempt
        finally:
            _shutdown(p)

        # Decide whether to retry. Only retry on infra-style failures.
        if outcome_from_attempt in ("timeout", "subprocess_crash") and attempt < attempt_max:
            continue
        break

    # Count event lines (best-effort).
    try:
        with open(events_path) as f:
            summary["events_lines"] = sum(1 for line in f if line.strip())
    except OSError:
        summary["events_lines"] = 0

    if last_error and not summary.get("error"):
        summary["error"] = last_error

    # If nav succeeded but result.json wasn't written (e.g. hard-error path), write summary alone.
    if not result_path.exists():
        try:
            result_path.write_text(json.dumps({
                "summary": summary,
                "navigate_result": nav_result,
                "navigate_error": nav_error,
            }, indent=2, default=str))
        except OSError:
            pass

    return summary


def _read_line_with_timeout(p: subprocess.Popen, timeout_s: float) -> str | None:
    """Read one stdout line from the child with a wall-clock budget.

    On timeout, kill the process. Avoids select() since stdout is text-mode
    and we want a portable line read; uses a watchdog thread instead.
    """
    result: dict = {"line": None}
    done = threading.Event()

    def _reader():
        try:
            result["line"] = p.stdout.readline()
        except Exception:
            result["line"] = None
        finally:
            done.set()

    t = threading.Thread(target=_reader, daemon=True)
    t.start()
    done.wait(timeout=timeout_s)
    if not done.is_set():
        # Timeout: kill child so the reader thread unblocks.
        try:
            p.kill()
        except OSError:
            pass
        done.wait(timeout=2.0)
        return None
    line = result.get("line")
    if not line:
        return None
    return line


def _shutdown(p: subprocess.Popen) -> None:
    """Ask the binary to close cleanly, then ensure it's reaped.

    Closes stdin/stdout/events-file explicitly so we never trip a
    ResourceWarning under pytest/unittest (the GC eventually reaps
    them but warns loudly first).
    """
    try:
        if p.stdin and not p.stdin.closed:
            p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 99,
                                      "method": "close", "params": {}}) + "\n")
            p.stdin.flush()
    except Exception:
        pass
    try:
        p.wait(timeout=3)
    except subprocess.TimeoutExpired:
        try:
            p.kill()
        except OSError:
            pass
        try:
            p.wait(timeout=2)
        except Exception:
            pass
    for stream in (p.stdin, p.stdout):
        if stream is not None:
            try:
                stream.close()
            except Exception:
                pass
    fh = getattr(p, "_events_fh", None)
    if fh is not None:
        try:
            fh.flush()
            fh.close()
        except Exception:
            pass


# ---------- driver -----------------------------------------------------------

def collect(corpus: list[dict], *, binary: Path, runs_dir: Path,
            concurrency: int, timeout_s: float, policy_blocklist: bool,
            exec_scripts: bool, retry_once: bool = True,
            progress_stream=sys.stderr) -> dict:
    """Run the corpus through `concurrency` workers; write summary; return it."""
    runs_dir.mkdir(parents=True, exist_ok=True)
    n = len(corpus)
    completed = 0
    completed_lock = threading.Lock()
    summaries: list[dict] = []

    def _go(site: dict) -> dict:
        s = run_site(site, binary=binary, out_root=runs_dir,
                     policy_blocklist=policy_blocklist,
                     exec_scripts=exec_scripts,
                     timeout_s=timeout_s, retry_once=retry_once)
        return s

    started = time.perf_counter()
    with futures.ThreadPoolExecutor(max_workers=max(1, concurrency)) as ex:
        future_map = {ex.submit(_go, site): site for site in corpus}
        for fut in futures.as_completed(future_map):
            site = future_map[fut]
            try:
                s = fut.result()
            except Exception as e:
                s = {
                    "url": site.get("url"),
                    "host": domain_of(site.get("url", "") or ""),
                    "category": site.get("category"),
                    "outcome": "other",
                    "error": f"{type(e).__name__}: {e}",
                    "wall_ms": None,
                    "events_lines": 0,
                }
            summaries.append(s)
            with completed_lock:
                completed += 1
                idx = completed
            tag = s.get("outcome", "other").upper()
            wall = s.get("wall_ms")
            wall_str = f"{wall/1000:.1f}s" if isinstance(wall, (int, float)) else "  -  "
            ev = s.get("events_lines") or 0
            print(f"[{idx:>3d}/{n}] {tag:<18s} {s.get('url','')}  "
                  f"{ev} events  {wall_str}",
                  file=progress_stream, flush=True)
    elapsed = time.perf_counter() - started

    cats = Counter(s.get("category") or "unknown" for s in summaries)
    outcomes = Counter(s.get("outcome") or "other" for s in summaries)
    summary = {
        "schema_version": 1,
        "started_at": ts_now(),
        "elapsed_s": round(elapsed, 1),
        "binary": str(binary),
        "runs_dir": str(runs_dir),
        "n_sites": n,
        "concurrency": concurrency,
        "timeout_s": timeout_s,
        "policy_blocklist": policy_blocklist,
        "exec_scripts": exec_scripts,
        "retry_once": retry_once,
        "outcomes": dict(outcomes),
        "categories": dict(cats),
        "by_category_outcomes": _crosstab(summaries),
        "ok": outcomes.get("ok", 0),
        "summaries": summaries,
    }
    (runs_dir / "_summary.json").write_text(json.dumps(summary, indent=2, default=str))
    return summary


def _crosstab(summaries: list[dict]) -> dict:
    """category × outcome counts."""
    out: dict[str, Counter] = {}
    for s in summaries:
        cat = s.get("category") or "unknown"
        out.setdefault(cat, Counter())[s.get("outcome") or "other"] += 1
    return {cat: dict(c) for cat, c in out.items()}


# ---------- legacy matrix mode (back-compat) ---------------------------------

# Imported lazily to keep the parallel path lean.
def _legacy_main(args) -> int:
    """Restore PR #5-era behaviour: site × task × policy × repeat matrix.

    Uses the v1 corpus_v1.txt by default. Single-process, serial.
    """
    from urllib.parse import urlparse as _urlparse

    HOST_RATE_LIMIT_SEC = 8.0

    TASK_DEFS = {
        "extract": {
            "method": "extract", "params": {},
            "success_pred": lambda r: bool(r.get("result")) and (r.get("result") or {}).get("strategy"),
        },
        "query_links": {
            "method": "query", "params": {"selector": "a[href]"},
            "success_pred": lambda r: len(r.get("result", []) or []) >= 1,
        },
    }
    POLICY_CONFIGS = {
        "off": {"flags": []},
        "blocklist": {"flags": ["--policy=blocklist"]},
    }

    corpus_path = Path(args.corpus) if args.corpus else LEGACY_CORPUS
    corpus_lines = _load_txt_corpus(corpus_path)
    urls = [c["url"] for c in corpus_lines]
    if args.only:
        urls = [u for u in urls if args.only in u]
    if not urls:
        print("no URLs after filtering", file=sys.stderr)
        return 2

    tasks = [t.strip() for t in (args.tasks or ",".join(TASK_DEFS)).split(",") if t.strip()]
    policies = [p.strip() for p in (args.policies or ",".join(POLICY_CONFIGS)).split(",") if p.strip()]
    for t in tasks:
        if t not in TASK_DEFS:
            print(f"unknown task: {t}", file=sys.stderr); return 2
    for p in policies:
        if p not in POLICY_CONFIGS:
            print(f"unknown policy: {p}", file=sys.stderr); return 2

    runs_dir = Path(args.runs_dir) if args.runs_dir else REPO / "train" / "runs" / ts_now()
    runs_dir.mkdir(parents=True, exist_ok=True)

    binary = bin_path()
    if not binary.exists():
        print(f"binary not built: {binary} — run `cargo build --release`", file=sys.stderr)
        return 2

    print(f"[legacy-matrix] corpus: {len(urls)} URLs", flush=True)
    print(f"[legacy-matrix] matrix: tasks={tasks} policies={policies} repeats={args.runs_per_cell}", flush=True)
    print(f"[legacy-matrix] runs_dir: {runs_dir}", flush=True)

    last_nav_per_host: dict[str, float] = {}
    summaries: list[dict] = []

    def _drv_call(p: subprocess.Popen, msg: dict) -> dict:
        p.stdin.write(json.dumps(msg) + "\n"); p.stdin.flush()
        line = p.stdout.readline()
        if not line:
            raise RuntimeError("empty response — binary likely crashed")
        return json.loads(line)

    for url in urls:
        host = _urlparse(url).netloc
        out_dir = runs_dir / safe_dir_name(host)
        out_dir.mkdir(parents=True, exist_ok=True)

        for task_name in tasks:
            for policy_name in policies:
                for repeat in range(args.runs_per_cell):
                    last = last_nav_per_host.get(host, 0)
                    delta = time.time() - last
                    if delta < HOST_RATE_LIMIT_SEC:
                        time.sleep(HOST_RATE_LIMIT_SEC - delta)

                    label = f"{host} task={task_name} policy={policy_name} run={repeat}"
                    print(f"  {label} ...", end=" ", flush=True)
                    events_path = out_dir / f"{task_name}_{policy_name}_{repeat}.events.jsonl"
                    p = _spawn(binary, POLICY_CONFIGS[policy_name]["flags"], events_path)
                    cell_summary = {"url": url, "task": task_name, "policy": policy_name,
                                    "repeat": repeat, "started_at": ts_now(), "ok": False}
                    try:
                        nav = _drv_call(p, {"jsonrpc": "2.0", "id": 1, "method": "navigate",
                                             "params": {"url": url, "exec_scripts": True}})
                        nav_result = nav.get("result") or {}
                        cell_summary.update({
                            "nav_status": nav_result.get("status"),
                            "nav_bytes": nav_result.get("bytes"),
                            "navigation_id": nav_result.get("navigation_id"),
                            "challenge": nav_result.get("challenge"),
                        })
                        td = TASK_DEFS[task_name]
                        task_resp = _drv_call(p, {"jsonrpc": "2.0", "id": 2,
                                                   "method": td["method"], "params": td["params"]})
                        task_success = bool(td["success_pred"](task_resp))
                        cell_summary["task_success"] = task_success
                        nav_id = cell_summary.get("navigation_id")
                        if nav_id:
                            _drv_call(p, {"jsonrpc": "2.0", "id": 3, "method": "report_outcome",
                                          "params": {"navigation_id": nav_id,
                                                     "task_class": "extract" if task_name == "extract" else "query",
                                                     "success": task_success}})
                        result_path = out_dir / f"{task_name}_{policy_name}_{repeat}.result.json"
                        result_path.write_text(json.dumps({
                            "summary": cell_summary,
                            "navigate_result": nav_result,
                            "task_result": task_resp.get("result") if task_resp.get("result") is not None else task_resp.get("error"),
                        }, indent=2, default=str))
                        cell_summary["ok"] = True
                    except Exception as e:
                        cell_summary["error"] = str(e)
                    finally:
                        _shutdown(p)
                        try:
                            with open(events_path) as f:
                                cell_summary["events_lines"] = sum(1 for line in f if line.strip())
                        except OSError:
                            cell_summary["events_lines"] = 0

                    last_nav_per_host[host] = time.time()
                    summaries.append(cell_summary)
                    if cell_summary["ok"]:
                        print(f"ok task_success={cell_summary.get('task_success')} "
                              f"events={cell_summary.get('events_lines')}")
                    else:
                        print(f"FAIL {cell_summary.get('error', '')}")

    manifest = {
        "schema_version": 1, "mode": "legacy-matrix",
        "started_at": ts_now(),
        "binary": str(binary),
        "corpus_path": str(corpus_path),
        "tasks": tasks, "policies": policies,
        "runs_per_cell": args.runs_per_cell,
        "host_rate_limit_sec": HOST_RATE_LIMIT_SEC,
        "dispatch_budget_ms": DISPATCH_BUDGET_MS,
        "summaries": summaries,
    }
    (runs_dir / "manifest.json").write_text(json.dumps(manifest, indent=2, default=str))
    ok = sum(1 for s in summaries if s.get("ok"))
    task_ok = sum(1 for s in summaries if s.get("task_success"))
    print(f"runs ok: {ok}/{len(summaries)}, task_success: {task_ok}/{len(summaries)}")
    return 0


# ---------- CLI --------------------------------------------------------------

def build_argparser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--corpus", default=None,
                    help="Path to corpus (.json list of {url, category, ...} or .txt of one-URL-per-line). "
                         f"Default: {DEFAULT_CORPUS.relative_to(REPO)}")
    ap.add_argument("--runs-dir", default=None,
                    help="Output directory; default train/runs/{timestamp}/")
    ap.add_argument("--concurrency", type=int, default=8,
                    help="Number of parallel unbrowser subprocesses (default 8)")
    ap.add_argument("--timeout-s", type=float, default=DEFAULT_TIMEOUT_S,
                    help=f"Per-site wall-clock budget in seconds (default {DEFAULT_TIMEOUT_S})")
    ap.add_argument("--smoke", type=int, nargs="?", const=3, default=None,
                    help="Smoke mode: take first N sites (default 3 if --smoke given without value)")
    ap.add_argument("--only", default=None,
                    help="Substring filter on URL")
    ap.add_argument("--no-policy", action="store_true",
                    help="Disable --policy=blocklist (default: enabled)")
    ap.add_argument("--no-exec-scripts", action="store_true",
                    help="Don't execute page scripts (default: enabled)")
    ap.add_argument("--no-retry", action="store_true",
                    help="Don't retry on timeout/subprocess_crash")
    # Legacy-matrix mode toggles (kept for backward-compat with PR #5):
    ap.add_argument("--legacy-matrix", action="store_true",
                    help="Run the old site×task×policy×repeat matrix (single-process serial)")
    ap.add_argument("--runs-per-cell", type=int, default=2,
                    help="(legacy-matrix only) repetitions per cell")
    ap.add_argument("--tasks", default=None,
                    help="(legacy-matrix only) comma-separated tasks")
    ap.add_argument("--policies", default=None,
                    help="(legacy-matrix only) comma-separated policies")
    return ap


def main(argv: list[str] | None = None) -> int:
    args = build_argparser().parse_args(argv)

    if args.legacy_matrix:
        return _legacy_main(args)

    binary = bin_path()
    if not binary.exists():
        print(f"binary not built: {binary} — run `cargo build --release` "
              f"or set UNBROWSER_BIN", file=sys.stderr)
        return 2

    corpus_path = Path(args.corpus) if args.corpus else DEFAULT_CORPUS
    if not corpus_path.exists():
        print(f"corpus not found: {corpus_path}", file=sys.stderr)
        return 2

    corpus = load_corpus(corpus_path)
    if args.only:
        corpus = [c for c in corpus if args.only in c["url"]]
    if args.smoke is not None:
        corpus = corpus[: max(1, args.smoke)]
    if not corpus:
        print("no URLs after filtering", file=sys.stderr)
        return 2

    runs_dir = Path(args.runs_dir) if args.runs_dir else REPO / "train" / "runs" / ts_now()
    runs_dir.mkdir(parents=True, exist_ok=True)

    print(f"corpus: {len(corpus)} URLs from {corpus_path}", file=sys.stderr, flush=True)
    print(f"runs_dir: {runs_dir}", file=sys.stderr, flush=True)
    print(f"concurrency={args.concurrency} timeout_s={args.timeout_s} "
          f"policy={'blocklist' if not args.no_policy else 'off'} "
          f"exec_scripts={not args.no_exec_scripts} "
          f"retry_once={not args.no_retry}",
          file=sys.stderr, flush=True)

    summary = collect(
        corpus,
        binary=binary,
        runs_dir=runs_dir,
        concurrency=args.concurrency,
        timeout_s=args.timeout_s,
        policy_blocklist=not args.no_policy,
        exec_scripts=not args.no_exec_scripts,
        retry_once=not args.no_retry,
    )

    # Manifest is a separate artifact for parity with legacy mode.
    manifest = {
        "schema_version": 1, "mode": "parallel",
        "started_at": summary["started_at"],
        "binary": str(binary),
        "corpus_path": str(corpus_path),
        "concurrency": summary["concurrency"],
        "timeout_s": summary["timeout_s"],
        "policy_blocklist": summary["policy_blocklist"],
        "exec_scripts": summary["exec_scripts"],
        "retry_once": summary["retry_once"],
        "n_sites": summary["n_sites"],
    }
    (runs_dir / "manifest.json").write_text(json.dumps(manifest, indent=2, default=str))

    # Final, machine-readable line on stdout (so CI can parse it).
    print(json.dumps({
        "runs_dir": str(runs_dir),
        "elapsed_s": summary["elapsed_s"],
        "outcomes": summary["outcomes"],
        "categories": summary["categories"],
    }))

    # Print human summary to stderr.
    print(file=sys.stderr)
    print(f"done in {summary['elapsed_s']}s. outcomes:", file=sys.stderr)
    for k in OUTCOMES:
        v = summary["outcomes"].get(k, 0)
        if v:
            print(f"  {k:<18s} {v}", file=sys.stderr)
    print(f"summary: {runs_dir / '_summary.json'}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
