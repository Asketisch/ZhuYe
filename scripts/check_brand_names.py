#!/usr/bin/env python3
"""品牌名门禁:仓库内不得残留其他产品的品牌名(归属表述 / 功能性引用)。

修复背景(修复原因):本仓库早期从多个同类 agent 项目借鉴实现,源码注释、
文档与提交信息里残留了它们的品牌名(例如「借鉴某产品的某机制」)。这类
表述既暴露实现来源,也会让读者误以为本项目与那些产品有隶属关系;更严重
的是,若某项能力整体依赖别家产品的私有存储格式(外部 harness 会话与
凭据导入),对方一次格式迭代即断裂。故本门禁把「其他产品品牌名」登记成
清单,命中即红,并同时扫描**提交信息** —— 品牌名一旦进入历史,后续 clone
即永久可见,只能靠改写历史清除,成本远高于当下拦住。

扫描范围:
1. 工作树 —— git 跟踪的全部文本文件(本脚本自身豁免:它必须写出违禁
   字面量才能匹配);
2. 提交信息 —— 全部可达提交(subject + body)。

豁免(EXEMPT_PATTERNS)只有一类:外部生态的**格式标识符**(目录名 /
环境变量 / 上游公开数据集路径)与**厂商 API 名 / 模型 id**。这些是互操作
所需的字面量(读第三方插件、校验上游提示词摘要、按模型名匹配提示词层),
不是品牌归属表述;厂商 API 名(provider vendor)与模型名由用户在
[AGENTS.md](../AGENTS.md) 的「开发流程检查表」中显式划为不处理范围。

用法:
    python3 scripts/check_brand_names.py            # 扫描工作树 + 提交信息
    python3 scripts/check_brand_names.py --list     # 只打印登记表
    python3 scripts/check_brand_names.py --worktree # 只扫工作树

提交信息扫描需要完整克隆(`git fetch --unshallow`);浅克隆下 `git log --all`
只覆盖已取回的提交,门禁结论不完整。
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SELF_REL = "scripts/check_brand_names.py"

# 品牌名登记表(pattern, 登记原因)。
# 维护约定:新增其它产品品牌名时向本表追加;若某处确属「格式标识符」而非
# 品牌归属表述,登记到 EXEMPT_PATTERNS 并写明依据 —— 豁免必须窄到具体
# 字面量,不得放宽成整类。
BRAND_PATTERNS: list[tuple[str, str]] = [
    (r"(?i)zcode", "ZCode —— 其他产品品牌名"),
    (r"(?i)claude[-_ ]code", "Claude Code —— 其他产品品牌名"),
    (r"(?i)\bcodex\b", "Codex —— 其他产品品牌名"),
    (r"\bCursor\b", "Cursor —— 其他产品品牌名(小写 cursor 是 tree-sitter 游标术语,不受影响)"),
    (r"\bCopilot\b", "Copilot —— 其他产品品牌名"),
    (r"\bWindsurf\b", "Windsurf —— 其他产品品牌名"),
    (r"(?i)\baider\b", "Aider —— 其他产品品牌名"),
]
# 豁免字面量(pattern, 依据)。仅限「互操作必需的格式标识符」与
# 「厂商 API 名 / 模型 id」两类,不得用于掩盖品牌归属表述。
EXEMPT_PATTERNS: list[tuple[str, str]] = [
    (
        r"\.claude-plugin/",
        "外部插件生态的目录名 —— 读取该生态的第三方插件必需(格式标识符)",
    ),
    (
        r"CLAUDE_PLUGIN_ROOT",
        "外部插件生态的环境变量约定 —— 展开该生态插件内的相对路径必需(格式标识符)",
    ),
    (
        r"Anthropic/claude-code/",
        "上游公开提示词数据集内的目录名 —— 按路径取文件校验 sha256 必需(格式标识符)",
    ),
    (
        r"claude-code-(?:sonnet|opus|haiku)",
        "上游公开提示词数据集内的文件名 —— 按名核对同代文件必需(格式标识符)",
    ),
    (
        r"OpenAI/Codex/",
        "上游公开提示词数据集内的目录名 —— 按路径取文件校验 sha256 必需(格式标识符)",
    ),
    (
        r"anthropic/claude-[\w.\-]+",
        "Anthropic 模型 id —— 厂商 API 名 / 模型名,非产品品牌名",
    ),
    (
        r"\bclaude-(?:sonnet|opus|haiku|3)[\w.\-]*",
        "Anthropic 模型 id —— 厂商 API 名 / 模型名,非产品品牌名",
    ),
    (r"\bgpt-[\w.\-]+", "OpenAI 模型 id —— 厂商 API 名 / 模型名,非产品品牌名"),
    (r"\bo[134]\b", "OpenAI 模型 id —— 厂商 API 名 / 模型名,非产品品牌名"),
    (r"\bchatgpt\b", "OpenAI 模型 id —— 厂商 API 名 / 模型名,非产品品牌名"),
    (
        r"io::Cursor",
        "Rust 标准库的 io::Cursor —— 标准库类型名,与 Cursor 产品无关",
    ),
]

# 不扫描的路径片段(构建产物 / 第三方目录)。
EXEMPT_DIR_PARTS = ("target", "node_modules", ".venv", "__pycache__")


def tracked_files() -> list[Path]:
    """git 跟踪的文件(排除本脚本自身与构建产物)。"""
    out = subprocess.check_output(
        ["git", "-C", str(ROOT), "ls-files", "-z"], text=False
    )
    files: list[Path] = []
    for raw in out.split(b"\0"):
        if not raw:
            continue
        rel = raw.decode("utf-8", "replace")
        if rel == SELF_REL:
            continue
        if any(part in EXEMPT_DIR_PARTS for part in rel.split("/")):
            continue
        path = ROOT / rel
        if path.is_file():
            files.append(path)
    return sorted(files)


def commit_messages() -> list[tuple[str, str]]:
    """全部可达提交的 (短 sha, 完整提交信息)。"""
    out = subprocess.check_output(
        ["git", "-C", str(ROOT), "log", "--all", "--format=%H%x00%B%x00"], text=False
    )
    chunks = out.split(b"\0")
    pairs: list[tuple[str, str]] = []
    for i in range(0, len(chunks) - 2, 3):
        sha = chunks[i].decode("utf-8", "replace").strip()
        body = chunks[i + 1].decode("utf-8", "replace")
        if sha:
            pairs.append((sha[:12], body))
    return pairs


def scan_text(text: str, label: str, violations: list[str]) -> None:
    brand = [(re.compile(p), r) for p, r in BRAND_PATTERNS]
    exempt = [(re.compile(p), r) for p, r in EXEMPT_PATTERNS]
    for line_no, line in enumerate(text.splitlines(), start=1):
        hits = [(rx, why) for rx, why in brand if rx.search(line)]
        if not hits:
            continue
        # 同一行若命中豁免字面量,则该行的违禁命中不作数 —— 豁免必须窄到
        # 具体字面量,故按「整行豁免」处理(登记表里的豁免项都是完整路径或
        # 完整 id,不会掩盖同行的其它品牌名)。
        if any(rx.search(line) for rx, _ in exempt):
            continue
        for rx, why in hits:
            violations.append(
                f"{label}:{line_no}: 命中品牌名 /{rx.pattern}/\n"
                f"    内容: {line.strip()[:160]}\n    原因: {why}"
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true", help="只打印登记表")
    parser.add_argument("--worktree", action="store_true", help="只扫工作树")
    args = parser.parse_args()

    if args.list:
        print("品牌名登记表:")
        for pattern, reason in BRAND_PATTERNS:
            print(f"- /{pattern}/\n    原因: {reason}")
        print("\n豁免字面量:")
        for pattern, reason in EXEMPT_PATTERNS:
            print(f"- /{pattern}/\n    依据: {reason}")
        return 0

    violations: list[str] = []
    scanned = 0
    for path in tracked_files():
        rel = path.relative_to(ROOT).as_posix()
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue  # 二进制产物不扫
        scanned += 1
        scan_text(text, rel, violations)

    if not args.worktree:
        for sha, body in commit_messages():
            scan_text(body, f"commit {sha}", violations)

    print(f"已扫描 {scanned} 个跟踪文件 + 全部可达提交信息。")
    if violations:
        print(f"\n发现 {len(violations)} 处品牌名残留:\n")
        for v in violations:
            print(f"  - {v}")
        print(
            "\n处理方式:改写为中性表述(「同类 agent」等);若该处确属互操作必需的"
            "格式标识符或厂商 API 名 / 模型 id,把具体字面量登记到 "
            "scripts/check_brand_names.py 的 EXEMPT_PATTERNS 并写明依据。"
        )
        return 1
    print("门禁通过:无其他产品品牌名残留。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
