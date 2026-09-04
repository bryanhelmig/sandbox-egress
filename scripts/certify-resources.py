#!/usr/bin/env python3
"""Opt-in, bounded resource certificate using the existing Rust soak lanes."""

import argparse
import datetime
import json
import hashlib
import os
from pathlib import Path
import platform
import re
import signal
import sys
import subprocess
import tempfile


LANES = {
    "identity_churn_has_bounded_process_resources": "resource_soak",
    "failed_startup_releases_process_resources": "failed_start_soak",
    "concurrent_management_churn_releases_process_resources": "control_soak",
    "concurrent_idle_expiry_releases_process_resources": "idle_soak",
    "concurrent_partial_client_hellos_release_process_resources": "tls_soak",
    "concurrent_partial_headers_release_process_resources": "header_soak",
    "concurrent_partial_upstream_responses_release_process_resources": "upstream_soak",
    "repeated_bidirectional_backpressure_releases_process_resources": "backpressure_soak",
    "terminal_connection_churn_releases_process_resources": "connection_soak",
}
BATCHED = {"resource_soak", "failed_start_soak", "control_soak", "backpressure_soak", "connection_soak"}


def assess(log, prefix, batches, max_rss, max_growth):
    """Require actual samples; a missing measurement never becomes a zero."""
    samples = []
    for line in log.splitlines():
        if not line.startswith(prefix + " event="):
            continue
        event = re.search(r"\bevent=(\w+)", line).group(1)
        if event == "start":
            continue  # Start lines use differently named baseline fields.
        sample = {"event": event}
        for field in ("rss_kib", "fds", "threads"):
            found = re.search(r"\b" + field + r"=Some\((\d+)\)", line)
            if not found:
                raise ValueError(f"{prefix}: required {field} unavailable at {event}")
            sample[field] = int(found.group(1))
        if event == "batch":
            found = re.search(r"\bbatch=(\d+)", line)
            if not found:
                raise ValueError(f"{prefix}: missing batch number")
            sample["batch"] = int(found.group(1))
        samples.append(sample)
    if len([s for s in samples if s["event"] == "finish"]) != 1:
        raise ValueError(f"{prefix}: missing or repeated final sample")
    peak = max(s["rss_kib"] for s in samples)
    if peak > max_rss:
        raise ValueError(f"{prefix}: sampled RSS {peak} KiB exceeds {max_rss}")
    growth = None
    if prefix in BATCHED:
        points = [s for s in samples if s["event"] == "batch"]
        if [s["batch"] for s in points] != list(range(1, batches + 1)):
            raise ValueError(f"{prefix}: incomplete or reordered batch evidence")
        # The first completed batch warms the allocator. Preserve all later
        # high-water observations instead of looking only at final recovery.
        growth = max(0, max(s["rss_kib"] for s in points[1:]) - points[0]["rss_kib"])
        if growth > max_growth:
            raise ValueError(f"{prefix}: post-warmup RSS growth {growth} KiB exceeds {max_growth}")
    elif {s["event"] for s in samples} != {"peak", "recovered", "finish"}:
        raise ValueError(f"{prefix}: missing active or recovered sample")
    return {"sampled_peak_rss_kib": peak, "post_warmup_growth_kib": growth, "samples": samples}


def capture(command, cwd):
    return subprocess.check_output(command, cwd=cwd, text=True, timeout=15).strip()


def source_state(root):
    """Fingerprint every tracked or unignored source file."""
    digest = hashlib.sha256()
    paths = capture(["git", "ls-files", "-c", "-o", "--exclude-standard", "-z"], root).split("\0")
    for name in sorted(set(paths) - {""}):
        path = root / name
        if path.is_file():
            digest.update(name.encode() + b"\0" + hashlib.sha256(path.read_bytes()).digest())
    return {
        "commit": capture(["git", "rev-parse", "HEAD"], root),
        "working_tree": capture(["git", "status", "--porcelain=v1"], root),
        "source_tree_sha256": digest.hexdigest(),
    }


