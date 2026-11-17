#!/usr/bin/env python3
"""统一调度器自测:覆盖注册表完整性与关键结果语义。"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import importlib
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path


TESTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS_DIR))
runner = importlib.import_module("runner")
registry = importlib.import_module("test_registry")
junit = importlib.import_module("check_junit")


class RunnerTest(unittest.TestCase):
    """调度器必须保证缺实现、缺依赖和 dry-run 都不伪装为通过。"""

    def test_registry_references_are_complete(self) -> None:
        runner.validate_registry()

    def test_并发基座检查互不干扰(self) -> None:
        with ThreadPoolExecutor(max_workers=8) as executor:
            results = list(executor.map(lambda _: runner._builtin_shared_harness(), range(32)))
        self.assertTrue(all(status == "pass" for status, _ in results), results)

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

    def test_live_can_be_explicitly_enabled(self) -> None:
        args = argparse.Namespace(dry_run=True, seed=None, report_dir=str(TESTS_DIR))
        with mock.patch.dict("os.environ", {"ZHUYE_LIVE": "1"}):
            result = runner.run_task("agent.eval", args)
        self.assertIn("[dry-run]", result.detail)

    def test_质量评测显式开启缺配置为基础设施错误(self) -> None:
        args = argparse.Namespace(dry_run=False, seed=None, report_dir=str(TESTS_DIR), timeout=5, artifact=None)
        with mock.patch.dict("os.environ", {"ZHUYE_EVAL": "1"}, clear=True):
            result = runner.run_task("agent.quality", args)
        self.assertEqual(result.status, "infra_error")
        self.assertIn("ZHUYE_EVAL_MODEL", result.detail)

    def test_artifact_task_uses_requested_directory(self) -> None:
        with tempfile.TemporaryDirectory() as report_dir:
            args = argparse.Namespace(dry_run=False, seed=None, report_dir=report_dir,
                                      timeout=5, artifact=report_dir)
            result = runner.run_task("agent.release.artifact", args)
        self.assertEqual(result.status, "fail")
        self.assertIn("退出码 2", result.detail)

    def test_no_build_requires_artifact(self) -> None:
        self.assertEqual(runner.main(["run", "--suite", "release", "--no-build"]), 2)

    def test_损坏产物失败且不调用cargo(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "zhuye"
            binary.write_text('#!/bin/sh\necho "zhuye 0.1.14"\n')
            binary.chmod(0o755)
            cargo = root / "cargo"
            cargo.write_text(f'#!/bin/sh\ntouch "{root}/cargo-called"\nexit 99\n')
            cargo.chmod(0o755)
            for filename in ("zhuye-0.1.14-py3-none-any.whl", "zhuye_py-0.1.14-py3-none-any.whl", "sdk.tgz"):
                (root / filename).write_text("损坏的产物")
            env = dict(os.environ, RUNNER_ARTIFACT=str(root), PATH=str(root) + os.pathsep + os.environ["PATH"])
            result = subprocess.run([sys.executable, str(TESTS_DIR / "release_artifacts.py")],
                                    env=env, capture_output=True, text=True, timeout=60)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((root / "cargo-called").exists())

    def test_junit区分跳过与执行(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "report.xml"
            path.write_text('<testsuite><testcase><skipped/></testcase></testsuite>')
            self.assertEqual(junit.executed(str(path)), 0)
            path.write_text('<testsuite><testcase/><testcase><skipped/></testcase></testsuite>')
            self.assertEqual(junit.executed(str(path)), 1)

    def test_cargo零用例不算通过(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            cargo = Path(tmp) / "cargo"
            cargo.write_text('#!/bin/sh\necho "test result: ok. 0 passed; 0 failed; 0 ignored;"\n')
            cargo.chmod(0o755)
            task_id = "__empty_cargo_test__"
            registry.TASKS[task_id] = {"command": [str(cargo), "test"]}
            try:
                args = argparse.Namespace(dry_run=False, seed=None, report_dir=tmp, timeout=5, artifact=None)
                result = runner.run_task(task_id, args)
            finally:
                del registry.TASKS[task_id]
            self.assertEqual(result.status, "infra_error")

    def test_sdk全部跳过不算通过(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            args = argparse.Namespace(dry_run=False, seed=None, report_dir=tmp, timeout=5, artifact=None)
            task_id = "sdk.python.unit"
            original = registry.TASKS[task_id]
            registry.TASKS[task_id] = {"command": [sys.executable, "-c", "print('2 skipped in 0.01s')"]}
            try:
                result = runner.run_task(task_id, args)
            finally:
                registry.TASKS[task_id] = original
            self.assertEqual(result.status, "infra_error")


if __name__ == "__main__":
    unittest.main()
