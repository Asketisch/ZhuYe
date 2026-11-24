#!/usr/bin/env python3
"""cli_e2e_run.py —— 单场景执行器(由 cli_e2e.sh 逐场景调用)。

一个场景一个进程:崩溃 / 超时 / serve 异常都被隔离在本进程内,结果经
`--result` JSON 交付,套件入口把每个检查项折成一条 tcase。

退出码:0 = PASS;2 = SKIP(环境性跳过或 mock 模式下场景不适用);
1 = FAIL。结果 JSON 形状:

    {"scenario", "title", "status": "PASS|FAIL|SKIP",
     "checks": [{"name", "status": "pass|fail|skip", "detail"}],
     "metrics": {...}, "session_id", "work"}
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cli_e2e_lib as lib  # noqa: E402
from cli_e2e_scenarios import SCENARIOS  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--scenario", required=True, help="场景名(见 cli_e2e_scenarios)")
    ap.add_argument("--bin", required=True, help="zhuye CLI 二进制路径")
    ap.add_argument("--work", required=True, help="本场景的临时工作根(隔离 HOME / 工作区落此)")
    ap.add_argument("--toml", required=True, help="测试配置 cli_e2e.toml")
    ap.add_argument("--result", required=True, help="结果 JSON 输出路径")
    ap.add_argument("--mock", action="store_true", help="mock 自检模式(脚本化模型回复,不发网络请求)")
    args = ap.parse_args()

    if args.scenario not in SCENARIOS:
        print(f"未知场景:{args.scenario}(可用:{', '.join(sorted(SCENARIOS))})", file=sys.stderr)
        return 1
    scenario = SCENARIOS[args.scenario]
    cfg = lib.load_config(args.toml)
    # 场景参数 = 注册默认值 ← toml [scenarios.<name>](字段级覆盖)。
    params = {**scenario.defaults, **(cfg.get("scenarios") or {}).get(args.scenario, {})}

    result = {
        "scenario": scenario.name,
        "title": scenario.title,
        "status": "FAIL",
        "checks": [],
        "metrics": {},
        "session_id": None,
        "work": args.work,
    }
    if args.mock and scenario.needs_model:
        result["status"] = "SKIP"
        result["checks"] = [
            {"name": "mock 自检适用性", "status": "skip", "detail": "场景本质依赖真实模型,mock 模式不适用"}
        ]
        return finish(result, args.result)
    if not args.mock and scenario.mock_only:
        result["status"] = "SKIP"
        result["checks"] = [
            {"name": "真实模式适用性", "status": "skip",
             "detail": "场景依赖 mock 专属的确定性构造(小窗口/脚本时序),仅自检模式运行"}
        ]
        return finish(result, args.result)
    if args.mock and not scenario.mock_script:
        result["status"] = "SKIP"
        result["checks"] = [
            {"name": "mock 自检脚本", "status": "skip", "detail": "场景未提供 mock 脚本,自检跳过"}
        ]
        return finish(result, args.result)

    cx = lib.Ctx(scenario, cfg, params, args.work, args.bin, args.mock)
    try:
        scenario.run(cx)
    except lib.WaitTimeout as e:
        cx.checks.add("场景执行未超时", False, str(e))
    except lib.SessionDied as e:
        cx.checks.add("serve 进程存活到场景结束", False, str(e))
    except Exception:
        cx.checks.add("场景执行无未捕获异常", False, traceback.format_exc(limit=8))
    finally:
        cx.close_all()
        for s in cx.sessions:
            pass
        result["session_id"] = next((s.session_id for s in cx.sessions if s.session_id), None)
    result["checks"] = cx.checks.items
    result["metrics"] = cx.metrics
    result["status"] = cx.checks.status
    return finish(result, args.result)


def finish(result: dict, path: str) -> int:
    with open(path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)
    print(json.dumps(result, ensure_ascii=False))
    return {"PASS": 0, "SKIP": 2}.get(result["status"], 1)


if __name__ == "__main__":
    sys.exit(main())
