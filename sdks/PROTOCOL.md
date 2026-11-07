# ZhuYe Serve Wire Protocol(v0 + SDK 扩展)

`zhuye serve` 是常驻 stdio JSONL 会话服务:一个进程 = 一个常驻 AgentThread,
多轮对话共享内存状态。Python / TypeScript SDK 都构建在该协议之上 —— 本文档
是两套 SDK 的共同实现依据,也可供任何语言手写集成。

- **stdin**:每行一个 `Submission` JSON 对象(UTF-8,`\n` 分隔)。
- **stdout**:每行一个 `Event` JSON 对象。
- **stderr**:tracing 日志(人类可读,非协议数据;SDK 直接透传或丢弃)。

所有枚举判别符为 serde `snake_case` 字符串(协议 v0 已冻结:新增变体属
non-breaking,客户端必须容忍未知 `"type"`)。

---

## 0. 协议面与版本策略

### 0.1 三个协议面

| 面 | 载体 | 定义位置 |
|---|---|---|
| Submission(Op) | 客户端 → AgentThread 的 mpsc 通道;`zhuye serve` 上为 stdin 每行一个 JSON | `crates/protocol/zhuye-protocol/src/op.rs` |
| Event(EventMsg) | AgentThread → 客户端;serve 上为 stdout 每行一个 JSON | `crates/protocol/zhuye-protocol/src/event_msg/` |
| RolloutRecord | 持久化 JSONL(`~/.zhuye/sessions/YYYY/MM/DD/<thread>.jsonl`) | `crates/protocol/zhuye-protocol/src/recorder.rs` |

三者共享 `ThreadId` / `TurnId` newtype 与 `ContentBlock` / `ToolError`
等基础类型(`item.rs`)。

### 0.2 版本策略

- 判别符 serde `snake_case`;未知变体容忍是**客户端义务**;
- 语义化承诺:Op / EventMsg 只增不改;如需字段演进,新增变体而非
  复用旧名;
- 改动既有变体字段属 breaking,禁止;
- serve 的 SDK 扩展(如 `submission_closed`)在本文单独标注引入版本;
- `RolloutRecord` 的持久化形态见 `crates/protocol/zhuye-protocol/src/recorder.rs`:
  `SessionMeta`(首条)/ `Message` / `Compaction`(resume 回放压缩边界)/
  `Fork`(分叉标记)/ `Checkpoint` / `DiscussionTranscript` /
  `TokenCount`(token / 成本索引)。文件 256 KiB 轮转 ×3,单条 16 KiB
  截断 + 密钥脱敏(见 [docs/sanitize.md](../docs/sanitize.md));
  `truncate_after` 支持 rewind 物理截断(先写 `.bak`)。

---

## 1. 启动与握手

```
zhuye serve [--resume <uuid> | -c | -r <N>] [--agent <name>] [--plan-mode] ...
```

进程就绪后**主动**向 stdout 写一条全局事件(此时 stdin 还没有任何输入):

```json
{"id": "", "msg": {"type": "session_configured", "session_id": "<uuid>", "model": "...", "provider": "...", "approval_policy": "auto", "sandbox_policy": "workspace_only"}}
```

客户端以收到 `session_configured` 视为握手完成(建议超时 30s)。
`id == ""` 表示全局事件(不隶属于任何 submission)。

离线测试:`ZHUYE_MODEL=mock`(可选 `ZHUYE_MOCK_SCRIPT=<JSONL 路径>`)
启动内置 mock provider,免 API key、无网络。

## 2. Submission 输入(stdin)

```json
{"id": "<client-uuid>", "op": {"type": "user_input", "items": [{"type": "text", "text": "你好"}]}}
```

- `id` 由客户端生成(建议 uuid);该 submission 产生的所有 turn 事件都以
  同一 `id` 回到 stdout —— 这是客户端路由事件的关联键。
- 不带 turn 语义的 Op(如 `register_tools`)也走 Submission 信封,`id`
  可以为任意值(其错误回报用全局 `id=""`)。
