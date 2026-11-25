"""工作流插件 MCP 能力:真实服务、烛夜 Agent 与 WASM 组件。"""
from __future__ import annotations

import json
import os
from pathlib import Path
import select
import subprocess
import time

import pytest
from wire import ZHUYE_BIN

ROOT = Path(__file__).resolve().parents[2]
SERVER = Path(os.environ.get("ZHUYE_WORKFLOW_BIN", str(ZHUYE_BIN.parent / "zhuye-workflow-server")))
WASM = ROOT / "plugins/plugin-demo/bin/zhuye-workflow-echo.wasm"


class WorkflowClient:
    def __init__(self, sandbox, script):
        self.proc = subprocess.Popen(
            [str(SERVER)], cwd=sandbox.cwd,
            env={**sandbox.env(), "ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)},
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
        )
        self.seq = 0

    def request(self, method, params):
        self.seq += 1
        self.proc.stdin.write(json.dumps({"jsonrpc":"2.0", "id":self.seq, "method":method, "params":params}) + "\n")
        self.proc.stdin.flush()
        assert select.select([self.proc.stdout], [], [], 15)[0], "MCP 服务未响应"
        response = json.loads(self.proc.stdout.readline())
        assert response["id"] == self.seq
        return response["result"]

    def call(self, name, args):
        result = self.request("tools/call", {"name":name, "arguments":args})
        assert result["isError"] is False, result
        return json.loads(result["content"][0]["text"])

    def until(self, run_id, predicate):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            state = self.call("workflow_status", {"run_id":run_id})
            if predicate(state):
                return state
            time.sleep(0.03)
        pytest.fail(f"等待工作流状态超时: {state}")

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=10)
        assert self.proc.returncode == 0, self.proc.stderr.read()
        self.proc.stdout.close()
        self.proc.stderr.close()


@pytest.fixture
def workflow(sandbox, tmp_path):
    assert SERVER.is_file(), f"缺少工作流服务二进制: {SERVER}"
    assert WASM.is_file(), f"缺少工作流 WASM 样例: {WASM}"
    script = tmp_path / "agent.jsonl"
    script.write_text("", encoding="utf-8")
    client = WorkflowClient(sandbox, script)
    yield client, script
    client.close()


def test_任务_wasm_agent_阻塞恢复_清理(workflow, sandbox):
    client, script = workflow
    agent_result = {"status":"blocked", "output":{"review":"等待"}, "updates":{"reviewed":True},
                    "action":{"type":"wait", "reason":"等待确认"}}
    script.write_text(json.dumps({"type":"text", "text":json.dumps(agent_result, ensure_ascii=False)}, ensure_ascii=False) + "\n", encoding="utf-8")
    definition = {
        "variables":{"message":"你好"},
        "node_types":{
            "data":{"kind":"task"}, "wasm":{"kind":"wasm", "path":str(WASM)},
            "agent":{"kind":"agent", "model":"mock", "workspace":str(sandbox.cwd),
                     "instructions":"检查输入", "context_keys":["echoed"], "output_keys":["wasm"]},
        },
        "nodes":[
            {"id":"first", "node_type":"data", "config":{"status":"success", "output":{"first":1},
             "insertions":[{"id":"inserted","node_type":"data","after":"first","config":{"status":"success","output":{"inserted":True}}}]}},
            {"id":"skip", "node_type":"data", "skip_if":{"variable":"message", "equals":"你好"}, "config":{"status":"failed", "output":None}},
            {"id":"wasm", "node_type":"wasm"},
            {"id":"agent", "node_type":"agent", "timeout_ms":20000},
        ],
        "finally_nodes":[
            {"id":"cleanup_error", "node_type":"data", "config":{"status":"failed","output":None,"error":"清理失败"}},
            {"id":"cleanup_ok", "node_type":"data", "config":{"status":"success","output":{"cleaned":True}}},
        ],
    }
    run_id = client.call("workflow_start", {"definition":definition})["run_id"]
    blocked = client.until(run_id, lambda r: r["snapshot"]["state"] in ("blocked", "failed"))["snapshot"]
    assert blocked["state"] == "blocked", blocked
    assert blocked["reason"] == "等待确认"
    assert blocked["variables"]["echoed"] == "你好" and blocked["variables"]["reviewed"] is True
    assert [r["node_id"] for r in blocked["history"]] == ["first", "inserted", "skip", "wasm", "agent"]
    assert blocked["history"][2]["status"] == "skipped"
    assert blocked["history"][3]["output"] == {"echo":"你好"}
    assert blocked["history"][4]["agent_run_id"]
    assert blocked["history"][4]["node_type"] == "agent"
    assert blocked["history"][4]["action"] == {"type":"wait", "reason":"等待确认"}
    client.call("workflow_resume", {"run_id":run_id})
    done = client.until(run_id, lambda r: r["snapshot"]["state"] in ("completed", "failed"))["snapshot"]
    assert done["state"] == "completed", done
    assert [(r["node_id"], r["status"]) for r in done["finally_history"]] == [("cleanup_error", "failed"), ("cleanup_ok", "success")]
    assert done["finally_history"][1]["output"] == {"cleaned":True}


