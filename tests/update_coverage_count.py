#!/usr/bin/env python3
"""从 pytest 收集结果刷新或校验能力覆盖矩阵的测试总数。"""

import argparse
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
DOC = ROOT / "tests/COVERAGE.md"


def update(text: str, count: int) -> str:
    patterns = [
        (r"(\| \*\*能力覆盖层\*\*\(主\).*?\| ❌ \| )\d+( 条\(pytest 收集)", rf"\g<1>{count}\g<2>"),
        (r"(\| \*\*能力覆盖层合计\*\* \| \*\*)\d+( 条\*\*)", rf"\g<1>{count}\g<2>"),
    ]
    for pattern, replacement in patterns:
        text, changed = re.subn(pattern, replacement, text)
        if changed != 1:
            raise ValueError("覆盖矩阵中的计数行缺失或不唯一")
    return text


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="只检查计数,不修改文档")
    args = parser.parse_args()
    process = subprocess.run([sys.executable, "-m", "pytest", "tests/functional", "--collect-only", "-q", "-p", "no:cacheprovider"],
                             cwd=ROOT, capture_output=True, text=True, check=True)
    match = re.search(r"(\d+) tests? collected", process.stdout)
    if not match:
        raise ValueError("pytest 未报告收集数量")
    count = int(match.group(1))
    original = DOC.read_text()
    updated = update(original, count)
    if args.check and updated != original:
        print(f"能力覆盖矩阵计数过期,当前收集 {count} 条", file=sys.stderr)
        return 1
    if not args.check and updated != original:
        DOC.write_text(updated)
    print(f"能力覆盖层收集 {count} 条")
    return 0


if __name__ == "__main__":
    sys.exit(main())
