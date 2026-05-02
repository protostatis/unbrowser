#!/usr/bin/env python3
"""T1 — corpus collection harness.

Drives the unbrowser binary against a corpus of domains × task classes
× policy configurations. Captures every Phase A NDJSON event from
stderr to a JSONL file per run. Output is the input for T2 (aggregation).

See docs/probabilistic-policy.md §6 Track 2.

Usage:
  python3 train/collect.py                       # default corpus + matrix
  python3 train/collect.py --corpus train/corpus_v1.txt
  python3 train/collect.py --runs-dir custom/   # output directory
  python3 train/collect.py --only cnbc          # subset matching substring
  python3 train/collect.py --runs-per-cell 3    # repetitions per cell

Output structure:
  train/runs/{timestamp}/
    manifest.json                          # summary of the collection run
    {domain}/
      {task}_{policy}_{repeat}.events.jsonl  # all stderr NDJSON events
      {task}_{policy}_{repeat}.result.json   # the navigate result + outcome
"""
import argparse
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlparse

REPO = Path(__file__).resolve().parents[1]
BIN = REPO / "target" / "release" / "unbrowser"

# Politeness: minimum seconds between consecutive navigates to the same host.
# Random sleep within a window is overkill for v1; a flat floor is enough.
HOST_RATE_LIMIT_SEC = 8.0

# Per-RPC budget. Sites like Forbes/Verge can hit 30s; we set 45s to give
# them room without making bot-blocked sites hang the whole collection.
DISPATCH_BUDGET_MS = 45_000

# Task class definitions — what the "agent task" is for credit assignment.
# extract: run __extract() auto-strategy, succeed if any structured data returns
# query:   run a CSS query and check non-empty result
# (form/click/visual deferred to v2 — they need site-specific scripts)
TASK_DEFS = {
    "extract": {
        "method": "extract",
        "params": {},
        "success_pred": lambda r: bool(r.get("result")) and r.get("result", {}).get("strategy"),
    },
    "query_links": {
        "method": "query",
        "params": {"selector": "a[href]"},
        "success_pred": lambda r: len(r.get("result", []) or []) >= 1,
    },
}

# Policy configurations — what to vary across runs. The matrix exists so
# T2 can attribute outcome differences to specific policy decisions
# (controlled-replay-style strong evidence per spec §4.5 case 2).
POLICY_CONFIGS = {
    "off":       {"flags": []},
    "blocklist": {"flags": ["--policy=blocklist"]},
}


def ts_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def domain_of(url: str) -> str:
    return urlparse(url).netloc


def load_corpus(path: Path) -> list[str]:
    out = []
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        out.append(line)
    return out


class Driver:
    """One unbrowser subprocess; sends RPC, returns parsed responses."""
    def __init__(self, flags: list[str]):
        env = dict(os.environ)
        env["UNBROWSER_TIMEOUT_MS"] = str(DISPATCH_BUDGET_MS)
        self.p = subprocess.Popen(
            [str(BIN)] + flags,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, env=env,
        )
        self._id = 0

    def call(self, method: str, **params) -> dict:
        self._id += 1
        msg = {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params}
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("empty response — binary likely crashed")
        return json.loads(line)

    def close_and_drain(self) -> str:
        try:
            self.call("close")
        except Exception:
            pass
        try:
            _, stderr = self.p.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            self.p.kill()
            _, stderr = self.p.communicate()
        return stderr or ""


