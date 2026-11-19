#!/usr/bin/env python3
"""核心语义差分验证器 —— 新旧版本二进制在同一确定性输入上的行为对比。

定位(来自工作区 VERIFICATION_ENHANCEMENT_PLAN.md P1):

- **语义差分,而非全文 JSON 对比**:比较回合终态、回复文本、工具调用序列、
  审批请求、错误事件、文件副作用与 rollout 记录类型;session id、时间戳、
  耗时、临时路径等易变字段从不进入投影,无需逐字段归一化;
- **Protocol v0 兼容性**:候选新引入的事件类型必须逐条出现在用例的
  `approved_differences`(格式 `event:<TYPE>`)中,不得通过忽略全部未知
  字段来"兼容";基线有而候选丢失的事件类型一律视为分歧。rollout 记录
  类型走同口径的 `rollout:<TYPE>` 通道(见 `_compare_rollout_types`);
- **复用能力覆盖层夹具**:通过 `wire.ServeSession` + mock 模型脚本驱动真实
  `serve` 二进制,输入确定性,两侧结果可比。

模块分两层:

- 纯函数层:`project()`(事件流 → 语义投影)与 `compare()`(两份投影 +
  批准差异清单 → 分歧列表)。不触碰二进制,单测直接覆盖;
- 执行器层:`run_case()` / `run_all()` 驱动真实二进制,供
  `agent.contract.differential` 条件任务与命令行使用。

命令行:

    python3 tests/verification/differential.py \
        --baseline target/release/zhuye --candidate target/debug/zhuye \
        --output target/verification [--case <case_id>]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Callable

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests/functional"))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import wire  # noqa: E402  能力覆盖层原始 wire 夹具(ServeSession / Sandbox)

CASES_DIR = Path(__file__).with_name("cases")
INCIDENTS_DIR = Path(__file__).with_name("incidents")

# 已实现的场景 kind;新增 kind 必须同步实现 driver 并更新本集合。
KNOWN_KINDS = frozenset({
    "bash_readonly_gate",   # INC-001:Ask/Deny 压过只读 bash 捷径(12398067)
    "bash_redirect",        # INC-002:重定向写必须走审批(86f75e33)
    "write_default_mode",   # INC-003:默认档自动批准文件编辑(81776217)
    "crash_resume",         # INC-004:强杀后恢复已提交历史(5a4e8d54)
    "tool_error_recovery",  # INC-005:工具失败后的成功恢复(eval merge 语义)
})

# 终态事件类型与错误事件类型(与 wire.TERMINAL_TYPES 口径一致)。
TERMINAL = {"turn_complete", "turn_aborted"}
ERROR_TYPES = {"error", "stream_error"}

# 差分驱动超时:与能力覆盖层同量级,回归时快速失败而非挂死。
DRIVE_TIMEOUT = 40.0


# ── 纯函数层:投影与比较 ─────────────────────────────────────────────────


def project(events: list[dict[str, Any]], files: dict[str, str],
            rollout_types: list[str] | None = None) -> dict[str, Any]:
    """把事件流压缩为语义投影:只保留跨版本必须一致的判定信息。

    易变字段(submission id / 时间戳 / 耗时 / 临时路径)不进入投影;
    call_id 仅用于 begin/end 配对,配对后即丢弃。
    """
    turns = [{"terminal": e["msg"]["type"], "status": e["msg"].get("status")}
             for e in events if e["msg"].get("type") in TERMINAL]
    text = "".join(e["msg"].get("delta", "") for e in events
                   if e["msg"].get("type") == "agent_message_delta")
    tools: list[dict[str, Any]] = []
    for e in events:
        if e["msg"].get("type") == "tool_call_begin":
            tools.append({"call_id": e["msg"].get("call_id", ""),
                          "name": e["msg"].get("tool_name", ""), "is_error": None})
    end_by_call = {e["msg"].get("call_id"): e["msg"].get("is_error")
                   for e in events if e["msg"].get("type") == "tool_call_end"}
    for tool in tools:
        tool["is_error"] = end_by_call.get(tool["call_id"])
    tools = [{"name": tool["name"], "is_error": tool["is_error"]} for tool in tools]
    approvals = [e["msg"].get("kind", {}).get("tool_name", "")
                 for e in events if e["msg"].get("type") == "approval_request"]
    errors = [e["msg"].get("type") for e in events
              if e["msg"].get("type") in ERROR_TYPES]
    # collect 在回合终态返回,submission_closed 可能因接收线程调度而被附带。
    event_types = sorted({e["msg"].get("type", "") for e in events
                          if e["msg"].get("type") != "submission_closed"})
    projection: dict[str, Any] = {
        "turns": turns, "text": text, "tools": tools, "approvals": approvals,
        "errors": errors, "files": files, "event_types": event_types,
    }
    if rollout_types is not None:
        projection["rollout_types"] = rollout_types
    return projection


def compare(base: dict[str, Any], cand: dict[str, Any],
            approved: list[str] | None = None) -> list[str]:
    """比较两份语义投影,返回人类可读分歧列表(空列表 = 无分歧)。

    `approved` 中 `event:<TYPE>` 表示批准候选新增该事件类型,`rollout:<TYPE>`
    表示批准候选新增该 rollout 记录类型(Protocol v0 允许加变体);基线
    事件类型 / rollout 记录类型丢失始终是分歧,不接受批准豁免。
    """
    divergences: list[str] = []
    for label, key in (("回合终态", "turns"), ("回复文本", "text"),
                       ("工具调用序列", "tools"), ("审批请求序列", "approvals"),
                       ("错误事件", "errors"), ("文件副作用", "files")):
        if base.get(key) != cand.get(key):
            divergences.append(
                f"{label}不一致:baseline={json.dumps(base.get(key), ensure_ascii=False)}"
                f" candidate={json.dumps(cand.get(key), ensure_ascii=False)}")
    base_types = set(base.get("event_types", []))
    cand_types = set(cand.get("event_types", []))
    approved_set = set(approved or [])
    for new_type in sorted(cand_types - base_types):
        if f"event:{new_type}" not in approved_set:
            divergences.append(
                f"候选引入未批准的新事件类型:{new_type}"
                f"(如属 Protocol v0 合法新增,请将该用例 approved_differences 加入 \"event:{new_type}\")")
    for lost_type in sorted(base_types - cand_types):
        divergences.append(f"候选丢失基线事件类型:{lost_type}")
    divergences.extend(_compare_rollout_types(
        base.get("rollout_types"), cand.get("rollout_types"), approved_set))
    return divergences


def _compare_rollout_types(base: list[str] | None, cand: list[str] | None,
                            approved: set[str]) -> list[str]:
    """比较 rollout 记录类型序列(有序),返回分歧列表。

    与事件类型同口径:Protocol v0 只允许**加**记录类型,候选新增的记录
    类型必须逐条列入用例 `approved_differences`(格式 `rollout:<TYPE>`);
    基线有而候选丢失的记录类型一律是回归,不接受豁免。

    剔除已批准的新增类型后,剩余序列必须逐项一致 —— 顺序与条数都严格,
    避免「批准一个类型」变成「整条记录序列不校验」的旁路。
    """
    if base is None and cand is None:
        return []  # 非恢复类场景不含 rollout 投影
    base_seq = list(base or [])
    cand_seq = list(cand or [])
    divergences: list[str] = []
    base_set, cand_set = set(base_seq), set(cand_seq)
    # 候选新增且已批准的记录类型:从序列比对中剔除(它们本身不是分歧)。
    approved_new = {t for t in cand_set - base_set if f"rollout:{t}" in approved}
    for new_type in sorted(cand_set - base_set - approved_new):
        divergences.append(
            f"候选引入未批准的新 rollout 记录类型:{new_type}"
            f"(如属 Protocol v0 合法新增,请将该用例 approved_differences 加入 \"rollout:{new_type}\")")
    for lost_type in sorted(base_set - cand_set):
        divergences.append(f"候选丢失基线 rollout 记录类型:{lost_type}")
    stripped = [t for t in cand_seq if t not in approved_new]
    if stripped != base_seq:
        divergences.append(
            f"rollout 记录类型不一致:baseline={json.dumps(base_seq, ensure_ascii=False)}"
            f" candidate={json.dumps(cand_seq, ensure_ascii=False)}")
    return divergences


def check_case_oracle(case: dict[str, Any], projection: dict[str, Any]) -> list[str]:
    """独立校验事故预期,避免两版出现相同缺陷时误判通过。"""
    failures = []
    if case["kind"] == "bash_redirect" and case.get("decision", "deny") == "deny":
        if "bash" not in projection["approvals"]:
            failures.append("INC-002:缺少 bash 审批请求")
        if "out.txt" in projection["files"]:
            failures.append("INC-002:拒绝后仍产生 out.txt")
    return failures


def load_cases(case_dir: Path | None = None) -> list[dict[str, Any]]:
    """加载全部差分用例;格式或 kind 未知直接抛错,不静默跳过。"""
    directory = case_dir if case_dir is not None else CASES_DIR
    cases = []
    for path in sorted(directory.glob("*.json")):
        case = json.loads(path.read_text(encoding="utf-8"))
        validate_case(case)
        cases.append(case)
    if not cases:
        raise ValueError(f"差分用例目录为空:{directory}")
    return cases


def validate_case(case: dict[str, Any]) -> None:
    """用例结构校验:字段齐全、kind 已知、批准差异格式合法。"""
    for field in ("id", "kind", "description", "mock_script"):
        if field not in case:
            raise ValueError(f"差分用例缺少字段 {field}:{case}")
    if case["kind"] not in KNOWN_KINDS:
        raise ValueError(f"未知场景 kind:{case['kind']}(已知:{sorted(KNOWN_KINDS)})")
    if not isinstance(case["mock_script"], list) or not case["mock_script"]:
        raise ValueError(f"mock_script 必须为非空列表:{case.get('id')}")
    for item in case.get("approved_differences", []):
        if not (item.startswith("event:") or item.startswith("rollout:")):
            raise ValueError(
                f"approved_differences 只支持 event:<TYPE> / rollout:<TYPE> 格式:{item}")


# ── 执行器层:驱动真实二进制 ─────────────────────────────────────────────


def _make_sandbox(root: Path) -> wire.Sandbox:
    """隔离 HOME + cwd;最小配置关闭全部内置 hook(与能力覆盖层口径一致)。"""
    home, cwd = root / "home", root / "cwd"
    (home / ".zhuye").mkdir(parents=True)
    cwd.mkdir()
    (home / ".zhuye" / "config.toml").write_text(
        "# 差分验证用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n",
        encoding="utf-8")
    return wire.Sandbox(home=home, cwd=cwd)


def snapshot_files(cwd: Path) -> dict[str, str]:
    """工作区最终快照(相对路径 → 文本内容),排除 wire.py 的诊断残留。"""
    files: dict[str, str] = {}
    for path in sorted(cwd.rglob("*")):
        if not path.is_file() or path.is_symlink():
            continue
        rel = path.relative_to(cwd).as_posix()
        if rel.startswith("serve-"):
            continue  # wire.ServeSession 的 stderr / transcript 文件
        try:
            files[rel] = path.read_text(encoding="utf-8")
        except UnicodeError:
            files[rel] = "<binary>"
    return files


def _wait_rollout_contains(home: Path, needle: str, timeout: float = 10.0) -> None:
    """等首轮文本落盘进 rollout(崩溃恢复场景的前置条件)。"""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if needle in json.dumps(wire.rollout_records(home), ensure_ascii=False):
            return
        time.sleep(0.05)
    raise AssertionError(f"{timeout}s 内首轮检查点未落盘:{needle!r}")


def _drive_bash_readonly_gate(case: dict, sandbox: wire.Sandbox,
                              env: dict, events: list) -> None:
    """三种工作模式 × 只读 bash 捷径(INC-001):Ask 必问、Deny 必拒、Auto 放行。"""
    for mode in ("prompt", "deny", "auto"):
        with wire.ServeSession(sandbox, env=env, timeout=DRIVE_TIMEOUT) as session:
            session.wait_global(lambda e: e["msg"].get("type") == "session_configured",
                                timeout=DRIVE_TIMEOUT)
            session.submit({"type": "set_permission_mode", "mode": mode})
            if mode == "prompt":
                turn = session.send({"type": "user_input",
                                     "items": [{"type": "text", "text": "跑"}]})
                request = session.wait_event(turn, "approval_request",
                                             timeout=DRIVE_TIMEOUT)["msg"]
                session.send({"type": "tool_approval",
                              "id": request["request_id"], "decision": "approve"})
                events.extend(session.collect(turn, timeout=DRIVE_TIMEOUT))
            else:
                events.extend(session.submit(
                    {"type": "user_input", "items": [{"type": "text", "text": "跑"}]},
                    timeout=DRIVE_TIMEOUT))


def _drive_bash_redirect(case: dict, sandbox: wire.Sandbox,
                         env: dict, events: list) -> None:
    """Ask 档下 bash 重定向写必须弹审批;拒绝后不得产生文件(INC-002)。"""
    decision = case.get("decision", "deny")
    with wire.ServeSession(sandbox, env=env, timeout=DRIVE_TIMEOUT) as session:
        session.wait_global(lambda e: e["msg"].get("type") == "session_configured",
                            timeout=DRIVE_TIMEOUT)
        session.submit({"type": "set_permission_mode", "mode": "prompt"})

        def _on_event(ev: dict) -> None:
            msg = ev.get("msg", {})
            if msg.get("type") == "approval_request":
                payload = {"deny": {"reason": "差分验证拒绝"}} if decision == "deny" else "approve"
                session.send({"type": "tool_approval",
                              "id": msg["request_id"], "decision": payload})

        events.extend(session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "跑"}]},
            timeout=DRIVE_TIMEOUT, on_event=_on_event))


def _drive_write_default_mode(case: dict, sandbox: wire.Sandbox,
                              env: dict, events: list) -> None:
    """默认档(Auto)自动批准文件编辑,不弹审批(INC-003)。"""
    with wire.ServeSession(sandbox, env=env, timeout=DRIVE_TIMEOUT) as session:
        session.wait_global(lambda e: e["msg"].get("type") == "session_configured",
                            timeout=DRIVE_TIMEOUT)
        events.extend(session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "写"}]},
            timeout=DRIVE_TIMEOUT))


def _drive_crash_resume(case: dict, sandbox: wire.Sandbox,
                        env: dict, events: list) -> None:
    """首轮完成后强杀进程,--resume 恢复历史并继续(INC-004)。"""
    with wire.ServeSession(sandbox, env=env, timeout=DRIVE_TIMEOUT) as session:
        session.wait_global(lambda e: e["msg"].get("type") == "session_configured",
                            timeout=DRIVE_TIMEOUT)
        events.extend(session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "记住第一轮"}]},
            timeout=DRIVE_TIMEOUT))
        _wait_rollout_contains(sandbox.home, case["checkpoint"])
        thread_id = wire.rollout_thread_id(sandbox.home)
        session.kill()
        session._proc.wait(timeout=5)
    with wire.ServeSession(sandbox, args=["--resume", thread_id],
                           env=env, timeout=DRIVE_TIMEOUT) as resumed:
        resumed.wait_global(lambda e: e["msg"].get("type") == "session_configured",
                            timeout=DRIVE_TIMEOUT)
        events.extend(resumed.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "恢复后继续"}]},
            timeout=DRIVE_TIMEOUT))


def _drive_tool_error_recovery(case: dict, sandbox: wire.Sandbox,
                               env: dict, events: list) -> None:
    """工具失败(read 缺失文件)后,同回合内恢复并完成写(INC-005)。"""
    with wire.ServeSession(sandbox, env=env, timeout=DRIVE_TIMEOUT) as session:
        session.wait_global(lambda e: e["msg"].get("type") == "session_configured",
                            timeout=DRIVE_TIMEOUT)
        events.extend(session.submit(
            {"type": "user_input",
             "items": [{"type": "text", "text": "读取 missing.json,不存在则写入 result.json"}]},
            timeout=DRIVE_TIMEOUT))


DRIVERS: dict[str, Callable[[dict, wire.Sandbox, dict, list], None]] = {
    "bash_readonly_gate": _drive_bash_readonly_gate,
    "bash_redirect": _drive_bash_redirect,
    "write_default_mode": _drive_write_default_mode,
    "crash_resume": _drive_crash_resume,
    "tool_error_recovery": _drive_tool_error_recovery,
}


def run_case(case: dict[str, Any], binary: Path, out_dir: Path | None = None) -> dict[str, Any]:
    """对单个二进制执行一个差分场景,返回语义投影与原始证据。

    在独立临时沙箱内运行,不污染宿主环境;被测二进制通过 `wire.ZHUYE_BIN`
    注入(ServeSession 构造时读取)。原始事件与投影写入 out_dir(若提供)。
    """
    wire.ZHUYE_BIN = binary
    with tempfile.TemporaryDirectory(prefix="zhuye-diff-") as tmp:
        root = Path(tmp)
        sandbox = _make_sandbox(root)
        # mock 脚本放沙箱外,不污染工作区快照。
        script_path = root / "mock-script.jsonl"
        script_path.write_text("\n".join(case["mock_script"]) + "\n", encoding="utf-8")
        env = sandbox.mock_env(script_path, ZHUYE_APPROVALS="1")
        for setup_file, content in case.get("setup_files", {}).items():
            (sandbox.cwd / setup_file).write_text(content, encoding="utf-8")
        events: list[dict[str, Any]] = []
        DRIVERS[case["kind"]](case, sandbox, env, events)
        files = snapshot_files(sandbox.cwd)
        rollout = None
        if case["kind"] == "crash_resume":
            rollout = [record.get("type", "") for record in wire.rollout_records(sandbox.home)]
        projection = project(events, files, rollout)
        if out_dir is not None:
            target = out_dir / case["id"]
            target.mkdir(parents=True, exist_ok=True)
            (target / "events.json").write_text(
                json.dumps(events, ensure_ascii=False, indent=2), encoding="utf-8")
            (target / "projection.json").write_text(
                json.dumps(projection, ensure_ascii=False, indent=2), encoding="utf-8")
        return projection


def run_all(baseline: Path, candidate: Path, case_ids: list[str] | None,
            output: Path) -> int:
    """对全部(或指定)用例执行两侧差分,写报告并返回退出码。"""
    cases = load_cases()
    if case_ids:
        unknown = set(case_ids) - {c["id"] for c in cases}
        if unknown:
            print(f"未知用例:{','.join(sorted(unknown))}", file=sys.stderr)
            return 3
        cases = [c for c in cases if c["id"] in set(case_ids)]
    output.mkdir(parents=True, exist_ok=True)
    report: dict[str, Any] = {
        "baseline": {"path": str(baseline),
                     "sha256": hashlib.sha256(baseline.read_bytes()).hexdigest()},
        "candidate": {"path": str(candidate),
                      "sha256": hashlib.sha256(candidate.read_bytes()).hexdigest()},
        "results": [], "verdict": "pass",
    }
    has_infra = False
    for case in cases:
        print(f"差分用例:{case['id']}({case['kind']})", flush=True)
        try:
            base_proj = run_case(case, baseline, output / "baseline")
            cand_proj = run_case(case, candidate, output / "candidate")
            divergences = compare(base_proj, cand_proj, case.get("approved_differences"))
            divergences.extend(check_case_oracle(case, cand_proj))
            entry = {"id": case["id"], "kind": case["kind"], "verdict":
                     "pass" if not divergences else "divergence",
                     "divergences": divergences}
        except (OSError, AssertionError, subprocess.SubprocessError) as exc:
            entry = {"id": case["id"], "kind": case["kind"],
                     "verdict": "infra_error", "detail": str(exc)}
            has_infra = True
        report["results"].append(entry)
        for divergence in entry.get("divergences", []):
            print(f"  - {divergence}")
        if entry["verdict"] == "infra_error":
            print(f"  - 基础设施错误:{entry.get('detail')}")
    if any(r["verdict"] == "divergence" for r in report["results"]):
        report["verdict"] = "divergence"
    if has_infra and report["verdict"] == "pass":
        report["verdict"] = "infra_error"
    (output / "report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"差分结论:{report['verdict']}")
    if report["verdict"] == "infra_error":
        return 2
    if report["verdict"] == "divergence":
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="新旧版本二进制的核心语义差分验证")
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--case", action="append", dest="cases", default=None,
                        help="只跑指定用例(可重复);默认全部")
    parser.add_argument("--output", type=Path,
                        default=ROOT / "target/verification")
    args = parser.parse_args()
    for label, path in (("baseline", args.baseline), ("candidate", args.candidate)):
        if not path.is_file() or not path.resolve().is_file():
            print(f"{label} 二进制不存在:{path}", file=sys.stderr)
            return 3
    return run_all(args.baseline.resolve(), args.candidate.resolve(),
                   args.cases, args.output.resolve())


if __name__ == "__main__":
    sys.exit(main())
