#!/usr/bin/env python3
"""统一调度器自测:覆盖注册表完整性与关键结果语义。"""

from __future__ import annotations

import argparse
import importlib
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path


TESTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS_DIR))
runner = importlib.import_module("runner")
registry = importlib.import_module("test_registry")


class RunnerTest(unittest.TestCase):
    """调度器必须保证缺实现、缺依赖和 dry-run 都不伪装为通过。"""

    def test_registry_references_are_complete(self) -> None:
        runner.validate_registry()

    def test_mapped_task_without_command_is_rejected(self) -> None:
        task_id = "__mapped_without_command__"
        registry.TASKS[task_id] = {"note": "错误映射"}
        try:
            with self.assertRaisesRegex(RuntimeError, "缺少执行命令"):
                runner.validate_registry()
        finally:
            del registry.TASKS[task_id]

    def test_required_platform_gap_is_infrastructure_error(self) -> None:
        task_id = "__required_platform_gap__"
        original = registry.TASKS.get(task_id)
        registry.TASKS[task_id] = {
            "command": ["true"],
            "os": ["linux"],
            "required_on": ["linux", "darwin", "windows"],
        }
        try:
            with mock.patch.object(runner.platform, "system", return_value="Darwin"):
                args = argparse.Namespace(
                    dry_run=False, seed=None, report_dir=str(TESTS_DIR),
                    timeout=5, artifact=None,
                )
                result = runner.run_task(task_id, args)
        finally:
            if original is None:
                del registry.TASKS[task_id]
            else:
                registry.TASKS[task_id] = original
        self.assertEqual(result.status, "infra_error")
        self.assertIn("正式支持平台", result.detail)

    def test_task_must_belong_to_a_suite(self) -> None:
        task_id = "__orphan_task__"
        registry.TASKS[task_id] = {"command": ["true"]}
        try:
            with self.assertRaisesRegex(RuntimeError, "未进入任何选集"):
                runner.validate_registry()
        finally:
            del registry.TASKS[task_id]

    def test_dry_run_reports_skip(self) -> None:
        args = argparse.Namespace(dry_run=True, seed=None, report_dir=str(TESTS_DIR))
        result = runner.run_task("shared.harness", args)
        self.assertEqual(result.status, "skip")
        self.assertIn("[dry-run]", result.detail)

    def test_missing_executable_is_infrastructure_error(self) -> None:
        task_id = "__missing_executable__"
        original = registry.TASKS.get(task_id)
        registry.TASKS[task_id] = {"command": ["/definitely/not/a/real/executable"]}
        try:
            with tempfile.TemporaryDirectory() as report_dir:
                args = argparse.Namespace(
                    dry_run=False, seed=None, report_dir=report_dir,
                    timeout=5, artifact=None,
                )
                result = runner.run_task(task_id, args)
        finally:
            if original is None:
                del registry.TASKS[task_id]
            else:
                registry.TASKS[task_id] = original
        self.assertEqual(result.status, "infra_error")

    def test_task_environment_is_injected(self) -> None:
        task_id = "__environment_probe__"
        marker = "runner-env-marker"
        original = registry.TASKS.get(task_id)
        registry.TASKS[task_id] = {
            "command": [sys.executable, "-c", "import os; print(os.environ['RUNNER_PROBE'])"],
            "env": {"RUNNER_PROBE": marker},
        }
        try:
            with tempfile.TemporaryDirectory() as report_dir:
                args = argparse.Namespace(
                    dry_run=False, seed=None, report_dir=report_dir,
                    timeout=5, artifact=None,
                )
                result = runner.run_task(task_id, args)
                log = Path(result.log_path or "").read_text(encoding="utf-8")
        finally:
            if original is None:
                del registry.TASKS[task_id]
            else:
                registry.TASKS[task_id] = original
        self.assertEqual(result.status, "pass", log)
        self.assertIn(marker, log)

    def test_fault_and_fuzz_tasks_are_executable(self) -> None:
        for task_id in ("agent.fault", "agent.fuzz"):
            task = registry.TASKS[task_id]
            self.assertNotEqual(task.get("coverage"), "pending")
            self.assertTrue(task.get("command"), f"{task_id} 缺少可执行命令")


if __name__ == "__main__":
    unittest.main()
