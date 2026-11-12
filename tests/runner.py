#!/usr/bin/env python3
"""tests/runner.py —— 三仓统一测试调度器(薄入口)。

按 PLAN.md §1.4 的接口实现:

    python tests/runner.py list
    python tests/runner.py run --suite pr
    python tests/runner.py run --suite smoke
    python tests/runner.py run --task agent.contract.protocol
    python tests/runner.py run --case A-C03
    python tests/runner.py run --suite release --artifact <产物路径> --no-build
    python tests/runner.py run --case T-E04 --seed 42 --report-dir <目录>

设计要点:

- 调度器只做「任务注册表 → 原生测试工具」的编排,不复制测试逻辑;
  既有 `.sh` 入口保留,由本入口转发调用(POSIX)。
- Windows 上直接启动 Cargo / Python / Node / 原生适配器,不间接依赖 Bash;
  仅存在于 Bash 脚本的任务在 Windows 报告「平台不支持」,不算通过。
- 结果语义(PLAN §5.4):
    pass            通过
    fail            必测断言失败
    skip            条件不适用(平台/可选功能/live 缺凭据),不增加通过计数
    infra_error     基础设施错误(缺依赖、零用例、缺报告)
    timeout         超时(同样计入失败)
    pending         任务已登记但尚无实现映射,报告为「未覆盖」,不算通过
- 报告:人类摘要(stdout)+ 机器可读 JSON + JUnit XML(写入 report-dir)。
- 退出码:0 = 所选任务全部通过(允许 skip);1 = 存在失败;2 = 基础设施错误
  或存在 pending/未覆盖的必测任务;3 = 用法错误。

仅依赖 Python 3.10+ 标准库,不成为产品运行依赖。
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import time
import urllib.parse
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent

# 注册表与本文件同目录;由各仓库自带,任务 ID 与 PLAN.md 表格一致。
sys.path.insert(0, str(HERE))
import test_registry as reg  # noqa: E402  注册表必须本地导入


# --------------------------------------------------------------------------- #
# 数据模型
# --------------------------------------------------------------------------- #

@dataclass
class TaskResult:
    """单任务执行结果。"""

    task_id: str
    status: str            # pass / fail / skip / infra_error / timeout / pending
    detail: str = ""
    duration_s: float = 0.0
    command: list[str] | None = None
    log_path: str | None = None
    seed: int | None = None

    @property
    def ok(self) -> bool:
        # skip 不阻断;其余非 pass 状态都视为需要处理的结果。
        return self.status == "pass" or self.status == "skip"


# --------------------------------------------------------------------------- #
# 环境与证据采集(PLAN §5.4:报告必须记录真实被测身份)
# --------------------------------------------------------------------------- #

def _git(args: list[str]) -> str:
    try:
        out = subprocess.run(
            ["git", "-C", str(ROOT), *args],
            capture_output=True, text=True, timeout=15, check=False,
        )
        return out.stdout.strip() if out.returncode == 0 else "未知"
    except Exception:
        return "未知"


def environment_info() -> dict:
    """采集运行环境与被测产物身份。"""
    try:
        dirty = bool(_git(["status", "--porcelain"]))
    except Exception:
        dirty = None
    info = {
        "repo": ROOT.name,
        "git_head": _git(["rev-parse", "HEAD"]),
        "git_branch": _git(["rev-parse", "--abbrev-ref", "HEAD"]),
        "worktree_dirty": dirty,
        "os": platform.system(),
        "os_version": platform.version(),
        "arch": platform.machine(),
        "python": sys.version.split()[0],
        "runner_invocation": sys.argv,
    }
    # 子模块指针:两个客户端固定不同 Runtime 版本,报告必须记录(PLAN §1.3)。
    subs = _git(["submodule", "status"])
    info["submodules"] = subs if subs and subs != "未知" else []
    return info


# --------------------------------------------------------------------------- #
# 任务执行
# --------------------------------------------------------------------------- #

def _platform_supported(task: dict) -> tuple[bool, str]:
    """按注册表声明判断当前平台是否支持该任务。"""
    systems = task.get("os", None)
    if not systems:
        return True, ""
    current = platform.system().lower()
    for s in systems:
        name = s.lower()
        if name == current:
            return True, ""
        # Windows 的 Git-Bash 环境仍按 Windows 对待。
        if name == "windows" and current in ("mingw", "msys", "cygwin"):
            return True, ""
    return False, f"任务声明仅支持 {'/'.join(systems)},当前平台 {platform.system()}"


def _platform_required(task: dict) -> bool:
    """判断当前平台是否为该任务的正式支持平台。"""
    required = task.get("required_on")
    if not required:
        return False
    current = platform.system().lower()
    return current in {item.lower() for item in required}


def _needs_bash(task: dict) -> bool:
    cmd = task.get("command") or []
    return bool(cmd) and cmd[0].endswith(".sh")


def validate_registry() -> None:
    """启动前校验注册表,防止任务引用漂移导致门禁假绿。"""
    if not reg.TASKS:
        raise RuntimeError("任务注册表为空")
    if not reg.SUITES:
        raise RuntimeError("选集注册表为空")
    for suite, ids in reg.SUITES.items():
        if not ids:
            raise RuntimeError(f"选集 {suite} 为空")
        if len(ids) != len(set(ids)):
            raise RuntimeError(f"选集 {suite} 存在重复任务")
        unknown = [tid for tid in ids if tid not in reg.TASKS]
        if unknown:
            raise RuntimeError(f"选集 {suite} 引用未知任务:{','.join(unknown)}")
    for task_id, task in reg.TASKS.items():
        coverage = task.get("coverage", "mapped")
        has_impl = bool(task.get("builtin") or task.get("command"))
        if coverage == "mapped" and not has_impl:
            raise RuntimeError(f"mapped 任务 {task_id} 缺少执行命令")
        if coverage == "conditional" and task.get("enabled") and not has_impl:
            raise RuntimeError(f"已启用条件任务 {task_id} 缺少执行命令")
    # 除自动插入的 shared.harness 外,每个登记任务都必须至少进入一个
    # 长期选集,否则注册表看似完整、实际从未被 CI 调度。
    covered = {task_id for ids in reg.SUITES.values() for task_id in ids}
    uncovered = [
        task_id
        for task_id in reg.TASKS
        if task_id != "shared.harness" and task_id not in covered
    ]
    if uncovered:
        raise RuntimeError(f"任务未进入任何选集:{','.join(uncovered)}")
    for case, entry in reg.CASE_INDEX.items():
        task_id = entry["task"] if isinstance(entry, dict) else entry
        if task_id not in reg.TASKS:
            raise RuntimeError(f"用例 {case} 引用未知任务:{task_id}")


def _run_builtin(name: str, ctx: dict) -> tuple[str, str]:
    """执行注册表内置任务(纯 Python 自检,不派生进程)。"""
    if name == "shared.harness":
        return _builtin_shared_harness()
    return "fail", f"未知内置任务:{name}"


def _builtin_shared_harness() -> tuple[str, str]:
    """shared.harness —— 公共基座自检(B01/B08/B09 的调度器侧最小实现)。

    校验测试基础设施本身:Python 版本、可选工具链可用性、报告目录可写。
    缺少「必测依赖」时报基础设施错误,而不是让下游任务静默全绿。
    """
    problems: list[str] = []
    notes: list[str] = []

    if sys.version_info < (3, 10):
        problems.append(f"Python 版本 {sys.version.split()[0]} 低于最低要求 3.10")

    # 可选工具链:缺失时给出明确说明,由任务自身的 skip 语义兜底。
    for cmd, label in (("cargo", "Rust 工具链"), ("python3", "Python"), ("node", "Node.js")):
        path = shutil.which(cmd)
        if path:
            notes.append(f"{label}:{path}")
        else:
            notes.append(f"{label}:不可用(相关任务将按 skip/infra 语义报告)")

    # 报告目录可写性(B11 证据完整性的前置)。
    try:
        probe = ROOT / ".runner-probe"
        probe.write_text("probe", encoding="utf-8")
        probe.unlink()
        notes.append("仓库目录可写")
    except OSError as e:
        problems.append(f"仓库目录不可写:{e}")

    if problems:
        return "infra_error", ";".join(problems)
    return "pass", "; ".join(notes)


def run_task(task_id: str, args: argparse.Namespace) -> TaskResult:
    """执行单个任务并落盘日志证据。"""
    task = reg.TASKS[task_id]
    result = TaskResult(task_id=task_id, status="pass")
    if getattr(args, "seed", None) is not None:
        result.seed = args.seed

    coverage = task.get("coverage", "mapped")
    if coverage == "pending":
        return TaskResult(task_id, "pending",
                          task.get("note", "任务已登记,尚无实现映射(未覆盖)"))
    if coverage == "conditional" and not task.get("enabled", False):
        return TaskResult(task_id, "skip",
                          f"条件能力未启用:{task.get('note', '')}")

    ok, reason = _platform_supported(task)
    if not ok:
        if _platform_required(task):
            return TaskResult(
                task_id, "infra_error",
                f"正式支持平台缺少可执行映射 —— {reason}",
            )
        return TaskResult(task_id, "skip", f"平台不支持 —— {reason}")

    if args.dry_run:
        cmd = task.get("command")
        return TaskResult(task_id, "skip",
                          f"[dry-run] {cmd or '(内置任务)'}")

    start = time.monotonic()
    log_dir = Path(args.report_dir)
    log_dir.mkdir(parents=True, exist_ok=True)

    if task.get("builtin"):
        status, detail = _run_builtin(task["builtin"], vars(args))
        result.status, result.detail = status, detail
        result.duration_s = time.monotonic() - start
        return result

    if not task.get("command"):
        return TaskResult(task_id, "infra_error", "mapped 任务缺少执行命令")

    cmd = list(task["command"])
    # Windows 不经 Bash 间接依赖:脚本类任务在此平台直接报告不支持。
    if _needs_bash(task):
        if platform.system().lower() not in ("linux", "darwin"):
            return TaskResult(task_id, "skip",
                              "任务入口为 Bash 脚本,当前平台不支持")
        bash = shutil.which("bash")
        if not bash:
            return TaskResult(task_id, "infra_error", "未找到 bash")
        cmd = [bash, *cmd]

    env = os.environ.copy()
    env.setdefault("ZHUYE_MODEL", "mock")
    for key, value in task.get("env", {}).items():
        env[key] = str(value)
    if result.seed is not None:
        env["RUNNER_SEED"] = str(result.seed)
    if getattr(args, "artifact", None) and task.get("uses_artifact"):
        env["RUNNER_ARTIFACT"] = str(args.artifact)

    timeout = task.get("timeout_s", args.timeout)
    log_path = log_dir / f"{task_id.replace('/', '_')}.log"
    try:
        with open(log_path, "w", encoding="utf-8") as log:
            log.write(f"# 复现命令:{' '.join(cmd)}\n\n")
            log.flush()
            # 新进程组:超时时整组终止,回收子孙进程(B06)。
            proc = subprocess.Popen(
                cmd, cwd=str(ROOT), env=env, stdout=log, stderr=subprocess.STDOUT,
                start_new_session=os.name != "nt",
            )
            try:
                rc = proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                _kill_tree(proc)
                result.status = "timeout"
                result.detail = f"超过超时 {timeout}s,进程组已终止"
                result.command, result.log_path = cmd, str(log_path)
                result.duration_s = time.monotonic() - start
                return result
        result.command, result.log_path = cmd, str(log_path)
        result.duration_s = time.monotonic() - start
        if rc == 0:
            result.detail = task.get("note", "通过")
        else:
            result.status = "fail"
            result.detail = f"退出码 {rc}(详见 {log_path.name})"
    except FileNotFoundError as e:
        result.status, result.detail = "infra_error", f"依赖缺失:{e}"
        result.duration_s = time.monotonic() - start
    except OSError as e:
        result.status, result.detail = "infra_error", f"无法启动:{e}"
        result.duration_s = time.monotonic() - start
    return result


def _kill_tree(proc: subprocess.Popen) -> None:
    """终止整个进程组,尽力回收子孙进程。"""
    try:
        if os.name == "nt":
            proc.kill()
        else:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        proc.kill()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass


# --------------------------------------------------------------------------- #
# 选择与报告
# --------------------------------------------------------------------------- #

def resolve_selection(args: argparse.Namespace) -> tuple[list[str], str]:
    """把 CLI 选择解析为任务 ID 列表。"""
    if args.task:
        if args.task not in reg.TASKS:
            return [], f"未知任务:{args.task}(runner.py list 查看)"
        return [args.task], ""
    if args.case:
        entry = reg.CASE_INDEX.get(args.case)
        if not entry:
            return [], f"未知用例:{args.case}(注册表 CASE_INDEX 尚未收录)"
        task_id = entry["task"] if isinstance(entry, dict) else entry
        return [task_id], ""
    if args.suite:
        if args.suite not in reg.SUITES:
            return [], (f"未知选集:{args.suite}(可选:{' / '.join(reg.SUITES)})")
        return list(reg.SUITES[args.suite]), ""
    return [], "必须指定 --suite / --task / --case 之一"


def write_json_report(path: Path, env: dict, results: list[TaskResult], selection: str) -> None:
    counts: dict[str, int] = {}
    for r in results:
        counts[r.status] = counts.get(r.status, 0) + 1
    payload = {
        "schema": "zhuye-test-report/1",
        "generated_at": _dt.datetime.now().isoformat(timespec="seconds"),
        "selection": selection,
        "environment": env,
        "summary": {
            "total": len(results),
            "counts": counts,
            "all_ok": all(r.ok for r in results),
        },
        "results": [{
            "task": r.task_id, "status": r.status, "detail": r.detail,
            "duration_s": round(r.duration_s, 2),
            "command": r.command, "log": r.log_path, "seed": r.seed,
        } for r in results],
    }
    path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")


def write_junit_report(path: Path, results: list[TaskResult]) -> None:
    """JUnit XML:CI 系统可消费的逐任务结果。"""
    root = ET.Element("testsuites")
    suite = ET.SubElement(root, "testsuite", name="zhuye-runner", tests=str(len(results)))
    for r in results:
        tc = ET.SubElement(suite, "testcase",
                           name=r.task_id, classname=f"zhuye.{r.task_id}",
                           time=f"{r.duration_s:.2f}")
        if r.status == "fail":
            ET.SubElement(tc, "failure", message=r.detail)
        elif r.status == "timeout":
            ET.SubElement(tc, "error", message=r.detail, type="timeout")
        elif r.status == "infra_error":
            ET.SubElement(tc, "error", message=r.detail, type="infra")
        elif r.status in ("skip", "pending"):
            ET.SubElement(tc, "skipped", message=r.detail or r.status)
    ET.indent(root)
    ET.ElementTree(root).write(path, encoding="utf-8", xml_declaration=True)


def print_human_summary(results: list[TaskResult], env: dict, duration_s: float) -> None:
    icon = {"pass": "✅", "fail": "❌", "skip": "⏭️ ", "timeout": "⏱️ ",
            "infra_error": "🧰", "pending": "🕳️ "}
    print("\n" + "═" * 62)
    print(f"  结果汇总  ({env['repo']} @ {env['git_head'][:12]})"
          f"  工作树:{'干净' if not env['worktree_dirty'] else '有变更'}")
    print("═" * 62)
    for r in results:
        line = f"  {icon.get(r.status, '❓')} {r.status:<11} {r.task_id}"
        if r.duration_s:
            line += f"  ({r.duration_s:.1f}s)"
        print(line)
        if r.detail:
            print(f"       └ {r.detail}")
    counts: dict[str, int] = {}
    for r in results:
        counts[r.status] = counts.get(r.status, 0) + 1
    print("─" * 62)
    print("  " + "  ".join(f"{k}={v}" for k, v in sorted(counts.items()))
          + f"  总耗时 {duration_s:.1f}s")
    print("═" * 62)


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #

def cmd_list(_: argparse.Namespace) -> int:
    validate_registry()
    print(f"仓库:{ROOT.name}(任务 {len(reg.TASKS)} 个,选集 {len(reg.SUITES)} 个)\n")
    print("任务(coverage:mapped=已有实现映射  pending=未覆盖  conditional=条件能力):")
    for tid, task in reg.TASKS.items():
        suites = [s for s, ids in reg.SUITES.items() if tid in ids]
        cov = task.get("coverage", "mapped")
        note = task.get("note", "")
        print(f"  {tid:<32} [{cov:<11}] 选集:{','.join(suites) or '-'}"
              + (f"  # {note}" if note else ""))
    print("\n选集:")
    for name, ids in reg.SUITES.items():
        print(f"  {name:<10} -> {', '.join(ids)}")
    if reg.CASE_INDEX:
        print(f"\n用例索引:收录 {len(reg.CASE_INDEX)} 条 PLAN 用例 ID(runner.py run --case <ID>)")
    return 0


def cmd_run(args: argparse.Namespace) -> int:
    try:
        validate_registry()
    except RuntimeError as e:
        print(f"注册表错误:{e}", file=sys.stderr)
        return 3

    task_ids, err = resolve_selection(args)
    if err:
        print(f"错误:{err}", file=sys.stderr)
        return 3

    env = environment_info()
    print(f"被测身份:HEAD {env['git_head']}"
          f"(工作树{'干净' if not env['worktree_dirty'] else '有变更'})")
    print(f"环境:{env['os']}/{env['arch']}  Python {env['python']}\n")

    # 执行选集前先跑公共基座自检:基座坏掉时直接报基础设施错误(B07/B08)。
    sequence = list(task_ids)
    if "shared.harness" in reg.TASKS and "shared.harness" not in sequence:
        sequence.insert(0, "shared.harness")

    results: list[TaskResult] = []
    start = time.monotonic()
    for tid in sequence:
        print(f"▶ {tid} ...", flush=True)
        r = run_task(tid, args)
        results.append(r)
        mark = {"pass": "通过", "fail": "失败", "skip": "跳过",
                "timeout": "超时", "infra_error": "基础设施错误",
                "pending": "未覆盖"}.get(r.status, r.status)
        print(f"  → {mark}" + (f":{r.detail}" if r.detail else ""), flush=True)
        if r.status == "infra_error":
            # 基座坏了,后续任务的结果不可信,全部不再执行。
            print("  └ 基础设施错误,中止后续任务", flush=True)
            break
    duration = time.monotonic() - start

    report_dir = Path(args.report_dir)
    report_dir.mkdir(parents=True, exist_ok=True)
    write_json_report(report_dir / "report.json", env, results, " ".join(sys.argv[2:]))
    write_junit_report(report_dir / "junit.xml", results)
    print_human_summary(results, env, duration)
    print(f"报告:{report_dir / 'report.json'}(JUnit:{report_dir / 'junit.xml'})")

    has_fail = any(r.status in ("fail", "timeout") for r in results)
    has_infra = any(r.status in ("infra_error", "pending") for r in results)
    return 1 if has_fail else (2 if has_infra else 0)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="runner.py", description="三仓统一测试调度器(PLAN.md §1.4)")
    sub = parser.add_subparsers(dest="cmd", required=True)

    sub.add_parser("list", help="列出任务、选集与用例索引")

    run = sub.add_parser("run", help="执行选集 / 任务 / 用例")
    sel = run.add_mutually_exclusive_group(required=True)
    sel.add_argument("--suite", help="选集:" + " / ".join(reg.SUITES))
    sel.add_argument("--task", help="任务 ID(runner.py list 查看)")
    sel.add_argument("--case", help="PLAN 用例 ID(如 A-C03 / T-E04 / D-E05)")
    run.add_argument("--report-dir", default=str(ROOT / ".test-runner"),
                     help="报告与日志输出目录(默认 .test-runner)")
    run.add_argument("--timeout", type=int, default=3600,
                     help="单任务默认超时秒数(任务可自行覆盖)")
    run.add_argument("--seed", type=int, default=None, help="传入任务的随机种子")
    run.add_argument("--artifact", default=None,
                     help="发布验证用的产物路径(配合 release 选集)")
    run.add_argument("--no-build", action="store_true",
                     help="不触发构建,直接使用现有产物(由任务自行尊重)")
    run.add_argument("--dry-run", action="store_true",
                     help="只打印将要执行的命令,不实际运行")

    args = parser.parse_args(argv)
    if args.cmd == "list":
        return cmd_list(args)
    if args.no_build:
        os.environ["RUNNER_NO_BUILD"] = "1"
    return cmd_run(args)


if __name__ == "__main__":
    sys.exit(main())
