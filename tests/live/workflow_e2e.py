#!/usr/bin/env python3
"""使用 TOML 配置驱动隔离的工作流服务和多个实际 Agent。"""
from __future__ import annotations

import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time
import tomllib

from cli_e2e_lib import load_config, make_env

HERE = Path(__file__).resolve().parent


def main() -> int:
    mock = os.environ.get("ZHUYE_E2E_MOCK") == "1"
    endpoint = Path(os.environ.get("ZHUYE_E2E_TOML", str(HERE / "cli_e2e.toml")))
    if not endpoint.exists() and mock:
        endpoint = HERE / "cli_e2e.example.toml"
    cfg = load_config(str(endpoint))
    settings = cfg["_scalars"]
    if not mock and (not settings["BASE_URL"] or not settings["MODEL"]):
        raise ValueError("真模型演练需要配置 endpoint.base_url 和 endpoint.model")
    timeout = float(os.environ.get("ZHUYE_WORKFLOW_E2E_TIMEOUT", settings["PHASE_TIMEOUT"]))
    if timeout <= 0:
        raise ValueError("演练超时必须大于零")
    source = Path(os.environ.get("ZHUYE_WORKFLOW_E2E_CONFIG", str(HERE / "workflow_e2e.example.toml"))).resolve()
    template = source.read_text(encoding="utf-8")
    model = "mock" if mock else "e2e/" + settings["MODEL"]
    # 仅替换完整 TOML 字符串占位符,防止模型名称改变配置结构。
    is_default = source == (HERE / "workflow_e2e.example.toml").resolve()
    if is_default:
        template = template.replace('"__MODEL__"', json.dumps(model, ensure_ascii=False))
    definition = tomllib.loads(template)
    nodes = definition["nodes"]
    types = definition["node_types"]
    if sum(types[n["node_type"]]["kind"] == "agent" for n in nodes) < 2 or sum(types[n["node_type"]]["kind"] == "task" for n in nodes) < 2:
        raise ValueError("演练配置至少需要两个任务节点和两个 Agent 节点")
    reply = {"type": "text", "text": json.dumps({"status": "success", "output": {"summary": "自检完成"}}, ensure_ascii=False)}
    with tempfile.TemporaryDirectory(prefix="zhuye-workflow-e2e-") as work:
        env = make_env(work, cfg, mock_script=[reply] if mock else None)
        if is_default:
            template = template.replace('workspace = "."', 'workspace = ' + json.dumps(env.ws))
            config = Path(env.ws) / "workflow.toml"
            config.write_text(template, encoding="utf-8")
        else:
            config = source
        proc = subprocess.Popen([os.environ["ZHUYE_WORKFLOW_BIN"]], cwd=env.ws, env=env.proc_env,
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                text=True, bufsize=1)
        lines: queue.Queue[str] = queue.Queue()
        threading.Thread(target=lambda: [lines.put(line) for line in proc.stdout], daemon=True).start()
        sequence = 0
        deadline = time.monotonic() + timeout
        def call(name, args):
            nonlocal sequence
            sequence += 1
            request = {"jsonrpc": "2.0", "id": sequence, "method": "tools/call", "params": {"name": name, "arguments": args}}
            proc.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
            proc.stdin.flush()
            try:
                response = json.loads(lines.get(timeout=max(0.01, deadline - time.monotonic())))
            except queue.Empty as exc:
                raise TimeoutError("等待工作流服务响应超时") from exc
            if response.get("id") != sequence:
                raise RuntimeError("工作流服务返回了错误的请求 ID")
            result = response["result"]
            if result.get("isError"):
                raise RuntimeError(result["content"][0]["text"])
            return json.loads(result["content"][0]["text"])
        try:
            run_id = call("workflow_start", {"config_path": str(config)})["run_id"]
            while time.monotonic() < deadline:
                state = call("workflow_status", {"run_id": run_id})
                snapshot = state["snapshot"]
                if state["approvals"]:
                    raise RuntimeError("工作流等待用户审批,演练不会自动批准")
                if snapshot["state"] in ("completed", "failed", "cancelled", "blocked"):
                    break
                time.sleep(0.2)
            else:
                raise TimeoutError("工作流运行超时")
            if snapshot["state"] != "completed":
                raise RuntimeError(f"工作流未完成: {snapshot['state']}, 原因: {snapshot['reason']}, 历史: {snapshot['history']}")
            history = snapshot["history"]
            if [r["node_id"] for r in history] != [n["id"] for n in nodes]:
                raise RuntimeError("节点执行顺序与配置不一致")
            for record, node in zip(history, nodes):
                if record["status"] != "success":
                    raise RuntimeError(f"节点失败: {record['node_id']}: {record['error']}")
                if types[node["node_type"]]["kind"] == "agent" and not record["agent_run_id"]:
                    raise RuntimeError(f"Agent 缺少运行标识: {record['node_id']}")
            if [r["node_id"] for r in snapshot["finally_history"]] != [n["id"] for n in definition.get("finally_nodes", [])] or any(r["status"] != "success" for r in snapshot["finally_history"]):
                raise RuntimeError("清理节点未全部成功")
            print(f"[ok] {'离线自检' if mock else '真实模型'}: {len(history)} 个主节点, {len(snapshot['finally_history'])} 个清理节点, {sum(bool(r['agent_run_id']) for r in history)} 个 Agent")
            return 0
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, RuntimeError, TimeoutError) as error:
        print(f"[FAIL] 工作流演练: {error}", file=sys.stderr)
        sys.exit(1)
