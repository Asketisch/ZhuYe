#!/usr/bin/env python3
"""固定任务集的真实 agent 评测:执行、客观验收、评分和版本比较。"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests/functional"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import wire  # noqa: E402
import eval_policy  # noqa: E402  质量门禁策略判定层(与本文件同目录)

CASES = Path(__file__).with_name("cases.json")


def redact_key(text: str, key: str) -> str:
    """遮盖原始凭据及其 JSON 编码,避免轨迹泄漏。"""
    if not key:
        return text
    for value in (key, json.dumps(key, ensure_ascii=False)[1:-1]):
        text = text.replace(value, "[已隐藏]")
    return text


class EvaluationInfrastructureError(Exception):
    """评测运行所需的系统隔离不可用。"""


def sandbox_command(workspace: Path, command: list[str]) -> list[str]:
    """对模型生成的程序强制系统沙箱,禁止网络和工作区外写入。"""
    workspace = workspace.resolve()
    host_home = Path.home().resolve()
    if workspace == host_home or host_home in workspace.parents:
        raise EvaluationInfrastructureError("评测临时工作区不能位于宿主 HOME 内")
    if sys.platform == "darwin" and Path("/usr/bin/sandbox-exec").is_file():
        profile = ('(version 1)(allow default)(deny network*)(deny process-fork)'
                   '(deny file-write*)(allow file-write* (subpath '
                   + json.dumps(str(workspace)) + '))(allow file-write* (literal "/dev/null"))'
                   '(deny file-read* (subpath ' + json.dumps(str(host_home)) + '))')
        return ["/usr/bin/sandbox-exec", "-p", profile, *command]
    if sys.platform.startswith("linux"):
        bubblewrap = shutil.which("bwrap")
        if bubblewrap:
            return [bubblewrap, "--unshare-all", "--die-with-parent", "--new-session", "--ro-bind", "/", "/",
                    "--tmpfs", str(host_home), "--bind", str(workspace), str(workspace),
                    "--chdir", str(workspace), "--", *command]
    raise EvaluationInfrastructureError("缺少可用的系统沙箱(macOS sandbox-exec / Linux bubblewrap)")


def ensure_sandbox(workspace: Path) -> None:
    """在产生模型费用前验证沙箱后端确实可启动。"""
    try:
        subprocess.run(sandbox_command(workspace, [sys.executable, "-I", "-c", "pass"]),
                       cwd=workspace, env={"HOME": str(workspace), "PATH": os.defpath},
                       capture_output=True, timeout=15, check=True)
    except (OSError, subprocess.SubprocessError) as exc:
        raise EvaluationInfrastructureError(f"系统沙箱启动失败:{exc}") from exc


def prepare(case: dict, workspace: Path) -> tuple[str, dict]:
    """生成合成任务和仅由外部验证器使用的期望值。"""
    kind = case["kind"]
    if kind == "repair":
        expressions = {"add": "a + b", "subtract": "a - b", "multiply": "a * b", "maximum": "max(a, b)"}
        op = case["operation"]
        (workspace / "calc.py").write_text("def calculate(a, b):\n    return 0\n")
        (workspace / "protected.txt").write_text("不可更改\n")
        prompt = f"修复 calc.py 的 calculate(a, b)，使它返回 {expressions[op]}。请只修改 calc.py，并简要说明。"
        return prompt, {"operation": op, "inputs": case["inputs"]}
    if kind == "merge":
        for index, value in enumerate(case["values"]):
            (workspace / f"part{index + 1}.json").write_text(json.dumps({"value": value}))
        (workspace / "protected.txt").write_text("不可更改\n")
        prompt = (f"先尝试读取 missing.json；如果文件不存在，继续读取 part1.json、part2.json、part3.json，将它们的 value 相加，"
                  f"写入 result.json，包含 label={case['label']} 和 sum 两个字段。"
                  "不要修改已有文件。请使用工具完成并简要说明。")
        return prompt, {"label": case["label"], "sum": sum(case["values"])}
    if kind == "resume":
        wanted = {key: case[key] for key in ("code", "color", "priority")}
        prompt = ("请记住之后要写入 status.json 的项目参数:" + json.dumps(wanted, ensure_ascii=False)
                  + "。现在仅确认已记住，不要创建文件。\n背景资料:" + "项目背景资料用于评测上下文压缩。" * 350)
        return prompt, wanted
    raise ValueError(f"未知任务类型:{kind}")


def verify(case: dict, workspace: Path, expected: dict) -> tuple[bool, str]:
    """用外部测试和文件状态判定任务,不相信模型的自述。"""
    try:
        if case["kind"] == "repair":
            probe = ("import importlib.util,json,sys; sys.dont_write_bytecode=True; "
                     "s=importlib.util.spec_from_file_location('subject',sys.argv[1]); "
                     "m=importlib.util.module_from_spec(s); s.loader.exec_module(m); "
                     "print(json.dumps([m.calculate(*x) for x in json.loads(sys.argv[2])]))")
            command = [sys.executable, "-I", "-c", probe, str(workspace / "calc.py"), json.dumps(expected["inputs"])]
            clean_env = {"HOME": str(workspace), "PATH": os.defpath}
            ensure_sandbox(workspace)
            proc = subprocess.run(sandbox_command(workspace, command),
                                  cwd=workspace, env=clean_env,
                                  capture_output=True, text=True, timeout=15, check=True)
            operations = {"add": lambda a, b: a + b, "subtract": lambda a, b: a - b,
                          "multiply": lambda a, b: a * b, "maximum": max}
            values = [operations[expected["operation"]](*item) for item in expected["inputs"]]
            ok = json.loads(proc.stdout) == values and (workspace / "protected.txt").read_text() == "不可更改\n"
            return ok, "独立计算断言通过" if ok else "计算结果不符"
        target = workspace / ("result.json" if case["kind"] == "merge" else "status.json")
        ok = json.loads(target.read_text()) == expected
        if case["kind"] == "merge":
            ok = ok and (workspace / "protected.txt").read_text() == "不可更改\n"
            ok = ok and all(json.loads((workspace / f"part{i + 1}.json").read_text()) == {"value": value}
                            for i, value in enumerate(case["values"]))
        return ok, "产物及受保护文件断言通过" if ok else "产物或受保护文件不符"
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        return False, f"验收失败:{exc}"


def events_text(events: list[dict]) -> str:
    texts: list[str] = []
    current = ""
    for item in events:
        msg = item.get("msg", {})
        if msg.get("type") == "turn_started":
            if current:
                texts.append(current)
            current = ""
        elif msg.get("type") == "agent_message_delta":
            current += msg.get("delta", "")
        elif msg.get("type") == "agent_message":
            current = msg.get("text", "")
    return "".join([*texts, current])


def run_one(case: dict, binary: Path, model: str, base_url: str, key: str, destination: Path) -> dict:
    """在独立 HOME 和工作区执行一条用例,保存脱敏事件轨迹。"""
    start = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="zhuye-eval-") as tmp:
        root = Path(tmp)
        home, workspace = root / "home", root / "work"
        (home / ".zhuye").mkdir(parents=True)
        workspace.mkdir()
        config = ('[hooks]\nenabled = []\n[active]\nprovider = "openai"\nmax_iterations = 30\n[openai]\n'
                  f'api_key = {json.dumps(key)}\nbase_url = {json.dumps(base_url)}\n'
                  'timeout_secs = 300\n')
        (home / ".zhuye/config.toml").write_text(config)
        prompt, expected = prepare(case, workspace)
        sandbox = wire.Sandbox(home, workspace)
        env = {"ZHUYE_MODEL": f"openai/{model}", "ZHUYE_AUTO_COMPACT_INPUT_TOKENS": "1000"}
        wire.ZHUYE_BIN = binary
        events: list[dict] = []
        status, reason, passed = "fail", "任务未完成", False
        premature_output = False
        try:
            with wire.ServeSession(sandbox, env=env, timeout=300) as session:
                session.wait_global(lambda e: e["msg"].get("type") == "session_configured", timeout=60)
                events.extend(session.submit({"type": "user_input", "items": [{"type": "text", "text": prompt}]}, timeout=300))
                if case["kind"] == "resume":
                    events.extend(session.submit({"type": "user_input", "items": [{"type": "text", "text": "继续。"}]}, timeout=300))
                    premature_output = (workspace / "status.json").exists()
            if case["kind"] == "resume":
                thread_id = wire.rollout_thread_id(home)
                with wire.ServeSession(sandbox, args=["--resume", thread_id], env=env, timeout=300) as resumed:
                    events.extend(resumed.submit({"type": "user_input", "items": [{"type": "text", "text": "按之前约定写入 status.json。"}]}, timeout=300))
            errors = [item["msg"] for item in events if item.get("msg", {}).get("type") in ("error", "stream_error")]
            turns = [item["msg"] for item in events if item.get("msg", {}).get("type") == "turn_complete"]
            expected_turns = 3 if case["kind"] == "resume" else 1
            completed = len(turns) == expected_turns and all(turn.get("status") == "success" for turn in turns)
            if errors and not completed:
                reason = str(errors)
                upper = reason.upper()
                status = "infra_error" if any(code in upper for code in (
                    "RATE_LIMIT", "TIMEOUT", "TRANSPORT", "CONNECTION", "OVERLOADED",
                    "UNAVAILABLE", "NETWORK", "AUTH", "PROVIDER_5XX"
                )) else "fail"
            else:
                passed, reason = verify(case, workspace, expected)
                if not completed:
                    passed, reason = False, "回合未成功结束"
                if case["kind"] == "merge":
                    ends = [item["msg"] for item in events if item.get("msg", {}).get("type") == "tool_call_end"]
                    failure_seen = False
                    recovered = False
                    for end in ends:
                        if end.get("is_error"):
                            failure_seen = True
                        elif failure_seen:
                            recovered = True
                    if not recovered:
                        passed, reason = False, "未观察到工具失败后的成功恢复"
                if case["kind"] == "resume" and not any(
                    item.get("msg", {}).get("type") == "context_compacted"
                    and item["msg"].get("strategy") == "ll_m_summarize" for item in events
                ):
                    passed, reason = False, "未观察到上下文压缩事件"
                if premature_output:
                    passed, reason = False, "重启前提前创建了结果文件"
                status = "pass" if passed else "fail"
        except (OSError, AssertionError, EvaluationInfrastructureError, subprocess.SubprocessError) as exc:
            status, reason = "infra_error", str(exc)
        messages = [item.get("msg", {}) for item in events]
        tools = [message.get("tool_name", "") for message in messages if message.get("type") == "tool_call_begin"]
        usage = [message.get("usage", {}) for message in messages if message.get("type") == "turn_complete"]
        destination.mkdir(parents=True, exist_ok=True)
        trace = json.dumps(events, ensure_ascii=False, indent=2)
        (destination / "events.json").write_text(redact_key(trace, key))
        for path in workspace.glob("serve-*.stderr"):
            content = path.read_text(errors="replace")
            (destination / path.name).write_text(redact_key(content, key))
        for path in workspace.glob("serve-*.jsonl"):
            content = path.read_text(errors="replace")
            (destination / path.name).write_text(redact_key(content, key))
        return {"name": case["name"], "kind": case["kind"], "status": status, "reason": reason,
                "elapsed_s": time.monotonic() - start, "text": events_text(events), "tools": tools,
                "usage": usage, "objective_passed": passed}


def score(scorer: Path, results: list[dict]) -> None:
    """复用 zhuye-eval 判断回复和工具预算,合并客观验收。"""
    cases = []
    outputs = {}
    for i, result in enumerate(results):
        name = f"{result['name']}#{i}"
        cases.append({"name": name, "task": result["name"], "tags": [result["kind"]],
                      "assertions": [{"kind": "min_chars", "n": 1}, {"kind": "max_tool_calls", "n": 50}]})
        if result["status"] != "infra_error":
            outputs[name] = [result["text"], result["tools"]]
    completed = subprocess.run([str(scorer)], input=json.dumps({"cases": cases, "outputs": outputs}),
                               text=True, capture_output=True, check=True, timeout=60)
    report = json.loads(completed.stdout)
    for result, outcome in zip(results, report["cases"], strict=True):
        result["score"] = outcome
        if result["status"] == "pass" and not outcome["passed"]:
            result["status"] = "fail"
            result["reason"] = "评分断言失败"


def summarize(results: list[dict], input_price: float | None, output_price: float | None) -> dict:
    grouped = {}
    for item in results:
        group = grouped.setdefault(item["name"], {"pass": 0, "fail": 0, "infra_error": 0,
                                                    "elapsed_s": [], "input_tokens": 0,
                                                    "output_tokens": 0, "tool_calls": 0,
                                                    "missing_usage_runs": 0})
        group[item["status"]] += 1
        group["elapsed_s"].append(item["elapsed_s"])
        group["tool_calls"] += len(item["tools"])
        if not item["usage"]:
            group["missing_usage_runs"] += 1
        for usage in item["usage"]:
            group["input_tokens"] += usage.get("input_tokens", 0)
            group["output_tokens"] += usage.get("output_tokens", 0)
    for group in grouped.values():
        group["cost_usd"] = ((group["input_tokens"] * input_price + group["output_tokens"] * output_price) / 1_000_000
                             if input_price is not None and output_price is not None and not group["missing_usage_runs"] else None)
        group["mean_elapsed_s"] = sum(group["elapsed_s"]) / len(group["elapsed_s"])
    return grouped


def main() -> int:
    parser = argparse.ArgumentParser(description="比较固定模型与任务集上的 agent 效果")
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--scorer", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--input-price", type=float, default=None, help="每百万输入 token 的美元价格")
    parser.add_argument("--output-price", type=float, default=None, help="每百万输出 token 的美元价格")
    parser.add_argument("--policy", type=Path, default=None,
                        help="质量门禁策略文件(默认 tests/eval/policy.json)")
    parser.add_argument("--mode", choices=("observe", "enforce"), default="observe",
                        help="observe 记录 would_block 不阻断;enforce 下退化/样本不足返回 1")
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error("重复次数必须大于零")
    # 策略配置在产生任何模型费用前加载校验,配置错误属用法错误(退出码 3)。
    try:
        policy_doc = eval_policy.load_policy(args.policy)
    except (OSError, ValueError) as exc:
        print(f"质量门禁策略配置错误:{exc}", file=sys.stderr)
        return 3
    key = os.environ.get("ZHUYE_EVAL_API_KEY", "")
    if not key:
        parser.error("必须设置 ZHUYE_EVAL_API_KEY")
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    scorer = args.scorer.resolve()
    for path in [*binaries.values(), scorer]:
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f"可执行文件不存在:{path}")
    try:
        with tempfile.TemporaryDirectory(prefix="zhuye-eval-sandbox-") as tmp:
            ensure_sandbox(Path(tmp))
    except EvaluationInfrastructureError as exc:
        print(f"评测基础设施错误:{exc}", file=sys.stderr)
        return 2
    cases = json.loads(CASES.read_text())
    args.output.mkdir(parents=True, exist_ok=True)
    report = {"model": args.model, "base_url": args.base_url, "repetitions": args.repetitions,
              "case_sha256": hashlib.sha256(CASES.read_bytes()).hexdigest(), "versions": {}}
    for role, binary in binaries.items():
        version = subprocess.check_output([str(binary), "--version"], text=True, timeout=30).strip()
        results = []
        for case in cases:
            for iteration in range(args.repetitions):
                print(f"{role}: {case['name']} 第 {iteration + 1} 次", flush=True)
                results.append(run_one(case, binary, args.model, args.base_url, key,
                                       args.output / role / case["name"] / str(iteration + 1)))
        score(scorer, results)
        report["versions"][role] = {"path": str(binary), "version": version,
                                    "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                                    "cases": summarize(results, args.input_price, args.output_price),
                                    "results": results}
        if role == "candidate":
            before = report["versions"]["baseline"]["cases"]
            after = report["versions"]["candidate"]["cases"]
            report["comparison"] = {name: {
                "pass_delta": after[name]["pass"] - prior["pass"],
                "mean_elapsed_delta_s": after[name]["mean_elapsed_s"] - prior["mean_elapsed_s"],
                "input_tokens_delta": after[name]["input_tokens"] - prior["input_tokens"],
                "output_tokens_delta": after[name]["output_tokens"] - prior["output_tokens"],
                "tool_calls_delta": after[name]["tool_calls"] - prior["tool_calls"],
                "complete": prior["infra_error"] == 0 and after[name]["infra_error"] == 0,
            } for name, prior in before.items()}
        payload = json.dumps(report, ensure_ascii=False, indent=2)
        (args.output / "report.json").write_text(redact_key(payload, key))
    # 质量门禁判定:纯函数,只依赖已生成的汇总,不再触碰模型或二进制。
    policy_target = args.policy if args.policy is not None else eval_policy.DEFAULT_POLICY_PATH
    gate = {
        "mode": args.mode,
        "policy_path": str(policy_target),
        "policy_sha256": hashlib.sha256(policy_target.read_bytes()).hexdigest(),
        **eval_policy.assess(report, policy_doc),
    }
    (args.output / "gate.json").write_text(json.dumps(gate, ensure_ascii=False, indent=2))
    report["gate"] = gate
    payload = json.dumps(report, ensure_ascii=False, indent=2)
    (args.output / "report.json").write_text(redact_key(payload, key))
    print(f"质量门禁[{args.mode}] verdict={gate['verdict']} would_block={gate['would_block']}")
    for violation in gate["violations"]:
        print(f"  - 违规:{violation}")
    for item in gate["insufficient"]:
        print(f"  - 样本不足:{item}")
    has_infra = any(item["status"] == "infra_error"
                    for version in report["versions"].values() for item in version["results"])
    if has_infra:
        # 基础设施错误优先:此时样本不完整,任何 verdict 都不可信。
        print("评测存在基础设施错误,质量门禁不可判定", file=sys.stderr)
        return 2
    if args.mode == "enforce" and gate["verdict"] != "pass":
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