def run_cell(url: str, task_name: str, policy_name: str, repeat: int,
             out_dir: Path) -> dict:
    """One (domain × task × policy × repeat) cell. Returns a small summary."""
    task = TASK_DEFS[task_name]
    policy = POLICY_CONFIGS[policy_name]

    drv = Driver(flags=policy["flags"])
    summary = {
        "url": url,
        "task": task_name,
        "policy": policy_name,
        "repeat": repeat,
        "started_at": ts_now(),
        "ok": False,
    }

    try:
        t0 = time.perf_counter()
        nav = drv.call("navigate", url=url, exec_scripts=True)
        nav_ms = (time.perf_counter() - t0) * 1000
        nav_result = nav.get("result") or {}
        nav_id = nav_result.get("navigation_id")
        summary["nav_ms"] = round(nav_ms, 1)
        summary["nav_status"] = nav_result.get("status")
        summary["nav_bytes"] = nav_result.get("bytes")
        summary["navigation_id"] = nav_id
        summary["challenge"] = nav_result.get("challenge")

        # Run the task.
        task_resp = drv.call(task["method"], **task["params"])
        task_success = bool(task["success_pred"](task_resp))
        summary["task_success"] = task_success
        summary["task_method"] = task["method"]

        # Bind outcome back to navigation_id for credit assignment.
        if nav_id:
            drv.call("report_outcome",
                     navigation_id=nav_id,
                     task_class="extract" if task_name == "extract" else "query",
                     success=task_success)

        result_path = out_dir / f"{task_name}_{policy_name}_{repeat}.result.json"
        result_path.write_text(json.dumps({
            "summary": summary,
            "navigate_result": nav_result,
            "task_result": task_resp.get("result") if task_resp.get("result") is not None else task_resp.get("error"),
        }, indent=2))
        summary["ok"] = True
    except Exception as e:
        summary["error"] = str(e)
    finally:
        stderr = drv.close_and_drain()
        events_path = out_dir / f"{task_name}_{policy_name}_{repeat}.events.jsonl"
        # Stderr already comes as one NDJSON event per line; just persist.
        events_path.write_text(stderr)
        summary["events_lines"] = sum(1 for _ in stderr.splitlines() if _.strip())

    return summary


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default=str(REPO / "train" / "corpus_v1.txt"))
    ap.add_argument("--runs-dir", default=None,
                    help="Output dir; default train/runs/{timestamp}/")
    ap.add_argument("--only", default=None,
                    help="Only domains containing this substring")
    ap.add_argument("--runs-per-cell", type=int, default=2)
    ap.add_argument("--tasks", default=",".join(TASK_DEFS),
                    help=f"Comma-separated subset of {list(TASK_DEFS)}")
    ap.add_argument("--policies", default=",".join(POLICY_CONFIGS),
                    help=f"Comma-separated subset of {list(POLICY_CONFIGS)}")
    args = ap.parse_args()

    corpus = load_corpus(Path(args.corpus))
    if args.only:
        corpus = [u for u in corpus if args.only in u]
    if not corpus:
        sys.exit("no URLs after filtering")

    tasks = [t.strip() for t in args.tasks.split(",") if t.strip()]
    policies = [p.strip() for p in args.policies.split(",") if p.strip()]
    for t in tasks:
        assert t in TASK_DEFS, f"unknown task: {t}"
    for p in policies:
        assert p in POLICY_CONFIGS, f"unknown policy: {p}"

    if args.runs_dir:
        runs_dir = Path(args.runs_dir)
    else:
        runs_dir = REPO / "train" / "runs" / ts_now()
    runs_dir.mkdir(parents=True, exist_ok=True)

    if not BIN.exists():
        sys.exit(f"binary not built: {BIN} — run `cargo build --release`")

    print(f"corpus: {len(corpus)} URLs", flush=True)
    print(f"matrix: tasks={tasks} policies={policies} repeats={args.runs_per_cell}", flush=True)
    print(f"total runs: {len(corpus) * len(tasks) * len(policies) * args.runs_per_cell}", flush=True)
    print(f"runs_dir: {runs_dir}", flush=True)
    print()

    last_nav_per_host: dict[str, float] = {}
    summaries = []

    for url in corpus:
        host = domain_of(url)
        out_dir = runs_dir / host.replace(":", "_")
        out_dir.mkdir(parents=True, exist_ok=True)

        for task_name in tasks:
            for policy_name in policies:
                for repeat in range(args.runs_per_cell):
                    # Politeness wait.
                    last = last_nav_per_host.get(host, 0)
                    delta = time.time() - last
                    if delta < HOST_RATE_LIMIT_SEC:
                        time.sleep(HOST_RATE_LIMIT_SEC - delta)

                    label = f"{host} task={task_name} policy={policy_name} run={repeat}"
                    print(f"  {label} ...", end=" ", flush=True)
                    summary = run_cell(url, task_name, policy_name, repeat, out_dir)
                    last_nav_per_host[host] = time.time()
                    summaries.append(summary)

                    if summary.get("ok"):
                        ts = summary.get("task_success")
                        nav_ms = summary.get("nav_ms")
                        ev = summary.get("events_lines")
                        print(f"ok task_success={ts} nav_ms={nav_ms} events={ev}")
                    else:
                        print(f"FAIL {summary.get('error', '')}")

    manifest = {
        "schema_version": 1,
        "started_at": ts_now(),
        "binary": str(BIN),
        "corpus_path": args.corpus,
        "tasks": tasks,
        "policies": policies,
        "runs_per_cell": args.runs_per_cell,
        "host_rate_limit_sec": HOST_RATE_LIMIT_SEC,
        "dispatch_budget_ms": DISPATCH_BUDGET_MS,
        "summaries": summaries,
    }
    (runs_dir / "manifest.json").write_text(json.dumps(manifest, indent=2))
    print(f"\nmanifest: {runs_dir / 'manifest.json'}")

    ok = sum(1 for s in summaries if s.get("ok"))
    task_ok = sum(1 for s in summaries if s.get("task_success"))
    print(f"runs ok: {ok}/{len(summaries)}, task_success: {task_ok}/{len(summaries)}")


if __name__ == "__main__":
    main()
