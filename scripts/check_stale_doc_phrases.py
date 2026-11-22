#!/usr/bin/env python3
"""作废说法文档门禁:当前事实文档命中登记表里的已作废说法即失败。

修复背景(修复原因):规则演进后,旧表述常残留在其它文档里误导读者
(实例:cache 断点 TTL 在 wire 层曾被硬编码 5m,修复后部分文档仍按
旧语义描述;EventMsg/crate 计数随演进漂移而文档未同步)。本门禁把
「已作废说法」登记成清单,当前事实文档命中即红;历史/归档目录豁免
(允许冻结旧写法,只门禁现状文档)。

用法:
    python3 scripts/check_stale_doc_phrases.py            # 扫描并报错
    python3 scripts/check_stale_doc_phrases.py --list     # 只打印登记表

登记新条目:向 STALE_PHRASES 追加 (pattern, 原因说明);pattern 为
Python 正则。扫描范围:docs/**.md、根目录 *.md、crates/**/AGENTS.md、
sdks/*.md;豁免:EXEMPT_DIR_PARTS 中的路径片段(归档/规划/历史文档)。
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# 已作废说法登记表(pattern, 登记原因)。
# 维护约定:修复/演进使某文档表述作废时,除改文档外,把旧表述登记到这里,
# 防止同说法在其它现状文档里复发。
STALE_PHRASES: list[tuple[str, str]] = [
    # cache 断点 TTL 修复前的旧语义(修复:请求层逐断点携带,provider 透传)
    (r"消息锚点\s*5m", "cache 断点 TTL 已由请求层逐断点携带,不再有「消息锚点固定 5m」语义"),
    (r"wire 层硬编码\s*5m(?!.*(已修|修复))", "wire 层 5m 硬编码已修复;现状文档不得把硬编码当作现行行为描述"),
    (r"硬编码\s*[\"'`]5m[\"'`](?!.*(已修|修复))", "wire 层 5m 硬编码已修复;现状文档不得把硬编码当作现行行为描述"),
    # 协议变体计数的旧快照(演进后漂移)
    (r"EventMsg[^\n]{0,12}49\s*种", "EventMsg 变体数已演进,旧计数 49 已作废"),
    (r"events\.json`\s*50\s*变体", "events.json 变体数已演进,旧计数 50 已作废"),
    (r"Op[^\n]{0,12}(?:2[0-6]|2[8-9]|3\d)\s*种", "Op 变体数以源码 discriminant 为准(当前 27),旧计数已作废"),
    # workspace crate 计数的旧快照
    (r"(?:共|has)\s*\*{0,2}45\s*(?:个 crate|crates)\*{0,2}", "workspace crate 计数已演进(46),旧计数 45 已作废"),
    # serve 握手旧语义
    (r"serve\s*进程一问一答", "serve 握手语义已演进(session_configured 主动下发),旧表述作废"),
]

# 豁免路径片段(归档 / 规划 / 历史轨迹文档允许冻结旧写法)。
EXEMPT_DIR_PARTS = ("archive", "plans", "target", "node_modules")

# 扫描范围:现状事实文档。
SCAN_GLOBS = (
    "docs/**/*.md",
    "*.md",
    "crates/**/AGENTS.md",
    "sdks/*.md",
    "sdks/*/README.md",
    "plugins/*.md",
    "tests/COVERAGE.md",
)


def iter_doc_files() -> list[Path]:
    files: set[Path] = set()
    for pattern in SCAN_GLOBS:
        for path in ROOT.glob(pattern):
            if path.is_file():
                files.add(path)
    out = []
    for path in sorted(files):
        rel = path.relative_to(ROOT).as_posix()
        if any(part in rel for part in EXEMPT_DIR_PARTS):
            continue
        out.append(path)
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true", help="只打印登记表")
    args = parser.parse_args()

    if args.list:
        for pattern, reason in STALE_PHRASES:
            print(f"- {pattern}\n    原因: {reason}")
        return 0

    compiled = [(re.compile(pattern), reason) for pattern, reason in STALE_PHRASES]
    violations: list[str] = []
    scanned = 0
    for path in iter_doc_files():
        rel = path.relative_to(ROOT).as_posix()
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError) as exc:
            print(f"警告: 无法读取 {rel}: {exc}", file=sys.stderr)
            continue
        scanned += 1
        for line_no, line in enumerate(text.splitlines(), start=1):
            for regex, reason in compiled:
                if regex.search(line):
                    violations.append(
                        f"{rel}:{line_no}: 命中作废说法 /{regex.pattern}/\n"
                        f"    内容: {line.strip()[:160]}\n    原因: {reason}"
                    )

    print(f"已扫描 {scanned} 份现状文档,登记表 {len(STALE_PHRASES)} 条。")
    if violations:
        print(f"\n发现 {len(violations)} 处作废说法(当前事实文档不得保留旧表述):\n")
        for v in violations:
            print(f"  - {v}")
        print(
            "\n处理方式:更新该处表述为现行语义;若属历史文档,移入豁免目录"
            "(路径含 archive/plans)。"
        )
        return 1
    print("门禁通过:无作废说法残留。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
