"""能力覆盖层 · v1.11 基建#6:队列支持多 goal(goal 任务入队与顺序执行)。

覆盖:
- `queue add_goal` 字段级入队(goal 载荷落盘:objective / verify_command /
  token_budget,kind=goal)
- goal 任务以目标模式执行:driver 注入 EnterGoalMode → objective turn →
  (mock verifier 判 met)→ Complete → ExitGoalMode 收尾,任务结清
  done 且 note 记录 goal complete(与普通 prompt 任务的机器可辨差异)

依赖:goal-queue 插件(与 test_v110 V7 同款安装路径)。
依据:AGENTS.md 开发流程检查表 —— 能力断言到字段级。
"""

from __future__ import annotations

import json
import re
import time
from pathlib import Path

import pytest

from wire import ZHUYE_BIN, msg_types, require_binary, rollout_thread_id, run_cli

pytestmark = pytest.mark.usefixtures("sandbox")

# 仓库根(与 test_v110.py 同款推导:binary 位于 target/debug 下)。
REPO_ROOT = Path(ZHUYE_BIN).resolve().parents[2]


def _install_goal_queue_plugin(sandbox) -> None:
    """安装并启用 goal-queue 插件(与 test_v110 V7 同款)。"""
    src = REPO_ROOT / "plugins" / "goal-queue"
    install = run_cli(sandbox, "plugin", "install", str(src), timeout=60)
    assert install.returncode == 0, f"plugin install 失败:{install.stdout + install.stderr}"
    enable = run_cli(sandbox, "plugin", "enable", "goal-queue@inline", timeout=30)
    assert enable.returncode == 0, f"plugin enable 失败:{enable.stdout + enable.stderr}"


def _wait_status(store: Path, task_id: str, deadline_s: float = 30.0) -> dict:
    """轮询 queue store 直到任务终态(done/failed),返回任务对象。"""
    deadline = time.monotonic() + deadline_s
    last = None
    while time.monotonic() < deadline:
        if store.exists():
            tasks = json.loads(store.read_text())["tasks"]
            hit = next((t for t in tasks if t["id"] == task_id), None)
            if hit and hit["status"] in ("done", "failed"):
                return hit
            last = hit
        time.sleep(0.3)
    raise AssertionError(f"任务未到终态:{last}")


def _store_path(sandbox) -> Path:
    """queue store 路径(`~/.zhuye/queue/<thread_id>.json`)。"""
    thread_id = rollout_thread_id(sandbox.home)
    return sandbox.home / ".zhuye" / "queue" / f"{thread_id}.json"


def test_goal任务入队并跑到底(serve_session, mock_script, sandbox) -> None:
    """add_goal(objective + verify + budget)→ start → 目标模式执行:
    EnterGoalMode 注入、verdict=met 判完成、任务结清 done。"""
    require_binary()
    _install_goal_queue_plugin(sandbox)
    # mock 脚本逐行被消费(主 turn 与 goal 校验共用):
    # 1. 主 turn:queue add_goal;2. 主 turn:queue start;3. 主 turn 收尾文本;
    # 4. goal 任务 objective turn 文本;5. goal 校验调用 → verdict=met。
    script = mock_script(
        [
            '{"type":"tool_call","name":"queue","args":{"action":"add_goal","objective":"写下答案42","verify_command":"true","token_budget":5000,"name":"g1"}}',
            '{"type":"tool_call","name":"queue","args":{"action":"start"}}',
            '{"type":"text","text":"排队完成"}',
            '{"type":"text","text":"答案就是 42"}',
            '{"type":"text","text":"{\\"verdict\\":\\"met\\",\\"evidence\\":[\\"回答包含42\\"]}"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "排队并开跑"}]},
            timeout=30.0,
        )
        ends = [
            e["msg"]
            for e in evs
            if e.get("msg", {}).get("type") == "tool_call_end" and not e["msg"].get("is_error")
        ]
        assert len(ends) >= 2, f"add_goal/start 应成功:{msg_types(evs)}"

        def _text(msg: dict) -> str:
            return "".join(
                b.get("text", "")
                for b in msg.get("output", {}).get("content", [])
                if isinstance(b, dict)
            )

        add_text = _text(ends[0])
        assert "goal 任务已入队" in add_text, f"add_goal 输出:{add_text}"
        assert "目标模式执行" in add_text, "输出应说明 goal 任务语义"
        m = re.search(r"goal 任务已入队:(\S+)", add_text)
        assert m, f"输出应含任务 id:{add_text}"
        task_id = m.group(1)
        # 会话存活期内等 goal 任务跑到底(目标模式 turn + 校验 + 结清)。
        task = _wait_status(_store_path(sandbox), task_id)

    # 字段级断言:落盘的 goal 载荷完整透传。
    assert task["status"] == "done", f"goal 任务应结清 done:{task}"
    assert task["note"] == "goal complete", f"结清 note 应标记目标完成:{task}"
    assert task["goal"]["objective"] == "写下答案42"
    assert task["goal"]["verify_command"] == "true"
    assert task["goal"]["token_budget"] == 5000
    assert task["name"] == "g1"


def test_多goal顺序执行(serve_session, mock_script, sandbox) -> None:
    """两个 goal 任务入队 → 严格串行:第一个 met 收尾后第二个才开跑,
    两个都结清 done(队列支持多 goal 的核心语义)。"""
    require_binary()
    _install_goal_queue_plugin(sandbox)
    # verifier 判 met 的回包(嵌套 JSON,外层是 mock 脚本行)。
    met_line = '{"type":"text","text":"{\\"verdict\\":\\"met\\",\\"evidence\\":[]}"}'
    script = mock_script(
        [
            '{"type":"tool_call","name":"queue","args":{"action":"add_goal","objective":"目标甲"}}',
            '{"type":"tool_call","name":"queue","args":{"action":"add_goal","objective":"目标乙"}}',
            '{"type":"tool_call","name":"queue","args":{"action":"start"}}',
            '{"type":"text","text":"已排队"}',
            # goal 甲:objective turn + verifier met。
            '{"type":"text","text":"甲完成"}',
            met_line,
            # goal 乙:objective turn + verifier met。
            '{"type":"text","text":"乙完成"}',
            met_line,
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "排两个目标"}]},
            timeout=30.0,
        )
        ends = [
            e["msg"]
            for e in evs
            if e.get("msg", {}).get("type") == "tool_call_end" and not e["msg"].get("is_error")
        ]
        assert len(ends) >= 3, f"两次 add_goal + start 应成功:{msg_types(evs)}"
        store = _store_path(sandbox)
        # 会话存活期内等两个 goal 都到终态(顺序执行:甲 done 后乙才开跑)。
        deadline = time.monotonic() + 40.0
        tasks: list[dict] = []
        while time.monotonic() < deadline:
            if store.exists():
                tasks = json.loads(store.read_text())["tasks"]
                if len(tasks) >= 2 and all(t["status"] in ("done", "failed") for t in tasks):
                    break
            time.sleep(0.3)

    assert len(tasks) == 2, f"应有两条 goal 任务:{tasks}"
    assert all(t["status"] == "done" for t in tasks), f"两条都应 done:{tasks}"
    assert all(t["note"] == "goal complete" for t in tasks), tasks
    first, second = tasks
    assert first["goal"]["objective"] == "目标甲"
    assert second["goal"]["objective"] == "目标乙"
    assert first["started_at"] <= second["started_at"], "目标甲必须先于目标乙开跑(严格串行)"
