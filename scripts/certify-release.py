#!/usr/bin/env python3
"""Explicit maintainer checks in isolated git worktrees; never publishes anything."""

import argparse
import contextlib
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import signal
import statistics
import subprocess
import sys
import tempfile
import time

REQUIRED = ("evidence-controls", "ordinary", "conformance", "dependency", "resources", "management",
            "complexity", "msrv", "host", "linux-management", "benchmarks", "performance")
METRICS = {"setup_ns": 0.15, "close_ns": 0.05, "upload_mib_s": 0.20, "download_mib_s": 0.20}
BENCHMARKS = ("connect_direct_loopback_control", "connect_allowed_loopback",
              "attach_close_empty_lease_default_quiet")


def capture(command, cwd):
    return subprocess.check_output(command, cwd=cwd, text=True, timeout=30).strip()


def fingerprint(root):
    digest = hashlib.sha256()
    names = capture(["git", "ls-files", "-z"], root).split("\0")
    for name in sorted(filter(None, names)):
        path = root / name
        digest.update(name.encode() + b"\0")
        digest.update(path.read_bytes())
    return {"commit": capture(["git", "rev-parse", "HEAD"], root),
            "dirty": capture(["git", "status", "--porcelain"], root),
            "sha256": digest.hexdigest()}


@contextlib.contextmanager
def snapshot(repo, ref, parent):
    path = Path(tempfile.mkdtemp(prefix="sandbox-egress-cert-", dir=parent))
    capture(["git", "worktree", "add", "--detach", str(path), ref], repo)
    try:
        yield path
    finally:
        # Preserve unexpected edits for diagnosis; never force-remove a tree.
        if not capture(["git", "status", "--porcelain"], path):
            capture(["git", "worktree", "remove", str(path)], repo)


def run(command, cwd, log, timeout=900, environment=None):
    started = time.monotonic()
    with log.open("w") as stream:
        with subprocess.Popen(command, cwd=cwd, env=environment, stdout=stream,
                              stderr=subprocess.STDOUT, start_new_session=True) as process:
            try:
                process.wait(timeout=timeout)
            except (subprocess.TimeoutExpired, KeyboardInterrupt):
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise
    if process.returncode:
        raise ValueError(f"exit {process.returncode}: {command}; see {log}")
    return {"command": command, "seconds": time.monotonic() - started,
            "log": str(log), "log_sha256": hashlib.sha256(log.read_bytes()).hexdigest()}


def judge_performance(baseline, candidate):
    """Keep independent budgets; missing, nonfinite, or noisy evidence fails."""
    if len(baseline) < 3 or len(baseline) != len(candidate):
        raise ValueError("performance needs at least three complete alternating pairs")
    result = {}
    for name, tolerance in METRICS.items():
        old = [row[name] for row in baseline]
        new = [row[name] for row in candidate]
        if any(not math.isfinite(x) or x <= 0 for x in old + new):
            raise ValueError(f"invalid performance observation: {name}")
        # A noisy run cannot buy a larger regression budget.
        for label, values in [("baseline", old), ("candidate", new)]:
            if max(values) / min(values) - 1 > tolerance:
                raise ValueError(f"{name}: noisy {label}; investigate on a quiet worker")
        old_median, new_median = statistics.median(old), statistics.median(new)
        regression = (new_median / old_median - 1 if name.endswith("_ns")
                      else 1 - new_median / old_median)
        result[name] = {"baseline_median": old_median, "candidate_median": new_median,
                        "regression": regression, "tolerance": tolerance}
        if regression > tolerance:
            raise ValueError(f"{name}: regression {regression:.1%} exceeds {tolerance:.1%}")
    return result


def benchmark_contract(root):
    digest = hashlib.sha256()
    for name in ["benches/connections.rs", "benches/lifecycle.rs", "benches/support/mod.rs",
                 "tests/throughput.rs", "scripts/measure-throughput.sh", "Cargo.toml", "Cargo.lock",
                 "rust-toolchain.toml", ".cargo/config.toml"]:
        digest.update(name.encode() + b"\0" + (root / name).read_bytes())
    return digest.hexdigest()


