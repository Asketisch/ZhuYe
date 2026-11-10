"""验证已安装的 PyO3 wheel 在真实 Python 进程中的行为。"""

import json
import importlib.metadata
import os
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

import zhuye_py


class NativePythonTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.workspace = self.root / "workspace"
        (self.home / ".zhuye").mkdir(parents=True)
        self.workspace.mkdir()
        (self.home / ".zhuye/config.toml").write_text(
            "[hooks]\nenabled = []\n", encoding="utf-8"
        )
        environment = {
            "HOME": str(self.home),
            "ZHUYE_HOME": str(self.home / ".zhuye"),
            "PATH": os.environ.get("PATH", ""),
        }
        self.env = patch.dict(os.environ, environment, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)

    def test_wheel导入并多轮运行工具(self):
        self.assertTrue(Path(zhuye_py.__file__).resolve().is_relative_to(Path(sys.prefix)))
        self.assertEqual(importlib.metadata.version("zhuye-py"), zhuye_py.__version__)
        (self.workspace / "notes.txt").write_text("测试文件内容", encoding="utf-8")
        script = self.root / "mock.jsonl"
        script.write_text("\n".join(json.dumps(item, ensure_ascii=False) for item in [
            {"type": "text", "text": "第一轮回复"},
            {"type": "tool_call", "name": "read", "args": {"path": "notes.txt"}},
            {"type": "text", "text": "文件已读取"},
        ]) + "\n", encoding="utf-8")
        os.environ["ZHUYE_MOCK_SCRIPT"] = str(script)
        builder = zhuye_py.ZhuyeBuilder("mock").workspace(str(self.workspace)).approvals(False)
        self.assertEqual(builder.describe(), ("mock", str(self.workspace), False, False))
        agent = builder.build()
        self.assertEqual(agent.model(), "mock")
        self.assertEqual(agent.workspace(), str(self.workspace))
        self.assertFalse(agent.approvals_enabled())
        first = agent.run("第一轮")
        self.assertEqual((first.status, first.text), ("success", "第一轮回复"))
        self.assertEqual((first.input_tokens, first.output_tokens), (0, 4))
        second = agent.run("读取 notes.txt")
        self.assertEqual((second.status, second.text), ("success", "文件已读取"))
        self.assertEqual((second.input_tokens, second.output_tokens), (0, 8))
        self.assertGreaterEqual(second.elapsed_ms, 0)

    def test_网络等待期间释放GIL并准确返回用量(self):
        entered = threading.Event()
        release = threading.Event()
        requests = []

        class Gateway(BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                entered.set()
                release.wait(timeout=15)
                body = (json.dumps({"model": "llama3.2", "message": {
                    "role": "assistant", "content": "网关回复"}, "done": False}) + "\n" +
                    json.dumps({"model": "llama3.2", "message": {
                        "role": "assistant", "content": ""}, "done": True,
                        "prompt_eval_count": 42, "eval_count": 17}) + "\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/x-ndjson")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_args):
                pass

        gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
        server = threading.Thread(target=gateway.serve_forever, daemon=True)
        server.start()
        os.environ["OLLAMA_HOST"] = f"http://127.0.0.1:{gateway.server_port}"
        result = []
        try:
            agent = zhuye_py.ZhuyeBuilder("ollama/llama3.2").workspace(
                str(self.workspace)).approvals(False).build()

            def run():
                try:
                    result.append(agent.run("等待网关"))
                except Exception as exc:
                    result.append(exc)

            worker = threading.Thread(target=run, daemon=True)
            worker.start()
            self.assertTrue(entered.wait(timeout=10), "Python 线程无法在模型等待期间运行")
            self.assertEqual(requests[0]["model"], "llama3.2")
            release.set()
            worker.join(timeout=15)
            self.assertFalse(worker.is_alive(), "模型响应后 run 未结束")
            self.assertEqual(len(result), 1)
            self.assertFalse(isinstance(result[0], Exception), result)
            self.assertEqual((result[0].status, result[0].text), ("success", "网关回复"))
            self.assertEqual((result[0].input_tokens, result[0].output_tokens), (42, 17))
        finally:
            release.set()
            gateway.shutdown()
            server.join(timeout=5)
            gateway.server_close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
