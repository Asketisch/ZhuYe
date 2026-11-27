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

    def test_交错事件按回合拼接完整回复(self):
        events = [
            {"id": "first", "msg": {"type": "turn_started"}},
            {"id": "first", "msg": {"type": "agent_message_delta", "delta": "一"}},
            {"id": "second", "msg": {"type": "turn_started"}},
            {"id": "second", "msg": {"type": "agent_message_delta", "delta": "三"}},
            {"id": "first", "msg": {"type": "agent_message", "text": "一二"}},
            {"id": "second", "msg": {"type": "agent_message_delta", "delta": "四"}},
        ]
        self.assertEqual(quality.events_text(events), "一二三四")

    def test_中断工具前的调用用量保留且完整回合不重复计费(self):
        events = [
            {"id": "interrupted", "msg": {"type": "token_count", "input_tokens": 9, "cached_tokens": 3}},
            {"id": "other", "msg": {"type": "token_count", "input_tokens": 7}},
            {"id": "interrupted", "msg": {"type": "turn_aborted"}},
            {"id": "other", "msg": {"type": "token_count", "input_tokens": 15, "turn_total": True}},
            {"id": "other", "msg": {"type": "turn_complete", "usage": {"input_tokens": 15}}},
        ]
        usage = quality.reported_usage(events)
        self.assertEqual(sum(item["input_tokens"] for item in usage), 24)
        self.assertEqual(sum(item.get("cached_tokens", 0) for item in usage), 3)

    def test_等待首轮时响应第二轮审批并按请求去重(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "serve"
            binary.write_text("""#!/usr/bin/env python3
import json, sys

def emit(sid, **msg):
    print(json.dumps({"id": sid, "msg": msg}), flush=True)

for line in sys.stdin:
    item = json.loads(line)
    op = item["op"]
    if item["id"] == "second":
        emit("second", type="plan_ready", plan_id="p")
        emit("second", type="plan_ready", plan_id="p")
    elif op == {"type": "plan_approval", "id": "p", "choice": "manual_approve"}:
        emit("first", type="turn_complete")
        emit("second", type="turn_complete")
    elif op["type"] == "shutdown":
        emit(item["id"], type="shutdown_complete")
        break
""")
            binary.chmod(0o755)
            with mock.patch.object(quality.wire, "ZHUYE_BIN", binary):
                with quality.wire.ServeSession(quality.wire.Sandbox(root, root)) as session:
                    respond = quality.evaluation_interactions(session)
                    session.send({"type": "user_input"}, "first")
                    session.send({"type": "user_input"}, "second")
                    first = session.collect("first", timeout=5, on_session_event=respond)
                    second = session.collect("second", timeout=5, on_session_event=respond)
                    self.assertEqual({event["id"] for event in first}, {"first"})
                    self.assertEqual({event["id"] for event in second}, {"second"})
                    records = [json.loads(line) for line in session.transcript_path.read_text().splitlines()]
                    operations = [json.loads(record["line"])["op"] for record in records if record["direction"] == "stdin"]
                    self.assertEqual(sum(op["type"] == "plan_approval" for op in operations), 1)

    def test_评测明确拒绝独立工具审批(self):
        session = mock.Mock()
        respond = quality.evaluation_interactions(session)
        request = {"msg": {"type": "approval_request", "request_id": "tool-1"}}
        respond(request)
        respond(request)
        session.send.assert_called_once_with({"type": "tool_approval", "id": "tool-1",
            "decision": {"deny": {"reason": "评测不会代批需要人工授权的工具"}}})

    def test_中断前后审批计划且副作用只发生一次(self):
        binary = Path(os.environ.get("ZHUYE_BIN", ROOT / "target/debug/zhuye"))
        if not binary.is_file():
            self.skipTest("需要先构建 zhuye-cli")
        case = next(case for case in json.loads(quality.CASES.read_text()) if case["kind"] == "interrupted_tool")
        offline_script = quality.offline_script

        def scripted_plan(case, expected, workspace, phase):
            return [json.dumps({"type": "tool_call", "name": "ExitPlanMode",
                                "args": {"markdown": "# 核实工作区"}}),
                    *offline_script(case, expected, workspace, phase)]

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(quality, "offline_script", scripted_plan):
            result = quality.run_one(case, binary.resolve(), "mock", "http://127.0.0.1:0", "", Path(tmp), offline=True)
            events = json.loads((Path(tmp) / "events.json").read_text())
            self.assertEqual(result["status"], "pass", (result, events))
            self.assertEqual(sum(event["msg"]["type"] == "plan_approved" for event in events), 2)
            self.assertEqual(result["tools"].count("bash"), 1)
            self.assertTrue(result["objective_passed"])

    @mock.patch.object(quality, "sandbox_command", side_effect=lambda _workspace, command: command)
    def test_固定任务和客观验收(self, _sandbox):
        cases = json.loads(quality.CASES.read_text())
        self.assertEqual(len(cases), 18)
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
                elif case["kind"] == "concurrent_turns":
                    (path / "a.txt").write_text("alpha")
                    (path / "b.txt").write_text("beta")
                else:
                    target = "result.json" if case["kind"] in ("merge", "untrusted_file", "batch_order", "isolation_failure", "interrupted_tool") else "status.json"
                    (path / target).write_text(json.dumps(expected))
                    if case["kind"] == "batch_order":
                        (path / "state.txt").write_text(case["after"])
                    if case["kind"] == "interrupted_tool":
                        (path / "counter.txt").write_text("once\n")
                self.assertTrue(quality.verify(case, path, expected)[0])
                if case["kind"] in ("repair", "merge", "untrusted_file", "batch_order", "isolation_failure", "interrupted_tool", "concurrent_turns"):
                    (path / "protected.txt").write_text("changed")
                    self.assertFalse(quality.verify(case, path, expected)[0])

    def test_按类别汇总成功率缓存与成本(self):
        results = [
            {"name": "a", "kind": "repair", "status": "pass", "elapsed_s": 2,
             "tools": ["read"], "tool_errors": 0, "cache_warming_calls": 1,
             "cache_warming_cached_tokens": 5,
             "usage": [{"input_tokens": 100, "output_tokens": 20, "cached_tokens": 40, "cache_write_tokens": 10}]},
            {"name": "b", "kind": "repair", "status": "fail", "elapsed_s": 4,
             "tools": ["write"], "tool_errors": 1,
             "usage": [{"input_tokens": 100, "output_tokens": 30, "cached_tokens": 60}]},
        ]
        group = quality.summarize_categories(results, 1.0, 2.0)["repair"]
        self.assertEqual(group["success_rate"], 0.5)
        self.assertEqual(group["cache_hit_rate"], 0.5)
        self.assertEqual(group["cache_write_tokens"], 10)
        self.assertEqual(group["cache_warming_calls"], 1)
        self.assertEqual(group["tool_errors"], 1)
        self.assertAlmostEqual(group["cost_usd"], 0.0003)

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
        complete = quality.summarize(results[1:], 1.0, 2.0)
        self.assertEqual(complete["x"]["cost_usd"], 0.00014)
        difference = quality.compare_groups(complete, {"x": summary})["x"]
        self.assertFalse(difference["complete"])
        self.assertEqual(difference["infra_error_delta"], 1)
        self.assertIsNone(difference["mean_elapsed_delta_s"])
        self.assertIsNone(difference["input_tokens_delta"])

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
            executions = []

            def fake_run(*args, **_kwargs):
                executions.append(args[5].relative_to(root / "out").parts)
                return {"name": "single", "kind": "repair", "status": "pass", "reason": "ok",
                        "elapsed_s": 1, "text": "完成 SECRET-VALUE", "tools": ["write"],
                        "usage": [{"input_tokens": 1, "output_tokens": 2}], "objective_passed": True}
            with mock.patch.object(quality, "CASES", cases), mock.patch.object(quality, "run_one", side_effect=fake_run), \
                    mock.patch.object(quality, "ensure_sandbox"), \
                    mock.patch.object(sys, "argv", args), mock.patch.dict(os.environ, {"ZHUYE_EVAL_API_KEY": "SECRET-VALUE"}):
                self.assertEqual(quality.main(), 0)
            self.assertEqual(executions, [("baseline", "single", "1"), ("candidate", "single", "1"),
                                          ("candidate", "single", "2"), ("baseline", "single", "2")])
            raw = (root / "out/report.json").read_text()
            report = json.loads(raw)
            self.assertNotIn("SECRET-VALUE", raw)
            self.assertEqual(report["versions"]["candidate"]["cases"]["single"]["pass"], 2)
            self.assertEqual(report["comparison"]["single"]["pass_delta"], 0)

    def test_真实进程和本地模型网关批准计划后完成修复(self):
        binary = Path(os.environ.get("ZHUYE_BIN", ROOT / "target/debug/zhuye"))
        if not binary.is_file():
            self.skipTest("需要先构建 zhuye-cli")
        requests = []

        class Gateway(BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                steps = [
                    ("EnterPlanMode", {"task": "修复计算函数"}),
                    ("ExitPlanMode", {"markdown": "# 修复计划"}),
                    ("read", {"path": "calc.py"}),
                    ("write", {"path": "calc.py", "content": "def calculate(a, b): return a + b\n"}),
                ]
                if len(requests) <= len(steps):
                    name, arguments = steps[len(requests) - 1]
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
                self.assertGreaterEqual(len(requests), 5)
                self.assertIn("修复完成", result["text"])
                messages = [event["msg"] for event in json.loads((Path(tmp) / "events.json").read_text())]
                self.assertTrue(any(msg["type"] == "plan_request" for msg in messages))
                self.assertTrue(any(msg["type"] == "plan_approved" for msg in messages))
                self.assertTrue(any(msg["type"] == "permission_mode_changed" and msg["to"] == "prompt" for msg in messages))
        finally:
            gateway.shutdown()
            thread.join(timeout=5)
            gateway.server_close()

    def test_真二进制压缩摘要经重启传入后续请求(self):
        binary = Path(os.environ.get("ZHUYE_BIN", ROOT / "target/debug/zhuye"))
        if not binary.is_file():
            self.skipTest("需要先构建 zhuye-cli")
        case = next(case for case in json.loads(quality.CASES.read_text()) if case["kind"] == "resume")
        expected = {key: case[key] for key in ("code", "color", "priority")}
        marker = "摘要中保留的核验标识"
        summary_requests = []
        resumed_requests = []

        class Gateway(BaseHTTPRequestHandler):
            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                messages = request["messages"]
                prompt = json.dumps(messages[-1], ensure_ascii=False)
                delta, finish = {"content": "已确认"}, "stop"
                if "对话摘要助手" in prompt:
                    summary_requests.append(request)
                    # 重启阶段的再次压缩只能沿用已恢复的标识。
                    summary_marker = marker if "按之前约定写入 status.json。" not in prompt or marker in prompt else ""
                    delta = {"content": f"<summary>{summary_marker} {json.dumps(expected)}</summary>"}
                elif "按之前约定写入 status.json。" in prompt:
                    resumed_requests.append(request)
                    if marker in json.dumps(messages, ensure_ascii=False):
                        delta = {"tool_calls": [{"index": 0, "id": "resume-status", "type": "function",
                                                 "function": {"name": "write", "arguments": json.dumps({
                                                     "path": "status.json", "content": json.dumps(expected)
                                                 })}}]}
                        finish = "tool_calls"
                chunk = {"id": "eval-resume", "model": "test",
                         "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
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
            with tempfile.TemporaryDirectory() as tmp:
                result = quality.run_one(case, binary.resolve(), "test", f"http://127.0.0.1:{gateway.server_port}",
                                         "test-secret", Path(tmp))
                self.assertEqual(result["status"], "pass", (result, (Path(tmp) / "events.json").read_text()))
                self.assertTrue(summary_requests)
                self.assertTrue(resumed_requests)
        finally:
            gateway.shutdown()
            thread.join(timeout=5)
            gateway.server_close()
