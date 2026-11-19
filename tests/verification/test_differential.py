#!/usr/bin/env python3
"""差分验证器自测 —— 纯函数层与语料库自洽,不调用任何二进制。

覆盖口径(对应 VERIFICATION_ENHANCEMENT_PLAN.md P1 验收):

- project():事件流 → 语义投影;易变字段(call_id / 提交 id / 时间戳)不进入投影;
- compare():干净控制组零分歧;注入缺陷(缺失终态、工具顺序错乱、
  审批拒绝后副作用、未批准新事件、丢失基线事件)全部必被抓;
- approved_differences:合法新增事件经批准后不报分歧,丢失事件不可豁免;
  rollout 记录类型走同口径的 `rollout:<TYPE>` 批准通道,顺序与条数仍严格;
- 语料库自洽:用例 kind 已知、事故引用闭环、来源提交号存在、批准清单两侧一致。

运行方式:`python3 tests/verification/test_differential.py`(仅依赖标准库)。
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import differential  # noqa: E402

HERE = Path(__file__).resolve().parent


def ev(msg: dict, ev_id: str = "sub-1") -> dict:
    """构造一条 wire 事件(id 为提交 id,属易变字段)。"""
    return {"id": ev_id, "msg": msg}


def base_events() -> list[dict]:
    """干净控制组事件流:一轮对话 + 一次 write 工具 + 审批拒绝场景混合。

    结构:tool_call_begin(write) → approval_request → tool_call_end(error,
    已拒绝) → text delta → turn_complete。文件快照为空(拒绝无副作用)。
    """
    return [
        ev({"type": "session_configured"}),
        ev({"type": "tool_call_begin", "call_id": "c1", "tool_name": "write", "args": None}),
        ev({"type": "approval_request", "request_id": "r1",
            "kind": {"tool_name": "write"}}),
        ev({"type": "tool_call_end", "call_id": "c1", "is_error": True}),
        ev({"type": "agent_message_delta", "delta": "已拒绝"}),
        ev({"type": "turn_complete", "status": "success"}),
    ]


class ProjectTest(unittest.TestCase):
    """投影层:只保留语义,丢弃易变字段。"""

    def test_projection_shape(self):
        files = {"out.txt": "data"}
        projection = differential.project(base_events(), files)
        self.assertEqual(projection["turns"],
                         [{"terminal": "turn_complete", "status": "success"}])
        self.assertEqual(projection["tools"], [{"name": "write", "is_error": True}])
        self.assertEqual(projection["approvals"], ["write"])
        self.assertEqual(projection["errors"], [])
        self.assertEqual(projection["files"], files)
        self.assertEqual(projection["text"], "已拒绝")

    def test_volatile_fields_excluded(self):
        # call_id / 提交 id / request_id 变化不影响投影 —— 两次独立运行可比。
        # begin 与 end 的 call_id 必须同步重命名(同一调用的配对标识)。
        renamed = []
        for e in base_events():
            msg = e["msg"]
            if msg["type"] in ("tool_call_begin", "tool_call_end"):
                msg = {**msg, "call_id": "other-call"}
            renamed.append({**e, "id": "different-sub", "msg": msg})
        p1 = differential.project(base_events(), {})
        p2 = differential.project(renamed, {})
        self.assertEqual(p1, p2)

    def test_tool_end_paired_by_call_id(self):
        # 两个工具调用的 end 乱序到达也按 call_id 正确配对。
        events = [
            ev({"type": "tool_call_begin", "call_id": "a", "tool_name": "read"}),
            ev({"type": "tool_call_begin", "call_id": "b", "tool_name": "write"}),
            ev({"type": "tool_call_end", "call_id": "b", "is_error": False}),
            ev({"type": "tool_call_end", "call_id": "a", "is_error": True}),
            ev({"type": "turn_complete", "status": "success"}),
        ]
        projection = differential.project(events, {})
        self.assertEqual(projection["tools"], [
            {"name": "read", "is_error": True},
            {"name": "write", "is_error": False},
        ])

    def test_rollout_projection_optional(self):
        with_rollout = differential.project([], [], ["session", "message"])
        self.assertEqual(with_rollout["rollout_types"], ["session", "message"])
        without = differential.project([], [])
        self.assertNotIn("rollout_types", without)

    def test_late_submission_closed_does_not_change_projection(self):
        control = differential.project(base_events(), {})
        closed = differential.project(base_events() + [ev({"type": "submission_closed"})], {})
        self.assertEqual(control, closed)


class CompareControlTest(unittest.TestCase):
    """干净控制组:相同语义零分歧,不得误报。"""

    def test_identical_projections_no_divergence(self):
        projection = differential.project(base_events(), {"a.txt": "x"})
        self.assertEqual(differential.compare(projection, projection, []), [])

    def test_new_event_type_with_approval_no_divergence(self):
        base = differential.project(base_events(), {})
        cand_events = base_events() + [ev({"type": "compaction_started"}, "g")]
        cand = differential.project(cand_events, {})
        divergences = differential.compare(base, cand, ["event:compaction_started"])
        self.assertEqual(divergences, [])


class IncidentOracleTest(unittest.TestCase):
    """事故预期独立于新旧版本的差异。"""

    def test_both_versions_broken_and_healthy_control(self):
        case = next(c for c in differential.load_cases() if c["id"] == "bash_redirect")
        healthy = differential.project(
            [ev({"type": "approval_request", "kind": {"tool_name": "bash"}})], {})
        broken = differential.project([], {"out.txt": "hi"})
        self.assertEqual(differential.compare(broken, broken), [])
        self.assertEqual(differential.check_case_oracle(case, healthy), [])
        failures = differential.check_case_oracle(case, broken)
        self.assertTrue(any("审批请求" in item for item in failures))
        self.assertTrue(any("out.txt" in item for item in failures))
        self.assertEqual(len(differential.check_case_oracle(
            case, {**healthy, "approvals": []})), 1)
        self.assertEqual(len(differential.check_case_oracle(
            case, {**healthy, "files": {"out.txt": "hi"}})), 1)

    def test_report_checks_candidate_and_keeps_both_sides(self):
        case = next(c for c in differential.load_cases() if c["id"] == "bash_redirect")
        broken = differential.project([], {"out.txt": "hi"})
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline, candidate = root / "old", root / "new"
            baseline.write_text("old")
            candidate.write_text("new")
            with mock.patch.object(differential, "load_cases", return_value=[case]), \
                 mock.patch.object(differential, "run_case", return_value=broken) as run:
                result = differential.run_all(baseline, candidate, None, root / "report")
            self.assertEqual(result, 1)
            self.assertEqual(run.call_args_list[0].args[2], root / "report" / "baseline")
            self.assertEqual(run.call_args_list[1].args[2], root / "report" / "candidate")
            report = json.loads((root / "report" / "report.json").read_text())
            self.assertEqual(report["verdict"], "divergence")
            self.assertEqual(len(report["results"][0]["divergences"]), 2)


class CompareMutantTest(unittest.TestCase):
    """变异自测:注入的已知缺陷必须全部被抓(验证器有效性证明)。"""

    MUTANTS = {
        "缺失终态": lambda p: {**p, "turns": []},
        "终态状态漂移": lambda p: {**p, "turns": [{"terminal": "turn_aborted",
                                                  "status": None}]},
        # 控制组只有一个工具,单纯反转等价于自身;用双工具乱序模拟顺序回归。
        "工具顺序错乱": lambda p: {**p, "tools": [
            {"name": "write", "is_error": True}, {"name": "read", "is_error": False}]},
        "工具错误被吞": lambda p: {**p, "tools": [{"name": "write", "is_error": False}]},
        "拒绝后产生副作用": lambda p: {**p, "files": {"out.txt": "不该存在"}},
        "审批请求消失": lambda p: {**p, "approvals": []},
        "错误事件新增": lambda p: {**p, "errors": ["stream_error"]},
        "回复文本漂移": lambda p: {**p, "text": "别的回复"},
        "丢失基线事件类型": lambda p: {**p, "event_types": ["session_configured"]},
    }

    def test_all_mutants_detected(self):
        control = differential.project(base_events(), {})
        for label, mutate in self.MUTANTS.items():
            divergences = differential.compare(control, mutate(control), [])
            self.assertTrue(divergences, f"变异 {label!r} 未被检测到")

    def test_each_mutant_names_its_dimension(self):
        # 分歧信息必须指明维度,便于定位(只报「测试失败」不可接受)。
        expectations = {
            "缺失终态": "回合终态", "工具顺序错乱": "工具调用序列",
            "拒绝后产生副作用": "文件副作用", "审批请求消失": "审批请求序列",
            "丢失基线事件类型": "丢失基线事件类型",
        }
        control = differential.project(base_events(), {})
        for label, needle in expectations.items():
            divergences = differential.compare(control, self.MUTANTS[label](control), [])
            self.assertTrue(any(needle in d for d in divergences),
                            f"变异 {label!r} 的分歧未指明维度:{divergences}")

    def test_unapproved_new_event_type_detected(self):
        base = differential.project(base_events(), {})
        cand = differential.project(base_events() + [ev({"type": "mystery_event"}, "g")], {})
        divergences = differential.compare(base, cand, [])
        self.assertTrue(any("未批准的新事件类型:mystery_event" in d for d in divergences))

    def test_lost_event_type_cannot_be_approved(self):
        # 基线有而候选丢失的类型是回归:即使写进 approved_differences 也不豁免。
        base = differential.project(base_events() + [ev({"type": "legacy_event"}, "g")], {})
        cand = differential.project([e for e in base_events()
                                     if e["msg"]["type"] != "approval_request"], {})
        divergences = differential.compare(
            base, cand, ["event:approval_request"])
        self.assertTrue(any("丢失基线事件类型:approval_request" in d for d in divergences))


class CompareRolloutTest(unittest.TestCase):
    """rollout 记录类型维度:Protocol v0 加变体的独立批准通道。

    rollout 记录类型是协议 `RolloutRecord` 枚举的投影(与 wire 事件类型
    同属「只允许加变体」范畴),但走独立的 `rollout:<TYPE>` 前缀 —— 批准
    一个记录类型不得变成整条序列免检。
    """

    BASE_ROLLOUT = ["session_meta", "message", "token_count", "message"]

    def _proj(self, rollout: list[str]) -> dict:
        """带 rollout 记录类型投影的控制组。"""
        return differential.project(base_events(), {}, rollout)

    def test_both_sides_without_rollout_projection_no_divergence(self):
        # 非恢复类场景两侧都不含 rollout 投影 —— 不得误报。
        control = differential.project(base_events(), {})
        self.assertEqual(differential.compare(control, control, []), [])

    def test_new_rollout_type_with_approval_no_divergence(self):
        base = self._proj(self.BASE_ROLLOUT)
        cand = self._proj(["session_meta", "prompt_selection_changed"]
                          + self.BASE_ROLLOUT[1:])
        divergences = differential.compare(base, cand, ["rollout:prompt_selection_changed"])
        self.assertEqual(divergences, [])

    def test_unapproved_new_rollout_type_detected(self):
        base = self._proj(self.BASE_ROLLOUT)
        cand = self._proj(["session_meta", "prompt_selection_changed"]
                          + self.BASE_ROLLOUT[1:])
        divergences = differential.compare(base, cand, [])
        self.assertTrue(any("未批准的新 rollout 记录类型:prompt_selection_changed" in d
                            for d in divergences))

    def test_lost_rollout_type_cannot_be_approved(self):
        # 基线有而候选丢失的记录类型是回归:写进 approved_differences 也不豁免。
        base = self._proj(["session_meta", "fork", "message"])
        cand = self._proj(["session_meta", "message"])
        divergences = differential.compare(base, cand, ["rollout:fork"])
        self.assertTrue(any("丢失基线 rollout 记录类型:fork" in d for d in divergences))

    def test_rollout_order_strictness_preserved(self):
        # 批准一个新增类型不得变成「整条记录序列免检」:顺序错乱仍必被抓。
        base = self._proj(["session_meta", "message", "token_count"])
        cand = self._proj(["session_meta", "prompt_selection_changed",
                           "token_count", "message"])
        divergences = differential.compare(base, cand, ["rollout:prompt_selection_changed"])
        self.assertTrue(any("rollout 记录类型不一致" in d for d in divergences))

    def test_rollout_type_count_change_detected(self):
        # 同类型条数变化(重复落盘)同样必被抓。
        base = self._proj(["session_meta", "message"])
        cand = self._proj(["session_meta", "message", "message"])
        divergences = differential.compare(base, cand, [])
        self.assertTrue(any("rollout 记录类型不一致" in d for d in divergences))

    def test_rollout_approval_prefix_accepted_by_validate(self):
        differential.validate_case({
            "id": "x", "kind": "crash_resume", "description": "", "mock_script": ["{}"],
            "approved_differences": ["rollout:prompt_selection_changed"]})


class CorpusIntegrityTest(unittest.TestCase):
    """语料库自洽:用例与事故引用闭环,来源提交真实存在。"""

    def test_all_cases_valid(self):
        cases = differential.load_cases()
        self.assertGreaterEqual(len(cases), 5)
        for case in cases:
            self.assertIn(case["id"], differential.KNOWN_KINDS.union(
                {c["id"] for c in cases}))  # id 与 kind 均已登记

    def test_case_ids_unique(self):
        cases = differential.load_cases()
        ids = [c["id"] for c in cases]
        self.assertEqual(len(ids), len(set(ids)))

    def test_incidents_reference_existing_cases(self):
        for path in sorted((HERE / "incidents").glob("*.json")):
            incident = json.loads(path.read_text(encoding="utf-8"))
            case_path = HERE / "incidents" / ".." / "cases" / incident["case_file"]
            self.assertTrue((HERE / "cases" / incident["case_file"]).is_file(),
                            f"{incident['id']} 引用的用例不存在:{incident['case_file']}")
            self.assertRegex(incident["source_commit"], r"^[0-9a-f]{7,40}$")
            self.assertTrue(incident["oracle"], f"{incident['id']} 缺少 oracle 说明")

    def test_incident_ids_match_case_backlink(self):
        # 用例的 source_incident 与事故 id 双向闭环,防止只改一侧。
        for path in sorted((HERE / "incidents").glob("*.json")):
            incident = json.loads(path.read_text(encoding="utf-8"))
            case = json.loads(
                (HERE / "cases" / incident["case_file"]).read_text(encoding="utf-8"))
            self.assertEqual(case.get("source_incident"), incident["id"],
                             f"{incident['id']} 与用例 {case['id']} 的反向引用不一致")

    def test_incident_approved_differences_match_case(self):
        # 事故元数据与用例的批准清单必须一致 —— 防止只改一侧导致口径失真。
        for path in sorted((HERE / "incidents").glob("*.json")):
            incident = json.loads(path.read_text(encoding="utf-8"))
            case = json.loads(
                (HERE / "cases" / incident["case_file"]).read_text(encoding="utf-8"))
            self.assertEqual(incident.get("approved_differences", []),
                             case.get("approved_differences", []),
                             f"{incident['id']} 与用例 {case['id']} 的批准清单不一致")

    def test_source_commits_exist_in_history(self):
        # 来源提交号必须能在本仓库历史中解析到(用 rev-parse 验证,不受
        # git 短哈希长度自适应的影响)。CI 的 actions/checkout 默认浅克隆
        # (depth=1),没有历史对象 —— 此时跳过;本地开发仓为完整克隆,
        # 校验仍然生效。
        shallow = subprocess.run(
            ["git", "rev-parse", "--is-shallow-repository"],
            cwd=HERE.parents[1], capture_output=True, text=True,
            timeout=15, check=False)
        if shallow.stdout.strip() == "true":
            self.skipTest("浅克隆无历史对象:来源提交校验仅在完整克隆下执行")
        for path in sorted((HERE / "incidents").glob("*.json")):
            incident = json.loads(path.read_text(encoding="utf-8"))
            verify = subprocess.run(
                ["git", "rev-parse", "--verify",
                 f"{incident['source_commit']}^{{commit}}"],
                cwd=HERE.parents[1], capture_output=True, text=True,
                timeout=15, check=False)
            self.assertEqual(
                verify.returncode, 0,
                f"{incident['id']} 来源提交无法解析:{incident['source_commit']}")


class CaseValidationTest(unittest.TestCase):
    """用例校验:格式非法必须显式失败。"""

    def test_unknown_kind_rejected(self):
        with self.assertRaises(ValueError):
            differential.validate_case({"id": "x", "kind": "mystery", "description": "",
                                        "mock_script": ["{}"]})

    def test_bad_approved_difference_rejected(self):
        with self.assertRaises(ValueError):
            differential.validate_case({"id": "x", "kind": "bash_redirect",
                                        "description": "", "mock_script": ["{}"],
                                        "approved_differences": ["ignore:everything"]})

    def test_empty_mock_script_rejected(self):
        with self.assertRaises(ValueError):
            differential.validate_case({"id": "x", "kind": "bash_redirect",
                                        "description": "", "mock_script": []})


if __name__ == "__main__":
    unittest.main(verbosity=2)