@pytest.mark.parametrize("approved", [False, True], ids=["拒绝", "批准"])
def test_agent工具审批保留权限边界(workflow, sandbox, approved):
    client, script = workflow
    sandbox.write_config('\n[[permissions.rule]]\ntool="write"\naction="ask"\n')
    replies = [
        {"type":"tool_call", "name":"write", "args":{"path":"approval.txt", "content":"已批准"}},
        {"type":"text", "text":json.dumps({"status":"success", "output":{"finished":True}})},
    ]
    script.write_text("".join(json.dumps(r, ensure_ascii=False)+"\n" for r in replies), encoding="utf-8")
    definition = {"node_types":{"agent":{"kind":"agent", "model":"mock", "workspace":str(sandbox.cwd), "instructions":"写文件"}},
                  "nodes":[{"id":"writer", "node_type":"agent", "timeout_ms":20000}]}
    run_id = client.call("workflow_start", {"definition":definition})["run_id"]
    pending = client.until(run_id, lambda r: bool(r["approvals"]) or r["snapshot"]["state"] in ("completed", "failed", "cancelled"))
    assert len(pending["approvals"]) == 1, pending
    approval = pending["approvals"][0]
    assert approval["kind"]["tool_name"] == "write"
    assert approval["kind"]["args"] == {"path":"approval.txt", "content":"已批准"}
    target = sandbox.cwd / "approval.txt"
    assert not target.exists()
    result = client.call("workflow_approval", {"run_id":run_id,"request_id":approval["request_id"],"approve":approved,"reason":"测试拒绝"})
    assert result["accepted"] is True
    done = client.until(run_id, lambda r: r["snapshot"]["state"] in ("completed", "failed"))["snapshot"]
    assert done["state"] == "completed", done
    assert done["history"][0]["output"] == {"finished":True}
    assert target.exists() is approved
    if approved:
        assert target.read_text(encoding="utf-8") == "已批准"


def test_等待审批时取消会丢弃回执并执行清理(workflow, sandbox):
    client, script = workflow
    sandbox.write_config('\n[[permissions.rule]]\ntool="write"\naction="ask"\n')
    script.write_text(json.dumps({"type":"tool_call", "name":"write", "args":{"path":"cancelled.txt", "content":"禁止写入"}}, ensure_ascii=False) + "\n", encoding="utf-8")
    definition = {"node_types":{"agent":{"kind":"agent", "model":"mock", "workspace":str(sandbox.cwd), "instructions":"写文件"}, "data":{"kind":"task"}},
                  "nodes":[{"id":"writer", "node_type":"agent", "timeout_ms":20000}],
                  "finally_nodes":[{"id":"cleanup", "node_type":"data", "config":{"status":"success","output":{"cleaned":True}}}]}
    run_id = client.call("workflow_start", {"definition":definition})["run_id"]
    pending = client.until(run_id, lambda r: bool(r["approvals"]) or r["snapshot"]["state"] in ("completed", "failed"))
    assert len(pending["approvals"]) == 1, pending
    request_id = pending["approvals"][0]["request_id"]
    client.call("workflow_cancel", {"run_id":run_id})
    done = client.until(run_id, lambda r: r["snapshot"]["state"] in ("cancelled", "failed", "completed"))
    assert done["snapshot"]["state"] == "cancelled", done
    assert done["snapshot"]["history"][0]["status"] == "cancelled"
    assert done["snapshot"]["history"][0]["agent_run_id"] == done["snapshot"]["history"][0]["attempt_id"]
    assert done["snapshot"]["finally_history"][0]["output"] == {"cleaned":True}
    assert done["approvals"] == []
    response = client.request("tools/call", {"name":"workflow_approval", "arguments":{"run_id":run_id, "request_id":request_id, "approve":True}})
    assert response["isError"] is True
    assert not (sandbox.cwd / "cancelled.txt").exists()


