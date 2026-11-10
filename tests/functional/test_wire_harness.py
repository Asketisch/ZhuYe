"""原始协议测试设施自身的错误检测和断线行为。"""

from __future__ import annotations

import json

import pytest
import wire


def _session_with_stdout(sandbox, monkeypatch, lines):
    binary = sandbox.cwd / "fake-serve"
    binary.write_text("#!/usr/bin/env python3\nimport sys\n" + "".join(
        f"print({line!r}, flush=True)\n" for line in lines
    ))
    binary.chmod(0o755)
    monkeypatch.setattr(wire, "ZHUYE_BIN", binary)
    session = wire.ServeSession(sandbox)
    with session._cond:
        finished = session._cond.wait_for(lambda: session._stdout_eof, timeout=30)
    if not finished:
        session.kill()
        session._proc.wait(timeout=5)
        pytest.fail("模拟服务未在 30 秒内关闭 stdout")
    session._reader.join(timeout=30)
    assert not session._reader.is_alive()
    return session


@pytest.mark.parametrize("bad_line", ["非 JSON", '[]', '{"id":"x","msg":null}'])
def test_无效stdout不会被测试设施静默忽略(sandbox, monkeypatch, bad_line) -> None:
    session = _session_with_stdout(sandbox, monkeypatch, [bad_line])
    try:
        with pytest.raises(AssertionError, match="serve stdout"):
            session.wait_global(lambda _event: False, timeout=5)
    finally:
        session.close()


def test_进程提前退出立即暴露断线(sandbox, monkeypatch) -> None:
    session = _session_with_stdout(sandbox, monkeypatch, [])
    try:
        with pytest.raises(AssertionError, match="stdout 提前关闭"):
            session.wait_event("missing", "turn_complete", timeout=5)
    finally:
        session.close()


def test_断线前已排队的终态仍可消费(sandbox, monkeypatch) -> None:
    event = {"id": "sub", "msg": {"type": "turn_complete", "status": "success"}}
    session = _session_with_stdout(sandbox, monkeypatch, [json.dumps(event)])
    try:
        assert session.collect("sub", timeout=5) == [event]
    finally:
        session.close()


def test_未读取的非法输出也使场景失败(sandbox, monkeypatch) -> None:
    session = _session_with_stdout(sandbox, monkeypatch, ["非法输出"])
    with pytest.raises(AssertionError, match="serve stdout"):
        with session:
            pass
