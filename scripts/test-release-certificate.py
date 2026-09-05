#!/usr/bin/env python3
"""Fixed negative controls for the release verdict and performance evaluator."""
import copy
import importlib.util
from pathlib import Path
import tempfile
import subprocess
import sys
import unittest

spec = importlib.util.spec_from_file_location("certificate", Path(__file__).with_name("certify-release.py"))
certificate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(certificate)


class ReleaseTests(unittest.TestCase):
    def test_runner_rejects_failure_and_timeout_but_keeps_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            failed = root / "failed.log"
            with self.assertRaises(ValueError):
                certificate.run([sys.executable, "-c", "print('fixed failure'); exit(1)"], root, failed)
            self.assertIn("fixed failure", failed.read_text())
            timed_out = root / "timeout.log"
            with self.assertRaises(subprocess.TimeoutExpired):
                certificate.run([sys.executable, "-c", "import time; time.sleep(10)"], root, timed_out, timeout=0.05)
            self.assertTrue(timed_out.exists())

    def test_every_required_lane_must_pass(self):
        source = {"commit": "candidate", "dirty": "", "sha256": "source"}
        report = {"source_before": source, "source_after": source,
                  "checks": {name: {"status": "passed"} for name in certificate.REQUIRED}}
        certificate.certify_verdict(report)
        for name in certificate.REQUIRED:
            for status in ("failed", "not_run", "running"):
                changed = copy.deepcopy(report)
                changed["checks"][name]["status"] = status
                with self.subTest(name=name, status=status), self.assertRaises(ValueError):
                    certificate.certify_verdict(changed)
            changed = copy.deepcopy(report)
            del changed["checks"][name]
            with self.assertRaises(ValueError):
                certificate.certify_verdict(changed)

    def test_source_changes_cannot_certify(self):
        for after in ({"commit": "different", "dirty": ""}, {"commit": "same", "dirty": " M src/proxy.rs"}):
            with self.assertRaises(ValueError):
                certificate.certify_verdict({"source_before": {"commit": "same", "dirty": ""}, "source_after": after})

    def test_equal_performance_passes_and_each_regression_fails(self):
        samples = [dict.fromkeys(certificate.METRICS, 100.0) for _ in range(3)]
        certificate.judge_performance(samples, samples)
        for name in certificate.METRICS:
            candidate = copy.deepcopy(samples)
            for row in candidate:
                row[name] = 200.0 if name.endswith("_ns") else 50.0
            with self.subTest(metric=name), self.assertRaises(ValueError):
                certificate.judge_performance(samples, candidate)

    def test_missing_nonfinite_or_noisy_performance_fails(self):
        samples = [dict.fromkeys(certificate.METRICS, 100.0) for _ in range(3)]
        with self.assertRaises(ValueError):
            certificate.judge_performance(samples[:2], samples)
        for value in (float("nan"), float("inf"), 0.0, 1000.0):
            changed = copy.deepcopy(samples)
            changed[1]["setup_ns"] = value
            with self.subTest(value=value), self.assertRaises(ValueError):
                certificate.judge_performance(samples, changed)
        changed = copy.deepcopy(samples)
        del changed[0]["setup_ns"]
        with self.assertRaises(KeyError):
            certificate.judge_performance(samples, changed)

    def test_benchmark_contract_detects_changed_work(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ("benches/connections.rs", "benches/lifecycle.rs", "benches/support/mod.rs",
                         "tests/throughput.rs", "scripts/measure-throughput.sh", "Cargo.toml", "Cargo.lock",
                         "rust-toolchain.toml", ".cargo/config.toml"):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(name)
            before = certificate.benchmark_contract(root)
            (root / "benches/connections.rs").write_text("measure denied work instead")
            self.assertNotEqual(before, certificate.benchmark_contract(root))


if __name__ == "__main__":
    unittest.main()
