//! 工作流插件的 MCP stdio 服务;stdout 只输出 JSON-RPC。
use serde_json::{Value, json};
use std::{io, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;
use zhuye::ReviewDecision;
use zhuye_workflow_server::manager::{Definition, Manager};

fn tools() -> Value {
    let run_id = json!({"type":"string", "description":"workflow_start 返回的运行 ID"});
    let node = json!({"type":"object", "required":["id","node_type"], "properties":{
        "id":{"type":"string", "description":"本次工作流内唯一的节点实例 ID"},
        "node_type":{"type":"string", "description":"node_types 中声明的类型名"},
        "config":{}, "enabled":{"type":"boolean", "default":true},
        "max_runs":{"type":"integer", "minimum":1, "default":1},
        "timeout_ms":{"type":"integer", "minimum":1, "default":30000},
        "skip_if":{"type":"object", "required":["variable","equals"],
            "properties":{"variable":{"type":"string"},"equals":{}}}
    }});
    let definition = json!({"type":"object", "required":["node_types","nodes"], "properties":{
        "variables":{"type":"object"},
        "node_types":{"type":"object", "additionalProperties":{"type":"object", "required":["kind"], "properties":{
            "kind":{"type":"string","enum":["task","wasm","agent"]},
            "path":{"type":"string","description":"WASM 组件路径"},
            "model":{"type":"string"}, "workspace":{"type":"string"}, "instructions":{"type":"string"},
            "context_keys":{"type":"array","items":{"type":"string"}},
            "output_keys":{"type":"array","items":{"type":"string"}}
        }}},
        "nodes":{"type":"array","items":node},
        "finally_nodes":{"type":"array","items":node}
    }});
    let control = |name: &str, description: &str| {
        json!({
            "name": name, "description": description,
            "inputSchema": {"type":"object", "properties":{"run_id":run_id}, "required":["run_id"]}
        })
    };
    json!({"tools":[
        {"name":"workflow_start", "description":"启动工作流,返回运行 ID。节点类型可为 task、wasm 或 agent。",
         "inputSchema":{"type":"object", "properties":{"definition":definition, "config_path":{"type":"string", "description":"工作流 TOML 配置文件路径"}}, "oneOf":[{"required":["definition"]},{"required":["config_path"]}]}},
        control("workflow_status", "查询工作流状态、上下文、历史和待处理审批。"),
        control("workflow_pause", "在当前节点结束后暂停工作流。"),
        control("workflow_resume", "继续暂停或阻塞的工作流。"),
        control("workflow_cancel", "取消工作流并执行清理节点。"),
        control("workflow_forget", "移除已结束的工作流运行记录。"),
        {"name":"workflow_approval", "description":"回复一个待处理的 Agent 工具或 hook 审批;仅在用户授权后调用。",
         "inputSchema":{"type":"object", "properties":{"run_id":run_id, "request_id":{"type":"string"}, "approve":{"type":"boolean"}, "reason":{"type":"string"}}, "required":["run_id","request_id","approve"]}}
    ]})
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("缺少参数 {key}"))
}
fn id(args: &Value) -> Result<Uuid, String> {
    Uuid::parse_str(required(args, "run_id")?).map_err(|_| "run_id 不是有效 UUID".into())
}

async fn call(name: &str, args: &Value, manager: &Manager) -> Result<Value, String> {
    match name {
        "workflow_start" => {
            let definition: Definition = match (args.get("definition"), args.get("config_path")) {
                (Some(value), None) => serde_json::from_value(value.clone())
                    .map_err(|e| format!("工作流定义无效: {e}"))?,
                (None, Some(_)) => {
                    Definition::from_toml_file(std::path::Path::new(required(args, "config_path")?))
                        .await?
                }
                _ => return Err("必须且只能提供 definition 或 config_path".into()),
            };
            let run_id = manager.start(definition).await?;
            Ok(json!({"run_id": run_id}))
        }
        "workflow_status" => {
            let run_id = id(args)?;
            Ok(
                json!({"run_id": run_id, "snapshot": manager.status(run_id)?, "approvals": manager.approvals(run_id)?}),
            )
        }
        "workflow_pause" => {
            let run_id = id(args)?;
            Ok(json!({"run_id":run_id,"snapshot":manager.pause(run_id)?}))
        }
        "workflow_resume" => {
            let run_id = id(args)?;
            Ok(json!({"run_id":run_id,"snapshot":manager.resume(run_id)?}))
        }
        "workflow_cancel" => {
            let run_id = id(args)?;
            Ok(json!({"run_id":run_id,"snapshot":manager.cancel(run_id)?}))
        }
        "workflow_forget" => {
            let run_id = id(args)?;
            manager.forget(run_id)?;
            Ok(json!({"run_id":run_id,"forgotten":true}))
        }
        "workflow_approval" => {
            let run_id = id(args)?;
            let request_id = required(args, "request_id")?;
            let approve = args
                .get("approve")
                .and_then(Value::as_bool)
                .ok_or("缺少布尔参数 approve")?;
            let decision = if approve {
                ReviewDecision::Approve
            } else {
                ReviewDecision::Deny {
                    reason: args
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("用户拒绝")
                        .into(),
                }
            };
            manager.approve(run_id, request_id, decision)?;
            Ok(json!({"run_id":run_id,"request_id":request_id,"accepted":true}))
        }
        _ => Err(format!("未知工具: {name}")),
    }
}

async fn request(method: &str, params: &Value, manager: &Manager) -> Value {
    match method {
        "initialize" => {
            json!({"protocolVersion":"2025-06-18", "serverInfo":{"name":"zhuye-workflow","version":env!("CARGO_PKG_VERSION")}, "capabilities":{"tools":{"listChanged":false}}})
        }
        "tools/list" => tools(),
        "ping" => json!({}),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call(name, &args, manager).await {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
                }
                Err(error) => json!({"content":[{"type":"text","text":error}],"isError":true}),
            }
        }
        _ => json!({"content":[{"type":"text","text":"不支持的方法"}],"isError":true}),
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let manager = Arc::new(Manager::default());
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let capacity = Arc::new(Semaphore::new(64));
    let mut tasks = tokio::task::JoinSet::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                let mut out = stdout.lock().await;
                out.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}})).as_bytes()).await?;
                out.flush().await?;
                continue;
            }
        };
        let Some(id) = value.get("id").cloned() else {
            continue;
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let manager = Arc::clone(&manager);
        let stdout = Arc::clone(&stdout);
        let permit = Arc::clone(&capacity)
            .acquire_owned()
            .await
            .map_err(io::Error::other)?;
        tasks.spawn(async move {
            let _permit = permit;
            let result = request(&method, &params, &manager).await;
            let line = format!("{}\n", json!({"jsonrpc":"2.0","id":id,"result":result}));
            let mut out = stdout.lock().await;
            out.write_all(line.as_bytes()).await?;
            out.flush().await
        });
        // 收割已结束请求,防止长驻服务不断累积任务句柄。
        while let Some(result) = tasks.try_join_next() {
            result.map_err(io::Error::other)??;
        }
    }
    while let Some(result) = tasks.join_next().await {
        result.map_err(io::Error::other)??;
    }
    Ok(())
}