- **坏行不终止会话**:JSON 解析失败时 serve emit `id=""` 的
  `error` 事件(`code: "invalid_submission"`),继续读下一行。

常用 Op(完整定义见 `crates/protocol/zhuye-protocol/src/op.rs`):

| op.type | 字段 | 说明 |
|---|---|---|
| `user_input` | `items: [{type:"text", text}]`, 可选 `thread_settings` | 提交一轮对话 |
| `interrupt` | 可选 `child_id` | 打断当前 turn |
| `tool_approval` | `id`, `decision` | 响应审批请求。`decision` 是 `ReviewDecision`(serde snake_case):字符串 `"approve"` / `"approve_for_session"`,或对象 `{"deny":{"reason":"..."}}` |
| `register_tools` | `tools: [{name, description, parameters}]` | **serve-local**,注册客户端自定义工具 |
| `tool_execution_response` | `call_id`, `output` | **serve-local**,远程工具执行回执 |
| `shutdown` | — | 优雅退出(见 §6) |
| `rescan_extensions` | `skills: bool`, `commands: bool` | **v1.x 新增(additive)**:运行时重扫扩展目录(skills 按启动目录序替换文件型技能,插件注入与内置包不受影响);不开 turn,应答 `extensions_rescanned` 后 `submission_closed` 收尾。`commands` 位当前恒回 `false`(命令正文实时读盘) |

`register_tools` / `tool_execution_response` 由 serve 进程在 stdin 循环里就地
处理,**不进** core 的 submission loop。注册失败的两种情况以全局 `error` 事件
回报,工具被跳过:

- `code: "tool_name_invalid"` —— 空工具名;
- `code: "tool_name_conflict"` —— 与内置或已注册工具撞名(不覆盖,客户端换名重试)。

注册成功没有回执事件(静默接受)。

## 3. Event 输出(stdout)

```json
{"id": "<submission-id>", "msg": {"type": "agent_message_delta", "delta": "你"}}
```

turn 事件(带 submission `id`)按时间序输出,典型一轮:

```
turn_started → [thinking_delta | agent_message_delta | agent_message]*
             → [tool_call_args_delta* → tool_call_begin → tool_execution_request? → tool_call_end]*
             → [approval_request?] → turn_complete
```

SDK 关心的核心变体(完整列表见 `event_msg/mod.rs`):

| msg.type | 关键字段 | 说明 |
|---|---|---|
| `session_configured` | `session_id`, `model`, `provider` | 握手完成(全局) |
| `turn_started` | `turn_id` | turn 开始 |
| `agent_message_delta` | `delta` | 增量文本 |
| `agent_message` | `text` | 完整消息(非流式 provider) |
| `tool_call_args_delta` | `call_id`, `delta` | **v1.5 新增(additive)**:模型生成工具参数期间的原始 JSON 片段;客户端可渐进渲染,累积结果 = 后续 `tool_call_begin` 工具的完整参数。老客户端按未知 `type` 忽略即可 |
| `tool_call_begin` / `tool_call_end` | `call_id`, `tool_name`, `output`, `is_error` | 内置工具调用 |
| `tool_execution_request` | `call_id`, `tool`, `args` | **远程工具**:请求客户端本地执行(见 §5) |
| `approval_request` | `request_id`, `kind`, `risk` | 等待 `tool_approval` 回执 |
| `error` | `code`, `message` | 会话级错误 |
| `turn_complete` | `turn_id`, `status`, `usage` | turn 终态(`ok`/`cancelled`/`aborted`/`error`) |
| `turn_aborted` | `turn_id`, `reason` | turn 被打断(终态) |
| `shutdown_complete` | — | 会话收尾完成(终态,见 §6) |
| `submission_closed` | — | 该 submission 的 per-turn 通道已排空(终态,见下) |
| `extensions_rescanned` | `skills: usize`, `commands: bool` | **v1.x 新增(additive)**:`rescan_extensions` 的应答 —— skills 目录重扫后的条目总数;`commands` 当前恒 `false`。事件经 per-turn 通道 + 会话扇出双路送达(客户端按 submission id 去重) |

