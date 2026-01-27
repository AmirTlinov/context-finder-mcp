#!/usr/bin/env python3
"""Benchmark harness for Context.

Loads an audit candidates dataset and iterates through repositories, running:
  1. context command --json '{"action":"index", ...}'
  2. context command --json '{"action":"search", ...}' for each positive query.
  3. context command --json '{"action":"search", ...}' for negative examples.

Outputs consolidated metrics (time_ms, max_rss_kb, precision@k, false positives)
to JSON (bench/results/<timestamp>.json by default) and stores raw logs.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import shutil
import subprocess
import tempfile
import time
import threading
from pathlib import Path
from typing import Any, Dict, List, Set


REPO_ROOT = Path(__file__).resolve().parents[1]


def default_cli_path() -> Path:
    preferred = REPO_ROOT / "target/release/context"
    legacy = REPO_ROOT / "target/release/context-finder"
    return preferred if preferred.exists() else legacy


DEFAULT_CLI = default_cli_path()
DEFAULT_CANDIDATES_EXAMPLE = REPO_ROOT / "data/audit_candidates.json"
DEFAULT_CANDIDATES_LOCAL = REPO_ROOT / "data/audit_candidates.local.json"
DEFAULT_RESULTS_DIR = REPO_ROOT / "bench/results"
DEFAULT_LOG_DIR = REPO_ROOT / "bench/logs"


def default_candidates_path() -> Path:
    return (
        DEFAULT_CANDIDATES_LOCAL
        if DEFAULT_CANDIDATES_LOCAL.exists()
        else DEFAULT_CANDIDATES_EXAMPLE
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run Context benchmark harness")
    parser.add_argument(
        "--cli", type=Path, default=DEFAULT_CLI, help="Path to context binary"
    )
    parser.add_argument(
        "--candidates",
        type=Path,
        default=default_candidates_path(),
        help="Audit candidates JSON (defaults to local override if present, else the example dataset)",
    )
    parser.add_argument("--limit", type=int, default=10, help="search limit per query")
    parser.add_argument("--k", type=int, default=5, help="precision@k value")
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Path to final JSON report (defaults to bench/results/<timestamp>.json)",
    )
    parser.add_argument(
        "--skip-index",
        action="store_true",
        help="Skip indexing step (reuse existing index)",
    )
    parser.add_argument(
        "--reset-index",
        action="store_true",
        help="Remove local index dirs before indexing for deterministic full builds",
    )
    parser.add_argument(
        "--log-dir",
        type=Path,
        default=DEFAULT_LOG_DIR,
        help="Directory for raw command logs",
    )
    parser.add_argument(
        "--results-dir",
        type=Path,
        default=DEFAULT_RESULTS_DIR,
        help="Directory for reports when --output omitted",
    )
    parser.add_argument(
        "--include",
        nargs="*",
        default=None,
        help="Subset of repo names to benchmark (matches candidate 'name')",
    )
    parser.add_argument(
        "--start-from",
        type=str,
        default=None,
        help="Skip repos until this name is encountered",
    )
    parser.add_argument(
        "--progress-interval",
        type=int,
        default=30,
        help="Seconds between progress logs for long-running commands",
    )
    parser.add_argument(
        "--progress-files",
        type=int,
        default=500,
        help="Emit progress every N files scanned/indexed (requires index logs)",
    )
    parser.add_argument(
        "--resume",
        action="store_true",
        help="Resume existing report (requires --output pointing to existing file)",
    )
    parser.add_argument(
        "--trace",
        action="store_true",
        help="Pass --trace to each search to inspect scoring",
    )
    parser.add_argument(
        "--model",
        type=str,
        default=None,
        help="Embedding model id (sets CONTEXT_EMBEDDING_MODEL)",
    )
    parser.add_argument(
        "--model-dir",
        type=Path,
        default=None,
        help="Model cache directory (sets CONTEXT_MODEL_DIR)",
    )
    parser.add_argument(
        "--cuda-device",
        type=int,
        default=None,
        help="CUDA device id (sets CONTEXT_CUDA_DEVICE)",
    )
    parser.add_argument(
        "--cuda-mem-limit-mb",
        type=int,
        default=None,
        help="CUDA memory limit in MB (sets CONTEXT_CUDA_MEM_LIMIT_MB)",
    )
    parser.add_argument(
        "--profile",
        type=str,
        default=None,
        help="Search profile to use (e.g. general or targeted/venorus)",
    )
    return parser.parse_args()


def load_candidates(path: Path) -> List[Dict[str, Any]]:
    if not path.exists():
        raise FileNotFoundError(f"candidates file not found: {path}")
    return json.loads(path.read_text())


def run_command(
    cmd: List[str],
    cwd: Path | None = None,
    progress_label: str | None = None,
    interval: int = 30,
    env: Dict[str, str] | None = None,
) -> Dict[str, Any]:
    """Run a command via /usr/bin/time, print progress heartbeat every `interval` seconds."""

    time_file = tempfile.NamedTemporaryFile(delete=False)
    time_file.close()
    full_cmd = ["/usr/bin/time", "-f", "%M", "-o", time_file.name] + cmd

    start = time.perf_counter()

    process = subprocess.Popen(
        full_cmd,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )

    stdout_lines: List[str] = []
    stderr_lines: List[str] = []

    def _drain(pipe: Any, sink: List[str]) -> None:
        if pipe is None:
            return
        try:
            for chunk in iter(pipe.readline, ""):
                if not chunk:
                    break
                sink.append(chunk)
        finally:
            pipe.close()

    stdout_thread = threading.Thread(target=_drain, args=(process.stdout, stdout_lines))
    stderr_thread = threading.Thread(target=_drain, args=(process.stderr, stderr_lines))
    stdout_thread.daemon = True
    stderr_thread.daemon = True
    stdout_thread.start()
    stderr_thread.start()

    last_heartbeat = time.perf_counter()
    while True:
        if process.poll() is not None:
            break
        now = time.perf_counter()
        if now - last_heartbeat >= interval:
            label = progress_label or "cmd"
            elapsed = now - start
            print(f"[progress] {label} running {elapsed:.1f}s", flush=True)
            last_heartbeat = now
        time.sleep(0.5)

    process.wait()
    stdout_thread.join()
    stderr_thread.join()

    duration_ms = (time.perf_counter() - start) * 1000.0
    max_kb = 0
    try:
        with open(time_file.name, "r", encoding="utf-8") as fh:
            content = fh.read().strip()
            if content:
                digits = [
                    line for line in content.splitlines() if line.strip().isdigit()
                ]
                if digits:
                    max_kb = int(digits[-1])
    finally:
        os.unlink(time_file.name)

    return {
        "cmd": cmd,
        "returncode": process.returncode,
        "stdout": "".join(stdout_lines),
        "stderr": "".join(stderr_lines),
        "time_ms": duration_ms,
        "max_rss_kb": max_kb,
    }


def normalize_relative(path_value: str, repo_root: Path) -> str:
    repo_root = repo_root.resolve()
    candidate = Path(path_value)
    if not candidate.is_absolute():
        candidate = repo_root / candidate
    candidate = candidate.resolve()
    try:
        return candidate.relative_to(repo_root).as_posix()
    except ValueError:
        return candidate.as_posix()


def build_relevant_set(relevant_files: List[str], repo_root: Path) -> set[str]:
    repo_root = repo_root.resolve()
    normalized = set()
    for rel in relevant_files:
        normalized.add(normalize_relative(rel, repo_root))
    return normalized


def compute_precision(
    results: List[Dict[str, Any]],
    relevant_files: List[str],
    repo_root: Path,
    k: int,
) -> Dict[str, Any]:
    top = results[:k]
    relevant_set = build_relevant_set(relevant_files, repo_root)
    normalized_top = [normalize_relative(r.get("file", ""), repo_root) for r in top]
    hits = [path for path in normalized_top if path in relevant_set]
    precision = len(hits) / len(top) if top else 0.0
    return {
        "precision_at_k": precision,
        "hits": hits,
        "top_files": normalized_top,
    }


def evaluate_negative(
    results: List[Dict[str, Any]], repo_root: Path, k: int
) -> Dict[str, Any]:
    top = results[:k]
    normalized = [normalize_relative(r.get("file", ""), repo_root) for r in top]
    return {
        "top_files": normalized,
        "false_positive": bool(normalized),
    }


def ensure_executable(path: Path) -> None:
    if not path.exists():
        raise FileNotFoundError(f"context binary not found at {path}")
    if not os.access(path, os.X_OK):
        raise PermissionError(f"context binary not executable: {path}")


def resolve_cli_path(path: Path) -> Path:
    if path.exists():
        return path

    # Convenience: if the default points to the preferred name but only the legacy
    # alias is built, transparently fall back.
    if path.name == "context":
        legacy = path.with_name("context-finder")
        if legacy.exists():
            return legacy
    return path


def main() -> None:
    args = parse_args()
    args.cli = resolve_cli_path(args.cli)
    ensure_executable(args.cli)
    candidates = load_candidates(args.candidates)
    args.log_dir.mkdir(parents=True, exist_ok=True)
    results_path = args.output
    timestamp = dt.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    if results_path is None:
        args.results_dir.mkdir(parents=True, exist_ok=True)
        results_path = args.results_dir / f"bench_{timestamp}.json"
    elif args.resume and not results_path.exists():
        raise FileNotFoundError(f"Cannot resume: {results_path} does not exist")

    default_report: Dict[str, Any] = {
        "generated_at": timestamp,
        "cli": str(args.cli),
        "limit": args.limit,
        "k": args.k,
        "profile": args.profile or "general",
        "repos": [],
    }

    if args.resume and results_path.exists():
        print(f"[bench] resume mode: loading {results_path}")
        report = json.loads(results_path.read_text())
    else:
        report = default_report

    report["generated_at"] = timestamp
    report["cli"] = str(args.cli)
    report["limit"] = args.limit
    report["k"] = args.k
    report["embedding_model"] = args.model
    report["profile"] = args.profile or "general"

    base_env = os.environ.copy()
    if args.model:
        base_env["CONTEXT_EMBEDDING_MODEL"] = args.model
    if args.model_dir:
        base_env["CONTEXT_MODEL_DIR"] = str(args.model_dir)
    if args.cuda_device is not None:
        base_env["CONTEXT_CUDA_DEVICE"] = str(args.cuda_device)
    if args.cuda_mem_limit_mb is not None:
        base_env["CONTEXT_CUDA_MEM_LIMIT_MB"] = str(args.cuda_mem_limit_mb)
    if args.profile:
        base_env["CONTEXT_PROFILE"] = args.profile
    # Prefer locally downloaded ORT GPU libs if present
    ort_lib = Path(
        base_env.get(
            "ORT_LIB_LOCATION",
            Path.home()
            / ".cache"
            / "ort.pyke.io"
            / "dfbin"
            / "x86_64-unknown-linux-gnu"
            / "8BBB8416566A668A240B72A56DBBB82F99F430AF86F64D776D7EBF53E144EFC9"
            / "onnxruntime"
            / "lib",
        )
    )
    if ort_lib.exists():
        base_env["ORT_LIB_LOCATION"] = str(ort_lib)
        ld_path = base_env.get("LD_LIBRARY_PATH", "")
        merged = f"{ort_lib}:{ld_path}" if ld_path else str(ort_lib)
        base_env["LD_LIBRARY_PATH"] = merged
    # NVIDIA pip-installed libs (cuBLAS/cuDNN/cudart)
    nvidia_libs = [
        Path.home() / ".local/lib/python3.12/site-packages/nvidia/cublas/lib",
        Path.home() / ".local/lib/python3.12/site-packages/nvidia/cuda_runtime/lib",
        Path.home() / ".local/lib/python3.12/site-packages/nvidia/curand/lib",
        Path.home() / ".local/lib/python3.12/site-packages/nvidia/cufft/lib",
        Path.home() / ".local/lib/python3.12/site-packages/nvidia/cudnn/lib",
    ]
    for libdir in nvidia_libs:
        if libdir.exists():
            ld_path = base_env.get("LD_LIBRARY_PATH", "")
            base_env["LD_LIBRARY_PATH"] = (
                f"{libdir}:{ld_path}" if ld_path else str(libdir)
            )

    filtered = candidates
    if args.include:
        include_set = set(args.include)
        filtered = [entry for entry in candidates if entry.get("name") in include_set]

    started = args.start_from is None
    processed_names: Set[str] = set()
    for existing in report.get("repos", []):
        name = existing.get("name")
        if isinstance(name, str):
            processed_names.add(name)

    for entry in filtered:
        if not started:
            if entry.get("name") == args.start_from:
                started = True
            else:
                continue

        repo_name = entry.get("name")
        if args.resume and isinstance(repo_name, str) and repo_name in processed_names:
            print(f"[bench] skipping {repo_name} (already processed)")
            continue

        repo_path = Path(entry["path"]).expanduser().resolve()
        if not repo_path.exists():
            print(f"[WARN] skip missing repo {repo_path}")
            continue

        repo_log_dir = args.log_dir / f"{entry['name']}_{timestamp}"
        repo_log_dir.mkdir(parents=True, exist_ok=True)
        repo_record: Dict[str, Any] = {
            "name": entry.get("name"),
            "path": str(repo_path),
            "files": entry.get("files"),
            "language_count": entry.get("language_count"),
            "index": None,
            "queries": [],
            "negative_examples": [],
        }

        if args.reset_index and not args.skip_index:
            candidates = [
                repo_path / ".agents/mcp/.context",
                repo_path / ".agents/mcp/context/.context",
                repo_path / ".context",
                repo_path / ".context-finder",
            ]
            for index_dir in candidates:
                if index_dir.exists():
                    shutil.rmtree(index_dir)

        if not args.skip_index:
            idx_cmd = [str(args.cli)]
            if args.profile:
                idx_cmd.extend(["--profile", args.profile])
            idx_cmd.extend(
                [
                    "command",
                    "--json",
                    json.dumps(
                        {
                            "action": "index",
                            "payload": {"path": str(repo_path), "full": True},
                        }
                    ),
                ]
            )
            idx_result = run_command(
                idx_cmd,
                progress_label=f"index:{entry['name']}",
                interval=args.progress_interval,
                env=base_env,
            )
            (repo_log_dir / "index_stdout.json").write_text(
                idx_result["stdout"], encoding="utf-8"
            )
            (repo_log_dir / "index_stderr.log").write_text(
                idx_result["stderr"], encoding="utf-8"
            )
            parsed_index = {}
            status_ok = False
            if idx_result["returncode"] == 0 and idx_result["stdout"].strip():
                try:
                    parsed_index = json.loads(idx_result["stdout"])
                    status_ok = parsed_index.get("status") == "ok"
                except json.JSONDecodeError:
                    status_ok = False
            repo_record["index"] = {
                "time_ms": idx_result["time_ms"],
                "max_rss_kb": idx_result["max_rss_kb"],
                "returncode": idx_result["returncode"],
                "status": parsed_index.get("status"),
            }
            if idx_result["returncode"] != 0 or not status_ok:
                report["repos"].append(repo_record)
                continue

        for query in entry.get("queries", []):
            cmd = [str(args.cli)]
            if args.profile:
                cmd.extend(["--profile", args.profile])
            cmd.extend(
                [
                    "command",
                    "--json",
                    json.dumps(
                        {
                            "action": "search",
                            "payload": {
                                "query": query["query"],
                                "limit": args.limit,
                                "project": str(repo_path),
                                "trace": args.trace or None,
                            },
                        }
                    ),
                ]
            )
            search_result = run_command(cmd, env=base_env)
            log_prefix = query["query"].replace(" ", "_")[:50]
            (repo_log_dir / f"query_{log_prefix}_stdout.json").write_text(
                search_result["stdout"], encoding="utf-8"
            )
            (repo_log_dir / f"query_{log_prefix}_stderr.log").write_text(
                search_result["stderr"], encoding="utf-8"
            )

            parsed: Dict[str, Any] = {}
            data: Dict[str, Any] = {}
            if search_result["returncode"] == 0 and search_result["stdout"].strip():
                try:
                    parsed = json.loads(search_result["stdout"])
                    if parsed.get("status") == "ok":
                        data = parsed.get("data", {})
                except json.JSONDecodeError:
                    parsed = {}
            precision_info = compute_precision(
                data.get("results", []),
                query["relevant_files"],
                repo_path,
                args.k,
            )

            repo_record["queries"].append(
                {
                    "query": query["query"],
                    "type": query.get("type"),
                    "difficulty": query.get("difficulty"),
                    "expected_snippet": query.get("expected_snippet"),
                    "metrics": {
                        "time_ms": search_result["time_ms"],
                        "max_rss_kb": search_result["max_rss_kb"],
                        "precision_at_k": precision_info["precision_at_k"],
                        "hits": precision_info["hits"],
                        "top_files": precision_info["top_files"],
                        "returncode": search_result["returncode"],
                    },
                }
            )

        for negative in entry.get("negative_examples", []):
            cmd = [str(args.cli)]
            if args.profile:
                cmd.extend(["--profile", args.profile])
            cmd.extend(
                [
                    "command",
                    "--json",
                    json.dumps(
                        {
                            "action": "search",
                            "payload": {
                                "query": negative["query"],
                                "limit": args.limit,
                                "project": str(repo_path),
                                "trace": args.trace or None,
                            },
                        }
                    ),
                ]
            )
            neg_result = run_command(cmd, env=base_env)
            log_prefix = negative["query"].replace(" ", "_")[:50]
            (repo_log_dir / f"negative_{log_prefix}_stdout.json").write_text(
                neg_result["stdout"], encoding="utf-8"
            )
            (repo_log_dir / f"negative_{log_prefix}_stderr.log").write_text(
                neg_result["stderr"], encoding="utf-8"
            )

            parsed: Dict[str, Any] = {}
            data: Dict[str, Any] = {}
            if neg_result["returncode"] == 0 and neg_result["stdout"].strip():
                try:
                    parsed = json.loads(neg_result["stdout"])
                    if parsed.get("status") == "ok":
                        data = parsed.get("data", {})
                except json.JSONDecodeError:
                    parsed = {}
            neg_stats = evaluate_negative(data.get("results", []), repo_path, args.k)

            repo_record["negative_examples"].append(
                {
                    "query": negative["query"],
                    "reason": negative.get("reason"),
                    "metrics": {
                        "time_ms": neg_result["time_ms"],
                        "max_rss_kb": neg_result["max_rss_kb"],
                        "false_positive": neg_stats["false_positive"],
                        "top_files": neg_stats["top_files"],
                        "returncode": neg_result["returncode"],
                    },
                }
            )

        repo_record["summary"] = summarize_repo(repo_record)
        if repo_record["summary"].get("alert"):
            repo_record["alert"] = repo_record["summary"]["alert"]

        report.setdefault("repos", []).append(repo_record)
        if isinstance(repo_name, str):
            processed_names.add(repo_name)

    report["summary"] = build_summary(report)
    results_path.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(f"Benchmark report saved to {results_path}")


def summarize_repo(repo_record: Dict[str, Any]) -> Dict[str, Any]:
    queries = repo_record.get("queries", [])
    precision_values = [
        q.get("metrics", {}).get("precision_at_k", 0.0)
        for q in queries
        if isinstance(q, dict)
    ]
    avg_precision = (
        sum(precision_values) / len(precision_values) if precision_values else 0.0
    )

    neg_examples = repo_record.get("negative_examples", [])
    false_positive = sum(
        1 for n in neg_examples if n.get("metrics", {}).get("false_positive")
    )

    alerts = []
    if avg_precision < 0.9 and queries:
        alerts.append("precision<0.9")
    if false_positive > 0:
        alerts.append("false positives")

    return {
        "avg_precision_at_k": avg_precision,
        "query_count": len(queries),
        "negative_fp": false_positive,
        "alert": "; ".join(alerts),
    }


def build_summary(report: Dict[str, Any]) -> Dict[str, Any]:
    repos = report.get("repos", [])
    if not repos:
        return {
            "repo_count": 0,
            "avg_precision_at_k": 0.0,
            "total_negative_fp": 0,
            "alerts": [],
        }

    summaries = [r.get("summary", {}) for r in repos if isinstance(r, dict)]
    precision_vals = [s.get("avg_precision_at_k", 0.0) for s in summaries if s]
    avg_precision = sum(precision_vals) / len(precision_vals) if precision_vals else 0.0
    total_fp = sum(s.get("negative_fp", 0) for s in summaries)
    alerts = [r.get("name") for r in repos if r.get("summary", {}).get("alert")]

    return {
        "repo_count": len(repos),
        "avg_precision_at_k": avg_precision,
        "total_negative_fp": total_fp,
        "alerts": alerts,
    }


if __name__ == "__main__":
    main()
