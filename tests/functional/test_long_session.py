"""长会话压缩、落盘和重启后的组合场景。"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from wire import first_event, msg_types, rollout_files, rollout_thread_id


def test_摘要压缩后跨进程恢复并增量摘要(sandbox, serve_session) -> None:
    requests: list[dict] = []
    summaries: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            serialized = json.dumps(request["messages"], ensure_ascii=False)
            if "你是一个专业的对话摘要助手" in serialized:
                summaries.append(request)
                text = f"<summary>保留项目代号北极星;摘要版本{len(summaries)}</summary>"
            else:
                text = f"正常回复{len(requests) - len(summaries)}"
            chunk = {"id": "long-session", "model": "gpt-4o", "choices": [{
                "index": 0, "delta": {"content": text}, "finish_reason": "stop",
            }]}
            body = ("data: " + json.dumps(chunk, ensure_ascii=False)
                    + "\n\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config(
            '\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
            f'base_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {
            "ZHUYE_MODEL": "openai/gpt-4o",
            "ZHUYE_HOME": str(sandbox.home / ".zhuye"),
            "ZHUYE_AUTO_COMPACT_INPUT_TOKENS": "1000",
        }
        with serve_session(env=env) as session:
            first = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "项目代号是北极星"}
            ]})
            assert first_event(first, "turn_complete")["msg"]["status"] == "success", msg_types(first)
            ack = session.submit({"type": "compact"})
            assert first_event(ack, "context_compacted")["msg"]["strategy"] == "noop"
            second = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "长背景:" + "x" * 6000}
            ]}, timeout=30)
            compressed = first_event(second, "context_compacted")
            assert compressed is not None and compressed["msg"]["strategy"] == "ll_m_summarize", second
            assert compressed["msg"]["before_tokens"] == 2**32 - 1
            assert compressed["msg"]["after_tokens"] > 0
            assert first_event(second, "turn_complete")["msg"]["status"] == "success", msg_types(second)
            third = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "同一会话继续"}
            ]}, timeout=30)
            next_compacted = first_event(third, "context_compacted")
            assert next_compacted is not None and next_compacted["msg"]["strategy"] == "ll_m_summarize", third
            assert first_event(third, "turn_complete")["msg"]["status"] == "success", msg_types(third)

        tid = rollout_thread_id(sandbox.home)
        records = [json.loads(line) for line in rollout_files(sandbox.home)[-1].read_text().splitlines() if line]
        compactions = [r for r in records if r.get("type") == "compaction" and r.get("strategy") == "llm_summarize"]
        assert len(compactions) == 2, compactions
        record = compactions[0]
        assert "保留项目代号北极星;摘要版本1" in record["summary"], record
        assert record["before_tokens"] == compressed["msg"]["before_tokens"]
        assert record["after_tokens"] == compressed["msg"]["after_tokens"]
        assert "摘要版本2" in compactions[1]["summary"]
        assert len(summaries) == 2
        assert "项目代号是北极星" in json.dumps(summaries[0]["messages"], ensure_ascii=False)
        incremental = json.dumps(summaries[1]["messages"], ensure_ascii=False)
        assert "<previous-summary>" in incremental and "摘要版本1" in incremental, incremental
        assert "摘要版本2" in json.dumps(requests[-1]["messages"], ensure_ascii=False)

        with serve_session(args=["--resume", tid], env=env) as resumed:
            fourth = resumed.submit({"type": "user_input", "items": [
                {"type": "text", "text": "继续处理这个项目"}
            ]}, timeout=30)
            compressed = first_event(fourth, "context_compacted")
            assert compressed is not None and compressed["msg"]["strategy"] == "ll_m_summarize", fourth
            assert first_event(fourth, "turn_complete")["msg"]["status"] == "success", msg_types(fourth)
        assert len(summaries) == 3, summaries
        incremental = json.dumps(summaries[2]["messages"], ensure_ascii=False)
        assert "<previous-summary>" in incremental and "摘要版本2" in incremental, incremental
        resumed_model = json.dumps(requests[-1]["messages"], ensure_ascii=False)
        assert "摘要版本3" in resumed_model and "继续处理这个项目" in resumed_model, resumed_model
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