def require_stable_source(before, after, require_clean):
    """Reject release dirt or any source change while the lanes execute."""
    if require_clean and before["working_tree"]:
        raise ValueError("resource certification requires a clean working tree")
    if before != after:
        raise ValueError("source tree changed while resource certification was running")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path("target/resource-certificate.json"))
    parser.add_argument("--runs-per-batch", type=int, default=250)
    parser.add_argument("--batches", type=int, default=4)
    parser.add_argument("--connections", type=int, default=64)
    parser.add_argument("--max-rss-kib", type=int, default=131072)
    parser.add_argument("--max-growth-kib", type=int, default=8192)
    parser.add_argument("--lane-timeout-seconds", type=int, default=180)
    parser.add_argument("--require-clean", action="store_true")
    args = parser.parse_args()
    if args.batches < 3 or min(args.runs_per_batch, args.connections, args.max_rss_kib,
                              args.lane_timeout_seconds) <= 0 or args.max_growth_kib < 0:
        parser.error("require at least three batches, positive workload/peak/time bounds, and nonnegative growth")
    root = Path(__file__).resolve().parent.parent
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    logs = Path(tempfile.mkdtemp(prefix="resource-logs-", dir=output.parent))
    config = {
        "SANDBOX_EGRESS_REQUIRE_RESOURCE_METRICS": 1,
        "SANDBOX_EGRESS_SOAK_RUNS": args.runs_per_batch,
        "SANDBOX_EGRESS_SOAK_BATCHES": args.batches,
        "SANDBOX_EGRESS_FAILED_START_RUNS": args.runs_per_batch,
        "SANDBOX_EGRESS_FAILED_START_BATCHES": args.batches,
        "SANDBOX_EGRESS_CONTROL_CONCURRENCY": 16,
        "SANDBOX_EGRESS_CONTROL_BATCHES": args.batches,
        "SANDBOX_EGRESS_IDLE_CONNECTIONS": args.connections,
        "SANDBOX_EGRESS_TLS_CONNECTIONS": args.connections,
        "SANDBOX_EGRESS_HEADER_CONNECTIONS": args.connections,
        "SANDBOX_EGRESS_UPSTREAM_CONNECTIONS": args.connections,
        "SANDBOX_EGRESS_BACKPRESSURE_RUNS": 8,
        "SANDBOX_EGRESS_BACKPRESSURE_BATCHES": args.batches,
        "SANDBOX_EGRESS_TERMINAL_RUNS": args.runs_per_batch,
        "SANDBOX_EGRESS_TERMINAL_BATCHES": args.batches,
    }
    environment = dict(os.environ, **{key: str(value) for key, value in config.items()})
    report = {"schema": 2, "passed": False, "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "platform": platform.platform(), "workload": config,
              "max_rss_kib": args.max_rss_kib, "max_growth_kib": args.max_growth_kib,
              "lane_timeout_seconds": args.lane_timeout_seconds,
              "require_clean": args.require_clean, "lanes": {}}
    try:
        if platform.system() not in ("Linux", "Darwin"):
            raise ValueError("resource certification requires Linux or macOS measurements")
        subprocess.run([sys.executable, str(root / "scripts/test-resource-certificate.py")],
                       check=True, timeout=15)
        before = source_state(root)
        report.update(before)
        require_stable_source(before, before, args.require_clean)
        report["rustc"] = capture(["rustc", "--version"], root)
        for name, prefix in LANES.items():
            command = ["cargo", "test", "--locked", "--release", "--test", "resource_soak", name,
                       "--", "--ignored", "--nocapture", "--exact"]
            path = logs / f"{prefix}.log"
            result = {"command": command, "log": str(path), "passed": False}
            report["lanes"][name] = result
            print(f"certifying {name}", flush=True)
            with path.open("w") as stream:
                with subprocess.Popen(command, cwd=root, env=environment, stdout=stream,
                                      stderr=subprocess.STDOUT, start_new_session=True) as process:
                    try:
                        process.wait(timeout=args.lane_timeout_seconds)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
                        raise
            result["exit_code"] = process.returncode
            if process.returncode:
                raise ValueError(f"{name}: Rust lane failed; see {path}")
            result.update(assess(path.read_text(), prefix, args.batches, args.max_rss_kib, args.max_growth_kib))
            result["passed"] = True
        after = source_state(root)
        report["finished_source"] = after
        require_stable_source(before, after, args.require_clean)
        report["passed"] = True
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        report["error"] = str(error)
    finally:
        output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"resource certificate passed={report['passed']}: {output}")
    if not report["passed"]:
        print(report.get("error", "incomplete certificate"))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
