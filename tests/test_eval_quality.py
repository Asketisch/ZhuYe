"""真实模型评测的任务构造、客观验收与执行结果语义。"""

import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest import mock
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("quality_eval", ROOT / "tests/eval/run.py")
quality = importlib.util.module_from_spec(spec)
spec.loader.exec_module(quality)


class QualityTest(unittest.TestCase):
    def test_非流式完整消息替换之前的增量(self):
        events = [{"msg": {"type": "turn_started"}},
                  {"msg": {"type": "agent_message_delta", "delta": "一"}},
                  {"msg": {"type": "agent_message", "text": "一二"}}]
        self.assertEqual(quality.events_text(events), "一二")

    @mock.patch.object(quality, "sandbox_command", side_effect=lambda _workspace, command: command)
    def test_十二个固定任务和客观验收(self, _sandbox):
        cases = json.loads(quality.CASES.read_text())
        self.assertEqual(len(cases), 12)
        self.assertEqual({kind: sum(c["kind"] == kind for c in cases)
                          for kind in ("repair", "resume", "merge")},
                         {"repair": 4, "resume": 4, "merge": 4})
        expressions = {"add": "a+b", "subtract": "a-b", "multiply": "a*b", "maximum": "max(a,b)"}
        for case in cases:
            with self.subTest(case=case["name"]), tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp)
                _, expected = quality.prepare(case, path)
                self.assertFalse(quality.verify(case, path, expected)[0])
                if case["kind"] == "repair":
                    (path / "calc.py").write_text("def calculate(a,b): return " + expressions[case["operation"]])
                else:
                    target = "result.json" if case["kind"] == "merge" else "status.json"
                    (path / target).write_text(json.dumps(expected))
                self.assertTrue(quality.verify(case, path, expected)[0])
                if case["kind"] in ("repair", "merge"):
                    (path / "protected.txt").write_text("changed")
                    self.assertFalse(quality.verify(case, path, expected)[0])

    @mock.patch.object(quality, "sandbox_command", side_effect=lambda _workspace, command: command)
    def test_执行流程验证文件与工具事件(self, _sandbox):
        case = json.loads(quality.CASES.read_text())[0]

        class Session:
            def __init__(self, sandbox, **_kwargs):
                self.sandbox = sandbox
            def __enter__(self):
                return self
            def __exit__(self, *_args):
                return False
            def wait_global(self, *_args, **_kwargs):
                return {"msg": {"type": "session_configured"}}
            def submit(self, *_args, **_kwargs):
                (self.sandbox.cwd / "calc.py").write_text("def calculate(a,b): return a+b")
                return [{"msg": {"type": "tool_call_begin", "tool_name": "write"}},
                        {"msg": {"type": "agent_message_delta", "delta": "完成"}},
                        {"msg": {"type": "turn_complete", "status": "success", "usage": {"input_tokens": 3, "output_tokens": 2}}}]

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(quality.wire, "ServeSession", Session):
            result = quality.run_one(case, Path("/fake"), "model", "http://example", "secret-value", Path(tmp))
            self.assertEqual(result["status"], "pass")
            self.assertEqual(result["tools"], ["write"])
            self.assertEqual(result["usage"][0]["input_tokens"], 3)
            self.assertTrue((Path(tmp) / "events.json").exists())

    def test_模型代码不能读取宿主HOME(self):
        if not ROOT.is_relative_to(Path.home()):
            self.skipTest("仓库不位于宿主 HOME,无法构造读取边界样本")
        case = json.loads(quality.CASES.read_text())[0]
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            _, expected = quality.prepare(case, workspace)
            (workspace / "calc.py").write_text(
                f'def calculate(a,b):\n    open({str(ROOT / "Cargo.toml")!r}).read()\n    return a+b\n')
            try:
                allowed, _ = quality.verify(case, workspace, expected)
            except quality.EvaluationInfrastructureError as exc:
                self.skipTest(str(exc))
            self.assertFalse(allowed, "沙箱不能允许模型代码读取宿主 HOME")

    def test_linux沙箱命令隔离网络和宿主目录(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(quality.sys, "platform", "linux"), \
                mock.patch.object(quality.shutil, "which", return_value="/usr/bin/bwrap"):
            command = quality.sandbox_command(Path(tmp), ["python3", "-c", "pass"])
        self.assertIn("--unshare-all", command)
        self.assertIn("--die-with-parent", command)
        self.assertIn("--ro-bind", command)
        self.assertIn(str(Path.home().resolve()), command)

    def test_缺少系统沙箱时不能裸跑(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(quality.sys, "platform", "linux"), \
                mock.patch.object(quality.shutil, "which", return_value=None):
            with self.assertRaises(quality.EvaluationInfrastructureError):
                quality.ensure_sandbox(Path(tmp))

    def test_恢复场景要求真实压缩和恢复产物(self):
        case = json.loads(quality.CASES.read_text())[4]
        expected = {key: case[key] for key in ("code", "color", "priority")}

        class Session:
            def __init__(self, sandbox, args=None, **_kwargs):
                self.sandbox, self.resumed = sandbox, bool(args)
            def __enter__(self):
                return self
            def __exit__(self, *_args):
                return False
            def wait_global(self, *_args, **_kwargs):
                return {"msg": {"type": "session_configured"}}
            def submit(self, *_args, **_kwargs):
                if self.resumed:
                    (self.sandbox.cwd / "status.json").write_text(json.dumps(expected))
                return [{"msg": {"type": "context_compacted", "strategy": "ll_m_summarize"}},
                        {"msg": {"type": "agent_message_delta", "delta": "完成"}},
                        {"msg": {"type": "turn_complete", "status": "success", "usage": {}}}]

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(quality.wire, "ServeSession", Session), \
                mock.patch.object(quality.wire, "rollout_thread_id", return_value="thread"):
            result = quality.run_one(case, Path("/fake"), "model", "http://example", "secret-value", Path(tmp))
            self.assertEqual(result["status"], "pass", result)

    def test_合并任务必须从工具失败中恢复(self):
        case = json.loads(quality.CASES.read_text())[8]

        class Session:
            def __init__(self, sandbox, **_kwargs):
                self.sandbox = sandbox
            def __enter__(self):
                return self
            def __exit__(self, *_args):
                return False
            def wait_global(self, *_args, **_kwargs):
                return {"msg": {"type": "session_configured"}}
            def submit(self, *_args, **_kwargs):
                (self.sandbox.cwd / "result.json").write_text(json.dumps({"label": case["label"], "sum": sum(case["values"])}))
                return [{"msg": {"type": "tool_call_end", "is_error": True}},
                        {"msg": {"type": "tool_call_end", "is_error": False}},
                        {"msg": {"type": "agent_message_delta", "delta": "完成"}},
                        {"msg": {"type": "turn_complete", "status": "success", "usage": {}}}]

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(quality.wire, "ServeSession", Session):
            result = quality.run_one(case, Path("/fake"), "model", "http://example", "secret-value", Path(tmp))
            self.assertEqual(result["status"], "pass", result)

    def test_统计不把基础设施错误记成通过(self):
        results = [{"name": "x", "status": "infra_error", "elapsed_s": 2, "tools": [], "usage": []},
                   {"name": "x", "status": "pass", "elapsed_s": 1, "tools": ["read"],
                    "usage": [{"input_tokens": 100, "output_tokens": 20}]}]
        summary = quality.summarize(results, 1.0, 2.0)["x"]
        self.assertEqual((summary["pass"], summary["infra_error"]), (1, 1))
        self.assertIsNone(summary["cost_usd"])
        self.assertEqual(quality.summarize(results[1:], 1.0, 2.0)["x"]["cost_usd"], 0.00014)

    def test_评分器实际断言回复与工具预算(self):
        scorer = ROOT / "target/debug/zhuye-eval-score"
        if not scorer.is_file():
            self.skipTest("需要先构建 zhuye-eval-score")
        results = [
            {"name": "first", "kind": "repair", "status": "pass", "text": "完成", "tools": ["write"]},
            {"name": "second", "kind": "repair", "status": "pass", "text": "", "tools": []},
            {"name": "third", "kind": "repair", "status": "infra_error", "text": "", "tools": []},
        ]
        quality.score(scorer, results)
        self.assertEqual([r["status"] for r in results], ["pass", "fail", "infra_error"])
        self.assertEqual(results[2]["score"]["assertions"][0]["kind"], "output_present")

    def test_双版本结果完整比较并隐藏凭据(self):
        scorer = ROOT / "target/debug/zhuye-eval-score"
        if not scorer.is_file():
            self.skipTest("需要先构建 zhuye-eval-score")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cases = root / "cases.json"
            cases.write_text(json.dumps([{"name": "single", "kind": "repair"}]))
            binary = root / "zhuye"
            binary.write_text('#!/bin/sh\necho "zhuye 1.0"\n')
            binary.chmod(0o755)
            args = ["eval", "--candidate", str(binary), "--baseline", str(binary), "--model", "model",
                    "--base-url", "http://localhost", "--scorer", str(scorer), "--output", str(root / "out"),
                    "--repetitions", "2"]
            def fake_run(*_args):
                return {"name": "single", "kind": "repair", "status": "pass", "reason": "ok",
                        "elapsed_s": 1, "text": "完成 SECRET-VALUE", "tools": ["write"],
                        "usage": [{"input_tokens": 1, "output_tokens": 2}], "objective_passed": True}
            with mock.patch.object(quality, "CASES", cases), mock.patch.object(quality, "run_one", side_effect=fake_run), \
                    mock.patch.object(quality, "ensure_sandbox"), \
                    mock.patch.object(sys, "argv", args), mock.patch.dict(os.environ, {"ZHUYE_EVAL_API_KEY": "SECRET-VALUE"}):
                self.assertEqual(quality.main(), 0)
            raw = (root / "out/report.json").read_text()
            report = json.loads(raw)
            self.assertNotIn("SECRET-VALUE", raw)
            self.assertEqual(report["versions"]["candidate"]["cases"]["single"]["pass"], 2)
            self.assertEqual(report["comparison"]["single"]["pass_delta"], 0)

    def test_真实进程和本地模型网关完成修复(self):
        binary = Path(os.environ.get("ZHUYE_BIN", ROOT / "target/debug/zhuye"))
        if not binary.is_file():
            self.skipTest("需要先构建 zhuye-cli")
        requests = []

        class Gateway(BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                if len(requests) <= 2:
                    name = "read" if len(requests) == 1 else "write"
                    arguments = {"path": "calc.py"}
                    if name == "write":
                        arguments["content"] = "def calculate(a, b): return a + b\n"
                    delta = {"tool_calls": [{"index": 0, "id": f"fix-{len(requests)}", "type": "function",
                                             "function": {"name": name, "arguments": json.dumps(arguments)}}]}
                    finish = "tool_calls"
                else:
                    delta, finish = {"content": "修复完成"}, "stop"
                chunk = {"id": "eval", "model": "test", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
                body = ("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            def log_message(self, *_args):
                pass

        gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
        thread = threading.Thread(target=gateway.serve_forever, daemon=True)
        thread.start()
        try:
            case = json.loads(quality.CASES.read_text())[0]
            with tempfile.TemporaryDirectory() as tmp:
                result = quality.run_one(case, binary.resolve(), "test", f"http://127.0.0.1:{gateway.server_port}",
                                         "test-secret", Path(tmp))
                self.assertEqual(result["status"], "pass", (result, (Path(tmp) / "events.json").read_text()))
                self.assertIn("write", result["tools"])
                self.assertGreaterEqual(len(requests), 2)
                self.assertIn("修复完成", result["text"])
        finally:
            gateway.shutdown()
            thread.join(timeout=5)
            gateway.server_close()
