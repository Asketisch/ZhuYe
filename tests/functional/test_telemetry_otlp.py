"""能力覆盖层 · OTLP 真实上报:本地 HTTP collector 收真 span。

此前诚实清单记录「OTLP exporter 只验启动不报错」;本文件用进程内
ThreadingHTTPServer 充当 OTLP/HTTP collector,端到端验证:
`[analytics]` opt-in → serve 启动 exporter → 真实 turn 的 span
(`agent.turn`)带 `service.name=zhuye-agent` 资源被批量上报。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_telemetry_otlp.py -v
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import pytest

from wire import DRAIN_TIMEOUT, require_binary

pytestmark = pytest.mark.usefixtures("sandbox")


class _Collector:
    """最小 OTLP/HTTP collector:记录 POST(path / headers / body)。"""

    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []
        self._lock = threading.Lock()
        server = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:  # noqa: N802(http.server 约定)
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length)
                with server._lock:
                    server.requests.append({
                        "path": self.path,
                        "content_type": self.headers.get("Content-Type", ""),
                        "body": body,
                    })
                self.send_response(200)
                self.send_header("Content-Type", "application/x-protobuf")
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, *args: object) -> None:  # 静默默认 stderr 噪声
                pass

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def start(self) -> None:
        self.thread.start()

    def stop(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()

    def snapshot(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self.requests)


def test_O1_otlp_span真实上报(serve_session, mock_script, sandbox) -> None:
    """`[analytics]` opt-in → serve 把真实 turn span 批量上报到本地 collector。

    断言:POST 到 `/v1/traces`、protobuf content-type、payload 含
    `service.name=zhuye-agent` 资源与 `agent.turn` span 名(protobuf
    对字符串原样编码,可在原始字节中检索)。
    """
    require_binary()
    collector = _Collector()
    collector.start()
    sandbox.write_config(
        f"\n[analytics]\nenabled = true\nendpoint = \"http://127.0.0.1:{collector.port}/v1/traces\"\n"
    )
    try:
        script = mock_script(['{"type":"text","text":"otlp done"}'])
        # RUST_LOG 显式 info:shell 常见的 RUST_LOG=warn 会把 info 级
        # agent.turn span 整层过滤掉(EnvFilter 是全局的,OTLP 层同受控)。
        with serve_session(env=sandbox.mock_env(script, RUST_LOG="info")) as session:
            session.submit(
                {"type": "user_input", "items": [{"type": "text", "text": "上报一轮"}]},
                timeout=DRAIN_TIMEOUT,
            )
            # batch exporter 默认 ~5s 排程;轮询等待首个 POST(余量充足)。
            import time

            deadline = time.monotonic() + 20.0
            while time.monotonic() < deadline and not collector.snapshot():
                time.sleep(0.2)

        posts = [r for r in collector.snapshot() if r["path"].endswith("/v1/traces")]
        assert posts, "OTLP collector 未收到任何 /v1/traces 上报"
        assert any("application/x-protobuf" in r["content_type"] for r in posts), posts[0][
            "content_type"
        ]
        joined = b"".join(r["body"] for r in posts)
        assert b"zhuye-agent" in joined, "payload 缺 service.name 资源"
        assert b"agent.turn" in joined, "payload 缺 agent.turn span"
    finally:
        collector.stop()
