#!/usr/bin/env python3
"""Portable regressions for the privileged pooled lane's capability probe."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "pooled", Path(__file__).with_name("test-linux-pooled-boundary.py"))
pooled = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pooled)


class PreflightTests(unittest.TestCase):
    def invoke(self, arguments, outputs):
        """Keep real loopback sockets; substitute only kernel enumeration/destruction."""
        stack = contextlib.ExitStack()
        self.addCleanup(stack.close)
        stack.enter_context(mock.patch.object(pooled.sys, "argv", ["pooled", *arguments]))
        stack.enter_context(mock.patch.object(pooled.sys, "platform", "linux"))
        stack.enter_context(mock.patch.object(pooled.os, "geteuid", return_value=0))
        stack.enter_context(mock.patch.object(pooled.shutil, "which", return_value="tool"))
        commands = stack.enter_context(mock.patch.object(pooled, "run", side_effect=outputs))
        scenario = stack.enter_context(mock.patch.object(
            pooled, "scenario", side_effect=AssertionError("scenario ran before capability check")))
        errors = stack.enter_context(contextlib.redirect_stderr(io.StringIO()))
        stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
        return commands, scenario, errors

    def test_silent_destroy_noop_exits_78_before_any_scenario(self):
        for arguments in ([], ["--omit", "sockets"], ["--omit", "conntrack"], ["--preflight-only"]):
            with self.subTest(arguments=arguments):
                _, scenario, errors = self.invoke(arguments, ["ESTAB", "", "ESTAB"])
                with self.assertRaises(SystemExit) as stopped:
                    pooled.main()
                self.assertEqual(stopped.exception.code, 78)
                self.assertEqual(errors.getvalue().strip(),
                                 "kernel lacks CONFIG_INET_DIAG_DESTROY; pooled lane cannot run here")
                scenario.assert_not_called()
                self.doCleanups()

    def test_success_requires_before_and_after_inventory_of_one_exact_tuple(self):
        commands, scenario, _ = self.invoke(["--preflight-only"], ["ESTAB", "", ""])
        pooled.main()
        before, destroy, after = [call.args for call in commands.call_args_list]
        self.assertEqual(before[:2], ("ss", "-Htan"))
        self.assertEqual(destroy[:2], ("ss", "-Ktan"))
        self.assertEqual(before, after)
        self.assertEqual(destroy[2:], before[2:])
        expression = before[2:]
        self.assertEqual(expression[:4], ("src", "127.0.0.1", "sport", "="))
        self.assertEqual(expression[5:9], ("dst", "127.0.0.1", "dport", "="))
        self.assertTrue(0 < int(expression[4][1:]) < 65536)
        self.assertTrue(0 < int(expression[9][1:]) < 65536)
        self.assertNotEqual(expression[4], expression[9])
        scenario.assert_not_called()

    def test_missing_probe_socket_cannot_pass_or_be_called_unsupported(self):
        commands, scenario, errors = self.invoke([], [""])
        with self.assertRaisesRegex(RuntimeError, "could not observe"):
            pooled.main()
        self.assertEqual(commands.call_count, 1)
        self.assertEqual(errors.getvalue(), "")
        scenario.assert_not_called()

    def test_tool_errors_remain_failures_not_unsupported_kernel_results(self):
        for prefix in ([], ["ESTAB"], ["ESTAB", ""]):
            with self.subTest(prefix=prefix):
                _, scenario, errors = self.invoke([], [*prefix, RuntimeError("tool failed")])
                with self.assertRaisesRegex(RuntimeError, "tool failed"):
                    pooled.main()
                self.assertEqual(errors.getvalue(), "")
                scenario.assert_not_called()
                self.doCleanups()

    def test_success_status_with_permission_diagnostic_is_still_an_error(self):
        results = [subprocess.CompletedProcess([], 0, "ESTAB", ""),
                   subprocess.CompletedProcess([], 0, "", "SOCK_DESTROY answers: Operation not permitted"),
                   subprocess.CompletedProcess([], 0, "ESTAB", "")]
        with mock.patch.object(pooled.subprocess, "run", side_effect=results) as commands:
            with contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "Operation not permitted"):
                    pooled.preflight_socket_destroy()
            self.assertEqual(commands.call_count, 2)


class EntrypointTests(unittest.TestCase):
    def run_entrypoint(self, host_exit=0, preflight_exit=0):
        dockerfile = Path(__file__).resolve().parent.parent / "Dockerfile.host-boundary"
        command = json.loads(next(line[4:] for line in dockerfile.read_text().splitlines()
                                  if line.startswith("CMD ")))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "scripts").mkdir()
            (root / "bin").mkdir()
            stubs = {
                "scripts/test-linux-host-boundary.sh":
                    'echo host >> "$PREFLIGHT_TRACE"\n'
                    'if [ "$HOST_EXIT" != 0 ]; then exit "$HOST_EXIT"; fi\n'
                    'echo "host-boundary lane passed"\n',
                "bin/python3":
                    'if [ "$2" = --preflight-only ]; then\n'
                    '  echo preflight >> "$PREFLIGHT_TRACE"\n'
                    '  exit "$PREFLIGHT_EXIT"\n'
                    'fi\n'
                    'echo pooled >> "$PREFLIGHT_TRACE"\n',
                "bin/cut": 'echo fixture-hash\n',
                "bin/timeout": 'shift\nexec "$@"\n',
            }
            for name, body in stubs.items():
                path = root / name
                path.write_text("#!/bin/sh\n" + body)
                path.chmod(0o755)
            environment = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"],
                               PREFLIGHT_TRACE=str(root / "trace"), HOST_EXIT=str(host_exit),
                               PREFLIGHT_EXIT=str(preflight_exit))
            result = subprocess.run(command, cwd=root, env=environment, text=True,
                                    capture_output=True, timeout=5)
            return result, (root / "trace").read_text().splitlines()

    def test_unsupported_pooled_lane_preserves_host_coverage_and_exit_78(self):
        result, trace = self.run_entrypoint(preflight_exit=78)
        self.assertEqual(result.returncode, 78)
        self.assertEqual(trace, ["host", "preflight"])
        self.assertIn("host-boundary lane passed", result.stdout)

    def test_host_failure_stops_before_preflight_and_preserves_its_status(self):
        result, trace = self.run_entrypoint(host_exit=17)
        self.assertEqual(result.returncode, 17)
        self.assertEqual(trace, ["host"])
        self.assertNotIn("host-boundary lane passed", result.stdout)

    def test_supported_kernel_runs_both_lanes_in_order(self):
        result, trace = self.run_entrypoint()
        self.assertEqual(result.returncode, 0)
        self.assertEqual(trace, ["host", "preflight", "pooled"])


if __name__ == "__main__":
    unittest.main()
