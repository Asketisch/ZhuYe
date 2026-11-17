#!/usr/bin/env python3
"""拒绝缺失、空集或全跳过的必测 JUnit 报告。"""

import sys
import xml.etree.ElementTree as ET


def executed(path: str) -> int:
    root = ET.parse(path).getroot()
    cases = root.findall(".//testcase")
    return sum(case.find("skipped") is None for case in cases)


if __name__ == "__main__":
    try:
        count = executed(sys.argv[1])
        if count < 1:
            raise ValueError("没有执行任何用例")
        print(f"JUnit:实际执行 {count} 条")
    except (IndexError, OSError, ValueError, ET.ParseError) as exc:
        print(f"JUnit 报告无效:{exc}", file=sys.stderr)
        sys.exit(2)
