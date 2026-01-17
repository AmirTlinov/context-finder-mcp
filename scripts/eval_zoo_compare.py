#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
from dataclasses import dataclass
from typing import Any, Dict, List, Optional, Tuple


def percentile(sorted_values: List[int], pct: int) -> int:
    if not sorted_values:
        return 0
    idx = (len(sorted_values) - 1) * pct // 100
    return sorted_values[idx]


@dataclass(frozen=True)
class ZooSummary:
    tool_calls: int
    ok_calls: int
    strict_bad_calls: int
    latency_p50_ms: int
    latency_p95_ms: int
    latency_max_ms: int
    noise_defined_calls: int
    noise_mean: Optional[float]
    token_saved_defined_calls: int
    token_saved_mean: Optional[float]
    token_saved_negative_calls: int
    worktree_pack_calls: int
    worktrees_returned_total: int
    worktrees_with_digest_total: int
    worktrees_dirty_total: int
    worktrees_with_touches_total: int
    worktrees_purpose_truncated_total: int


def parse_report(path: str) -> Dict[str, Any]:
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def summarize(report: Dict[str, Any]) -> ZooSummary:
    repos: List[Dict[str, Any]] = list(report.get("repos", []))

    tool_calls = 0
    ok_calls = 0
    strict_bad_calls = 0
    latencies: List[int] = []

    noise_values: List[float] = []
    token_saved_values: List[float] = []
    token_saved_negative_calls = 0

    worktree_pack_calls = 0
    worktrees_returned_total = 0
    worktrees_with_digest_total = 0
    worktrees_dirty_total = 0
    worktrees_with_touches_total = 0
    worktrees_purpose_truncated_total = 0

    for repo in repos:
        tools: Dict[str, Dict[str, Any]] = dict(repo.get("tools", {}))
        for tool_name, m in tools.items():
            tool_calls += 1
            ok = bool(m.get("ok", False))
            strict_ok = bool(m.get("strict_ok", False))
            if ok:
                ok_calls += 1
                if not strict_ok:
                    strict_bad_calls += 1

            latency = int(m.get("latency_ms", 0) or 0)
            latencies.append(latency)

            noise = m.get("noise_ratio", None)
            if noise is not None:
                try:
                    noise_values.append(float(noise))
                except Exception:
                    pass

            saved = m.get("token_saved", None)
            if saved is not None:
                try:
                    v = float(saved)
                    token_saved_values.append(v)
                    if v < 0.0:
                        token_saved_negative_calls += 1
                except Exception:
                    pass

            if tool_name == "worktree_pack" and ok:
                worktree_pack_calls += 1
                worktrees_returned_total += int(m.get("worktrees_returned", 0) or 0)
                worktrees_with_digest_total += int(m.get("worktrees_with_digest", 0) or 0)
                worktrees_dirty_total += int(m.get("worktrees_dirty", 0) or 0)
                worktrees_with_touches_total += int(m.get("worktrees_with_touches", 0) or 0)
                worktrees_purpose_truncated_total += int(
                    m.get("worktrees_purpose_truncated", 0) or 0
                )

    latencies.sort()
    latency_p50_ms = percentile(latencies, 50)
    latency_p95_ms = percentile(latencies, 95)
    latency_max_ms = latencies[-1] if latencies else 0

    noise_defined_calls = len(noise_values)
    noise_mean = (
        (sum(noise_values) / noise_defined_calls) if noise_defined_calls > 0 else None
    )

    token_saved_defined_calls = len(token_saved_values)
    token_saved_mean = (
        (sum(token_saved_values) / token_saved_defined_calls)
        if token_saved_defined_calls > 0
        else None
    )

    return ZooSummary(
        tool_calls=tool_calls,
        ok_calls=ok_calls,
        strict_bad_calls=strict_bad_calls,
        latency_p50_ms=latency_p50_ms,
        latency_p95_ms=latency_p95_ms,
        latency_max_ms=latency_max_ms,
        noise_defined_calls=noise_defined_calls,
        noise_mean=noise_mean,
        token_saved_defined_calls=token_saved_defined_calls,
        token_saved_mean=token_saved_mean,
        token_saved_negative_calls=token_saved_negative_calls,
        worktree_pack_calls=worktree_pack_calls,
        worktrees_returned_total=worktrees_returned_total,
        worktrees_with_digest_total=worktrees_with_digest_total,
        worktrees_dirty_total=worktrees_dirty_total,
        worktrees_with_touches_total=worktrees_with_touches_total,
        worktrees_purpose_truncated_total=worktrees_purpose_truncated_total,
    )


def fmt_opt_float(v: Optional[float]) -> str:
    return "-" if v is None else f"{v:.4f}"


def print_summary(label: str, s: ZooSummary) -> None:
    print(f"{label}:")
    print(f"  tool_calls={s.tool_calls} ok_calls={s.ok_calls} strict_bad_calls={s.strict_bad_calls}")
    print(
        f"  latency_ms p50={s.latency_p50_ms} p95={s.latency_p95_ms} max={s.latency_max_ms}"
    )
    print(f"  noise_ratio calls={s.noise_defined_calls} mean={fmt_opt_float(s.noise_mean)}")
    print(
        f"  token_saved calls={s.token_saved_defined_calls} mean={fmt_opt_float(s.token_saved_mean)} negative={s.token_saved_negative_calls}"
    )
    print(
        f"  worktree_pack calls={s.worktree_pack_calls} worktrees={s.worktrees_returned_total} digest={s.worktrees_with_digest_total} dirty={s.worktrees_dirty_total} touches={s.worktrees_with_touches_total} purpose_truncated={s.worktrees_purpose_truncated_total}"
    )


def delta_float(a: Optional[float], b: Optional[float]) -> str:
    if a is None or b is None:
        return "-"
    return f"{(a - b):+.4f}"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--current", required=True, help="Current zoo_report.json")
    ap.add_argument("--previous", required=True, help="Previous zoo_report.json")
    args = ap.parse_args()

    cur = summarize(parse_report(args.current))
    prev = summarize(parse_report(args.previous))

    print_summary("current", cur)
    print_summary("previous", prev)

    print("delta (current - previous):")
    print(f"  tool_calls: {cur.tool_calls - prev.tool_calls:+d}")
    print(f"  ok_calls: {cur.ok_calls - prev.ok_calls:+d}")
    print(f"  strict_bad_calls: {cur.strict_bad_calls - prev.strict_bad_calls:+d}")
    print(
        f"  latency_ms p50={cur.latency_p50_ms - prev.latency_p50_ms:+d} p95={cur.latency_p95_ms - prev.latency_p95_ms:+d} max={cur.latency_max_ms - prev.latency_max_ms:+d}"
    )
    print(f"  noise_mean: {delta_float(cur.noise_mean, prev.noise_mean)}")
    print(f"  token_saved_mean: {delta_float(cur.token_saved_mean, prev.token_saved_mean)}")
    print(
        f"  worktrees_returned_total: {cur.worktrees_returned_total - prev.worktrees_returned_total:+d}"
    )
    print(
        f"  worktrees_with_digest_total: {cur.worktrees_with_digest_total - prev.worktrees_with_digest_total:+d}"
    )
    print(f"  worktrees_dirty_total: {cur.worktrees_dirty_total - prev.worktrees_dirty_total:+d}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