终态语义:turn 事件流以 `turn_complete` / `turn_aborted` 收尾。注意
`shutdown_complete` **携带 shutdown 那条 submission 的 id**(不是全局
`id=""`),订阅该 id 的消费者以此终结。

`submission_closed`(v1.3 新增,additive 非破坏)serve 在每条 submission
的 per-turn 通道排空后发出,**携带该 submission 的 id**。它让非 turn
操作(`compact` / `rewind` / `set_permission_mode` / `cycle_permission_mode`
/ goal / plan 等——这些操作没有 `turn_complete` 终态事件)的
`submit()` 迭代器也能确定性收尾;客户端把 `submission_closed` 与
`turn_complete` / `turn_aborted` 同等视为迭代器末尾。turn 类 submission
先被 `turn_complete` / `turn_aborted` 终结,其 `submission_closed` 到达时
监听者已摘除,被无害丢弃。

## 3.1 hooks 默认行为(serve 特有)

serve 是 SDK 嵌入入口,内置 hook(`verification` / `search_budget` /
`test_runner` / `plan_completion` / `langfuse_tracker`)**默认不启用**,
仅在 `~/.zhuye/config.toml [hooks].enabled` 显式列出时生效。exec / TUI
不受影响(未配置 = 全部启用)。这一差异的理由:`verification` 之类 Stop
hook 会在宿主 cwd 执行 `cargo test` 并否决 turn 完成,对嵌入方既慢又不
可预期。

## 4. 并发与顺序

- 允许上一个 turn 未结束就提交下一个 submission;core 侧并发执行,各
  submission 的事件以各自 `id` 路由,互不干扰。
- 所有事件经单一 writer task 串行写 stdout,**全局顺序有保证**。
- `interrupt` 会打断当前 turn(该 turn 以 `turn_aborted` 收尾)。

## 5. 远程工具执行流(自定义工具)

```
客户端                                zhuye serve
   │ ── submission: register_tools ──▶ │ (静默注册 RemoteTool)
   │ ◀──── tool_execution_request ──── │ LLM 调用该工具(call_id=c1)
   │ 本地执行 handler(args)            │ (oneshot 等待,默认 120s 超时)
   │ ── submission: ─────────────────▶ │
   │    tool_execution_response        │
   │    {call_id: c1, output}          │
   │ ◀──── tool_call_end (c1) ───────── │ LLM 拿到结果继续生成
```

- `tool_execution_request` 以**全局事件**(`id=""`)下发 —— 它属于工具调用,
  不隶属于任何 submission。
- `output` 为 `ToolOutput` wire 对象:`{content: [{type:"text", text}],
  is_error: bool, metadata: {}, elapsed_ms: int}`。SDK 内 handler 抛异常时
  应转成 `is_error=true` 的文本回执(让 LLM 看到错误而不是挂死)。
- 超时(env `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS`,默认 120s)未收到回执时,
  core 以错误 output 结束该工具调用,turn 继续但工具结果为错误文本。

## 6. 退出语义

优雅路径:客户端发 `shutdown` → serve 触发 core 取消并 emit
`shutdown_complete`(携带该 shutdown submission 的 id)→ 进程 exit 0。
若 shutdown 时仍有 turn 在跑,该 turn 以 `turn_aborted` 收尾。

- **stdin EOF** 等价于 shutdown(适合管道场景:关掉 stdin 即收尾)。
- 客户端 close 流程建议:写 `shutdown` → 关 stdin → 限时(约 2s)等进程退出
  → 超时 kill。
- 进程异常退出时,客户端应把所有挂起的 submission 消费者以错误终结。

## 7. 错误格式

```json
{"id": "", "msg": {"type": "error", "code": "invalid_submission", "message": "...", "details": {"line": "<截断预览>"}}}
```

`code` 现有值:`invalid_submission` / `tool_name_invalid` / `tool_name_conflict`;
其余为 core 既有错误码(见 `ErrorEvent`)。错误事件不终止会话,客户端决定
是否重试或关闭。
