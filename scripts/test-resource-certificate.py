#!/usr/bin/env python3
"""Fixed negative controls for the resource certificate, no Rust workload."""
import importlib.util
from pathlib import Path
import unittest
import sys
sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("certificate", Path(__file__).with_name("certify-resources.py"))
certificate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(certificate)


def fixture(values):
    return "\n".join(
        f"resource_soak event=batch batch={i} rss_kib=Some({rss}) fds=Some(10) threads=Some(5)"
        for i, rss in enumerate(values, 1)
    ) + "\nresource_soak event=finish rss_kib=Some(100) fds=Some(8) threads=Some(2)"


class CertificateTests(unittest.TestCase):
    def assess(self, text):
        return certificate.assess(text, "resource_soak", 3, 1000, 50)

    def test_warm_plateau_passes(self):
        self.assertEqual(self.assess(fixture([100, 120, 110]))["post_warmup_growth_kib"], 20)

    def test_temporary_growth_is_not_hidden_by_final_recovery(self):
        with self.assertRaisesRegex(ValueError, "growth"):
            self.assess(fixture([100, 200, 100]))

    def test_missing_measurement_fails(self):
        for field in ("rss_kib=Some(100)", "fds=Some(10)", "threads=Some(5)"):
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "unavailable"):
                self.assess(fixture([100, 100, 100]).replace(field, field.split("=")[0] + "=None"))

    def test_missing_batch_or_final_fails(self):
        with self.assertRaisesRegex(ValueError, "batch"):
            self.assess(fixture([100, 100]))
        with self.assertRaisesRegex(ValueError, "final"):
            self.assess(fixture([100, 100, 100]).split("\nresource_soak event=finish")[0])

    def test_peak_limit_is_independent_of_growth(self):
        with self.assertRaisesRegex(ValueError, "sampled RSS"):
            self.assess(fixture([1100, 1100, 1100]))


if __name__ == "__main__":
    unittest.main()
