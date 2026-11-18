#!/usr/bin/env python3
"""diff 触发的可靠性检查选择器 —— 只做「增量定向」,不替代完整选集。

用法(VERIFICATION_ENHANCEMENT_PLAN.md P3):

    # 按 git 基线选任务(推荐在 CI / 本地提交前使用)
    python3 tests/verification/select_reliability.py --base origin/develop

    # 从文件读变更清单(逐行一个路径;- 表示 stdin)
    python3 tests/verification/select_reliability.py --from-file changed.txt

规则表 `tests/verification/reliability.json` 的语义:

- `trigger_paths` 用 fnmatch glob 匹配变更文件;
- 命中规则时**追加**其 `task_ids`(去重、保持声明顺序);
- 本脚本只输出建议追加的定向任务,永远不会减少规定的
  pr / main / nightly / release 完整选集;
- 条件任务(如 differential)是否真的执行仍由各自的启用开关决定。
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import subprocess
import sys
from pathlib import Path

DEFAULT_RULES = Path(__file__).with_name("reliability.json")


def load_rules(path: Path | None = None) -> list[dict]:
    """读取规则表;空表不是错误(仓库可能暂无规则)。"""
    target = path if path is not None else DEFAULT_RULES
    if not target.is_file():
        return []
    data = json.loads(target.read_text(encoding="utf-8"))
    rules = data.get("rules", [])
    for rule in rules:
        for field in ("id", "trigger_paths", "task_ids", "reason"):
            if field not in rule:
                raise ValueError(f"可靠性规则缺少字段 {field}:{rule}")
    return rules


def selected_tasks(rules: list[dict], changed_files: list[str]) -> list[str]:
    """按触发路径匹配变更文件,返回应追加的任务(去重、保序)。"""
    picked: list[str] = []
    for rule in rules:
        if rule.get("status", "active") != "active":
            continue
        for pattern in rule["trigger_paths"]:
            if any(fnmatch.fnmatch(file, pattern) for file in changed_files):
                for task_id in rule["task_ids"]:
                    if task_id not in picked:
                        picked.append(task_id)
                break
    return picked


def changed_files_from_git(base: str) -> list[str]:
    """`git diff --name-only <base>...HEAD` 的变更清单(基线缺失时报错)。"""
    result = subprocess.run(
        ["git", "diff", "--name-only", f"{base}...HEAD"],
        capture_output=True, text=True, timeout=30, check=False)
    if result.returncode != 0:
        raise SystemExit(f"无法解析 git 基线 {base!r}:{result.stderr.strip()}")
    return [line for line in result.stdout.splitlines() if line.strip()]


def main() -> int:
    parser = argparse.ArgumentParser(description="diff 触发的可靠性检查选择器")
    parser.add_argument("--base", help="git 基线(如 origin/develop);与 --from-file 二选一")
    parser.add_argument("--from-file", help="变更文件清单(逐行一个路径;'-' 为 stdin)")
    parser.add_argument("--rules", type=Path, default=None, help="规则表路径(默认同目录)")
    args = parser.parse_args()
    if bool(args.base) == bool(args.from_file):
        parser.error("--base 与 --from-file 必须恰好提供一个")
    if args.from_file:
        stream = sys.stdin if args.from_file == "-" else open(args.from_file, encoding="utf-8")
        with stream:
            files = [line.strip() for line in stream if line.strip()]
    else:
        files = changed_files_from_git(args.base)
    tasks = selected_tasks(load_rules(args.rules), files)
    if tasks:
        print("\n".join(tasks))
    else:
        print("(无追加的定向可靠性检查)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
