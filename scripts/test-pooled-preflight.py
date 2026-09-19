#!/usr/bin/env python3
"""Portable regressions for the privileged pooled lane's capability probe."""
import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
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


if __name__ == "__main__":
    unittest.main()