def measure(root, output, label):
    run(["cargo", "bench", "--locked", "--bench", "connections", "--bench", "lifecycle",
         "--", "|".join(BENCHMARKS), "--noplot", "--sample-size", "30",
         "--warm-up-time", "1", "--measurement-time", "2"], root, output / f"{label}-setup.log")
    values = []
    for name in BENCHMARKS:
        path = root / "target/criterion" / name / "new/estimates.json"
        data = json.loads(path.read_text())
        values.append(data["median"]["point_estimate"])
        (output / f"{label}-{name}.json").write_text(json.dumps(data, indent=2) + "\n")
    log = output / f"{label}-throughput.log"
    run(["./scripts/measure-throughput.sh", "128", "8", "both"], root, log)
    rates = re.findall(r"throughput direction=(Upload|Download).*?mebibytes_per_second=([\d.]+)", log.read_text())
    if [direction for direction, _ in rates] != ["Upload", "Download"]:
        raise ValueError("missing or repeated throughput observations")
    return {"setup_ns": values[1] - values[0], "direct_ns": values[0],
            "connect_ns": values[1], "close_ns": values[2],
            "upload_mib_s": float(rates[0][1]), "download_mib_s": float(rates[1][1])}


def certify_verdict(report):
    if report["source_before"] != report["source_after"] or report["source_after"]["dirty"]:
        raise ValueError("candidate source changed during certification")
    for name in REQUIRED:
        if report["checks"].get(name, {}).get("status") != "passed":
            raise ValueError(f"required release check not passed: {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, help="reviewed git revision with the same benchmark contract")
    parser.add_argument("--output", type=Path, required=True, help="new evidence directory outside the snapshots")
    parser.add_argument("--worktree-parent", type=Path, default=Path.home() / "code")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    report = {"schema": 1, "passed": False, "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "platform": platform.platform(), "checks": {name: {"status": "not_run"} for name in REQUIRED}}
    manifest = output / "release.json"

    def save():
        manifest.write_text(json.dumps(report, indent=2) + "\n")

    def lane(name, operation):
        report["checks"][name] = {"status": "running"}
        save()
        print(f"release check: {name}", flush=True)
        try:
            result = operation()
            report["checks"][name] = {"status": "passed", "evidence": result}
        except (ValueError, OSError, subprocess.SubprocessError, KeyError) as error:
            report["checks"][name] = {"status": "failed", "error": str(error)}
            print(f"release check failed: {name}: {error}", flush=True)
        finally:
            save()

    try:
        if capture(["git", "status", "--porcelain"], repo):
            raise ValueError("release certification requires a committed, clean source tree")
        if os.environ.get("CARGO_TARGET_DIR") or os.environ.get("RUSTUP_TOOLCHAIN") or os.environ.get("RUSTFLAGS"):
            raise ValueError("unset build overrides for reproducible isolated certification")
        head = capture(["git", "rev-parse", "HEAD"], repo)
        baseline = capture(["git", "rev-parse", "--verify", args.baseline + "^{commit}"], repo)
        args.worktree_parent.mkdir(parents=True, exist_ok=True)
        with snapshot(repo, head, args.worktree_parent) as candidate, snapshot(repo, baseline, args.worktree_parent) as control:
            report["source_before"] = fingerprint(candidate)
            report["baseline_source"] = fingerprint(control)
            report["rustc"] = capture(["rustc", "-vV"], candidate)
            report["cargo_deny"] = capture(["cargo", "deny", "--version"], candidate)
            report["docker"] = capture(["docker", "version", "--format", "{{.Server.Version}}"], candidate)
            contract = benchmark_contract(candidate)
            if contract != benchmark_contract(control):
                raise ValueError("baseline benchmark/dependency contract differs; establish a comparable baseline first")
            report["benchmark_contract"] = contract
            for name, command in [("evidence-controls", ["python3", "scripts/test-release-certificate.py"]),
                                  ("ordinary", ["./scripts/check.sh"]),
                                  ("conformance", ["./scripts/test-conformance.sh"]),
                                  ("management", ["cargo", "test", "--locked", "--release", "--test", "management_load", "--", "--ignored", "--nocapture"]),
                                  ("complexity", ["./scripts/measure-complexity.sh"])]:
                lane(name, lambda name=name, command=command: run(command, candidate, output / f"{name}.log"))

            def dependency():
                # Fresh isolated advisory data, not whichever cache happened to exist.
                config = output / "deny.toml"
                text = (candidate / "deny.toml").read_text().replace("[advisories]", "[advisories]\ndb-path = " + json.dumps(str(output / "advisories")))
                config.write_text(text)
                command = ["cargo", "deny", "--locked", "--config", str(config)]
                fetched = run(command + ["fetch", "db", "index"], candidate, output / "dependency-fetch.log")
                checked = run(command + ["--offline", "check"], candidate, output / "dependency.log")
                heads = {}
                for git_dir in (output / "advisories").rglob(".git"):
                    heads[str(git_dir.parent.relative_to(output))] = capture(["git", "rev-parse", "HEAD"], git_dir.parent)
                if not heads:
                    raise ValueError("advisory database revision unavailable")
                return {"fetch": fetched, "check": checked, "advisory_revisions": heads}
            lane("dependency", dependency)

            def resources():
                result = run(["python3", "scripts/certify-resources.py", "--require-clean", "--output", str(output / "resources.json")], candidate, output / "resources.log", 1800)
                certificate = json.loads((output / "resources.json").read_text())
                if not certificate["passed"] or certificate["commit"] != head:
                    raise ValueError("resource certificate does not certify this candidate")
                return result
            lane("resources", resources)

            def run_image(name, image, privileged=False, command=None):
                container_name = f"sandbox-egress-cert-{os.getpid()}-{name}"
                invocation = ["docker", "run", "--name", container_name, "--rm", "--network=none"]
                if privileged:
                    invocation.append("--privileged")
                try:
                    checked = run(invocation + [image] + (command or []), candidate, output / f"{name}.log", 180)
                finally:
                    subprocess.run(["docker", "rm", "-f", container_name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False, timeout=30)
                return checked

            def container(name, dockerfile, privileged=False, command=None):
                image_file = output / f"{name}-image.txt"
                build = run(["docker", "build", "--iidfile", str(image_file), "-f", dockerfile, "."], candidate, output / f"{name}-build.log", 1800)
                image = image_file.read_text().strip()
                checked = run_image(name, image, privileged, command)
                return {"build": build, "image": image, "run": checked}
            lane("msrv", lambda: container("msrv", "Dockerfile"))
            lane("host", lambda: container("host", "Dockerfile.host-boundary", True))
            if report["checks"]["host"]["status"] == "passed":
                host_image = report["checks"]["host"]["evidence"]["image"]
                lane("linux-management", lambda: run_image("linux-management", host_image,
                     command=["/usr/local/bin/management_load", "--ignored", "--nocapture"]))
            lane("benchmarks", lambda: run(["./scripts/bench.sh"], candidate, output / "benchmarks.log", 1800))

            def performance():
                if capture(["rustc", "-vV"], control) != report["rustc"]:
                    raise ValueError("baseline and candidate toolchains differ")
                observations = {"baseline": [], "candidate": []}
                for repeat in range(3):
                    order = [("baseline", control), ("candidate", candidate)]
                    if repeat % 2:
                        order.reverse()
                    for label, tree in order:
                        observations[label].append(measure(tree, output, f"{label}-{repeat}"))
                        (output / "performance-samples.json").write_text(json.dumps(observations, indent=2) + "\n")
                comparison = judge_performance(**observations)
                if fingerprint(control) != report["baseline_source"]:
                    raise ValueError("baseline source changed")
                return {"samples": observations, "comparison": comparison}
            lane("performance", performance)
            report["source_after"] = fingerprint(candidate)
            certify_verdict(report)
            report["passed"] = True
    except (ValueError, OSError, subprocess.SubprocessError, KeyError) as error:
        report["error"] = str(error)
    finally:
        save()
    print(f"release checks passed={report['passed']}: {manifest}")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