def test_工作流插件经宿主发现并执行(sandbox, tmp_path, mock_script, serve_session):
    """验证安装、启用、MCP 工具发现、审批和工作流启动的真实宿主路径。"""
    import shutil
    from wire import require_binary, run_cli
    require_binary()
    source = tmp_path / "workflow-plugin"
    shutil.copytree(ROOT / "plugins/workflow", source, ignore=shutil.ignore_patterns("bin"))
    (source / "bin").mkdir(exist_ok=True)
    shutil.copy2(SERVER, source / "bin/zhuye-workflow-server")
    install = run_cli(sandbox, "plugin", "install", str(source), timeout=90)
    assert install.returncode == 0, install.stderr
    enable = run_cli(sandbox, "plugin", "enable", "workflow@inline", timeout=30)
    assert enable.returncode == 0, enable.stderr
    tool = "mcp__plugin:workflow@inline:workflow__workflow_start"
    definition = {"node_types":{"data":{"kind":"task"}},
                  "nodes":[{"id":"first","node_type":"data","config":{"status":"success","output":{"value":7}}}]}
    script = mock_script([
        json.dumps({"type":"tool_call", "name":tool, "args":{"definition":definition}}),
        json.dumps({"type":"text", "text":"完成"}),
    ])
    with serve_session(env={"ZHUYE_MODEL":"mock", "ZHUYE_MOCK_SCRIPT":str(script), "ZHUYE_APPROVALS":"1"}) as session:
        submission = session.send({"type":"user_input", "items":[{"type":"text","text":"启动工作流"}]})
        def approve(event):
            msg = event["msg"]
            if msg.get("type") == "approval_request":
                session.send({"type":"tool_approval", "id":msg["request_id"], "decision":"approve"})
        events = session.collect(submission, timeout=60, on_event=approve)
    begins = [e["msg"] for e in events if e["msg"]["type"] == "tool_call_begin"]
    assert any(e["tool_name"] == tool for e in begins), begins
    ends = [e["msg"] for e in events if e["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 1 and ends[0]["is_error"] is False, ends
    content = ends[0]["output"]["content"]
    parsed = json.loads(content[0]["text"])
    assert parsed["run_id"]


def test_agent使用配置模型组的自定义端点(workflow, sandbox):
    """配置的本地端点必须真的收到工作流 Agent 请求。"""
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    requests = []
    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append({"path": self.path, "body": json.loads(self.rfile.read(int(self.headers["Content-Length"])))})
            result = json.dumps({"status":"success", "output":{"from":"configured"}})
            chunk = {"id":"workflow-config-test", "model":"test-model", "choices":[{"index":0,"delta":{"content":result},"finish_reason":"stop"}]}
            body = ("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def log_message(self, *_args):
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config('\n[active]\ngroup="workflow_test"\n[[model_group]]\nid="workflow_test"\nendpoint="openai"\n'
                             f'base_url="http://127.0.0.1:{gateway.server_port}/v1"\napi_key=""\nmodel="test-model"\n')
        client, _ = workflow
        definition = {"node_types":{"agent":{"kind":"agent", "model":"workflow_test/test-model", "workspace":str(sandbox.cwd), "instructions":"检查输入", "output_keys":["first"]}},
                      "nodes":[{"id":"first", "node_type":"agent", "timeout_ms":15000}, {"id":"second", "node_type":"agent", "timeout_ms":15000}]}
        run_id = client.call("workflow_start", {"definition":definition})["run_id"]
        done = client.until(run_id, lambda r: r["snapshot"]["state"] in ("completed", "failed"))["snapshot"]
        assert done["state"] == "completed", done
        assert done["history"][0]["output"] == {"from":"configured"}
        assert len(requests) == 2
        assert all(r["path"] == "/v1/chat/completions" and r["body"]["model"] == "test-model" for r in requests)
        assert done["history"][1]["output"] == {"from":"configured"}
        assert done["history"][0]["agent_run_id"] != done["history"][1]["agent_run_id"]
        messages = json.dumps(requests[1]["body"]["messages"], ensure_ascii=False)
        assert "first" in messages and "configured" in messages
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_toml配置加载和校验(workflow, tmp_path):
    client, _ = workflow
    config = tmp_path / "workflow.toml"
    config.write_text('''
[variables]
message = "来自配置"
[node_types.data]
kind = "task"
[[nodes]]
id = "one"
node_type = "data"
config = { status = "success", output = { value = 7 }, updates = { done = true } }
[[nodes]]
id = "two"
node_type = "data"
config = { status = "success", output = { value = 8 } }
[[finally_nodes]]
id = "cleanup"
node_type = "data"
config = { status = "success", output = { cleaned = true } }
''', encoding="utf-8")
    run_id = client.call("workflow_start", {"config_path":str(config)})["run_id"]
    result = client.until(run_id, lambda r: r["snapshot"]["state"] in ("completed", "failed"))["snapshot"]
    assert result["state"] == "completed", result
    assert result["variables"] == {"message":"来自配置", "done":True}
    assert [(r["node_id"], r["output"]) for r in result["history"]] == [("one", {"value":7}), ("two", {"value":8})]
    assert result["finally_history"][0]["output"] == {"cleaned":True}
    for args in ({}, {"config_path":str(config), "definition":{}}, {"config_path":str(tmp_path / "missing.toml")}):
        assert client.request("tools/call", {"name":"workflow_start", "arguments":args})["isError"] is True
    config.write_text("[node_types\n", encoding="utf-8")
    error = client.request("tools/call", {"name":"workflow_start", "arguments":{"config_path":str(config)}})
    assert error["isError"] is True and "TOML" in error["content"][0]["text"]
