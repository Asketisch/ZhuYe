# Agent 使用场景与验证清单

本清单按用户任务而非 crate 罗列场景。确定性测试使用真实 `zhuye`
二进制、隔离目录和 mock 模型或本机 HTTP 网关;本机网关复现 provider
协议,不代表真实模型输出质量或外部服务行为。功能明细和未覆盖项以
`COVERAGE.md` 为准。

| 用户场景 | 已验证的行为 | 自动化入口 |
|---|---|---|
| 单轮、多轮对话 | 握手、事件顺序、终态、跨轮上下文 | `functional/test_serve.py` D1–D5、S1–S3 |
| 修改工作区文件 | 读取 → 审批 → 写入 → 命令验证 → 回答;拒绝时文件不变且理由回到模型 | `functional/test_scenarios.py::test_读取文件审批修改并执行验证` |
| 并行调用宿主工具 | OpenAI SSE 参数完整或交错分片 → 两个请求同时在途 → 逆序回执 → 模型看到两个结果 | `functional/test_scenarios.py::test_并行远程工具逆序回执并恢复对话` |
| Responses 并行工具与续跑 | 交错参数、逆序回执、独立调用/结果项、重启后恢复;输出截断续写及未完成工具不执行 | `functional/test_responses_scenarios.py` |
| Anthropic 并行工具 | 两个 content block 的参数分片交错到达,按 id 正确归属并逆序回执 | `functional/test_anthropic_tools.py` |
| 重启后恢复上下文 | 工具调用与结果落盘,进程重启后同一会话续跑,模型请求仍含调用及结果 | 同一并行工具场景;`functional/test_recovery.py` R1–R6 |
| 长会话压缩后恢复 | 摘要落盘,重启后摘要器收到旧摘要并增量更新,模型收到新摘要与当前请求 | `functional/test_long_session.py` |
| 中断在飞工具后续跑 | 中断不等待远程工具回执;取消记录落盘,迟到的旧回执不会结束下一轮工具 | `functional/test_scenarios.py::test_中断远程工具后迟到回执不污染下一轮` |
| 中断待审批操作 | 未批准的写入不会发生,迟到批准无副作用,后续回合仍可继续 | `functional/test_scenarios.py::test_中断待审批写入后迟到批准无副作用` |
| 中断委派任务 | 父回合或定向中断后子代理变为 cancelled、模型连接关闭,下一轮仍可继续 | `functional/test_multiagent_scenarios.py` |
| 子代理失败与恢复 | 失败原因回流父代理,部分回复不冒充成功;临时限流可恢复,失败历史重启后仍可用 | `functional/test_subagent_failure.py` |
| 子代理上下文 | 默认隔离;按请求传递长历史,在工具结果边界补齐调用与回执 | `functional/test_subagent_context.py` |
| 多级委派 | 子代理继续委派孙代理,成功/失败逐级传播、过程文本不混入最终结果;定向取消后状态与连接一致 | `functional/test_nested_subagents.py` |
| 并行委派混合结果 | 两个同角色子代理同时运行,成功与失败结果均按调用 ID 回到父代理并继续推理 | `functional/test_parallel_subagents.py` |
| 子代理工具审批 | 子代理写入请求转发到父会话;批准执行、拒绝无副作用,结果继续交给父代理 | `functional/test_subagent_approval.py` |
| 子代理人工提问 | 子代理自由文本或选择题进入父会话,标准 response Op 回答后继续完成 | `functional/test_subagent_human_input.py` |
| 子代理权限继承 | 父会话显式拒绝的写入不能在子代理中改走审批执行,文件无副作用 | `functional/test_subagent_permissions.py` |
| 子代理沙箱边界 | 父级放行写入后,子代理仍不能越界写文件;工作区内写入继续成功 | `functional/test_subagent_permissions.py` |
| 中断模型响应 | 等待响应头和流式输出时关闭连接,只产生一个终态,已输出文本落盘并在下一轮恢复 | `functional/test_model_cancellation.py` |
| 模型服务限流 | 本机网关返回 HTTP 429 → 切换备用凭证 → 下一轮继续工作并携带前文 | `functional/test_scenarios.py::test_模型限流后切换凭证并继续下一轮` |
| 模型流内故障 | Anthropic 流内限流、过载和提前断流;Responses 流内失败与配额耗尽后的凭证切换 | `functional/test_anthropic_errors.py`;`functional/test_responses_scenarios.py` |
| 部分回复后断流 | 三种 provider 在干净 EOF、连接截断或流内报错时,部分回复先落盘再报错,下一轮可继续 | `functional/test_stream_failure.py` |
| 损坏的工具参数 | 无效 JSON 不调用宿主工具,错误回流模型;同批有效工具照常执行 | `functional/test_invalid_tool_arguments.py` |
| 工具全量驱动(exec 参数面) | mock 逐工具驱动全部内置工具:输出与副作用、read-before-edit 四态、OS 沙箱越界、ast 与信息类、notebook、worktree、checkpoint/rewind、人工输入 headless 优雅报错 | `functional/test_tools.py` T30–T38 |
| 用户输入变体 | `local_image` 就地读盘进模型上下文;`skill` 注入正文;`question_answer` 内联解除挂起提问 gate | `functional/test_serve.py::test_A30_inline_question_answer经user_input回答`;`test_A31_local_image进入模型上下文` |
| 整段消息变体 | 非流式 provider 的 `agent_message` 全文事件不经 delta,文本等价进入下一轮模型上下文 | `functional/test_serve.py::test_D13_整段消息agent_message变体` |
| 拒绝与沙箱 | 审批拒绝、资源规则、路径逃逸、沙箱拒绝后的错误和无副作用 | `functional/test_tools.py` T5–T9、T13–T19、T24–T26 |
| 持久化和回退 | rollout、权限轨迹、rewind、标题、导出、cron 持久化与损坏保护 | `functional/test_persistence.py` |
| 定时任务触发 | 到期 cron 注入真实新回合并完成,持久化 last_fired / next_fire | `functional/test_persistence.py::test_P14_cron到期注入真实回合` |
| Autopilot 调度 | 配置 opt-in 后按 cron 到期注入真实回合,history 终态回填 success | `functional/test_persistence.py::test_P15_autopilot到期注入真实回合` |
| 后台任务与结果回流 | `run_in_background` 立即回执任务 id;成功输出回合边界注入模型上下文并排空队列;失败任务经 background_status 查询可见,不被注入 | `functional/test_tools.py::test_T29_后台bash任务与边界注入` |
| 多代理和任务 | 子代理、讨论、任务、目标、侧通道查询/交互/取消 | `functional/test_orchestration.py`;`functional/test_serve.py` A13–A24 |
| 集成入口 | MCP、WASM、插件导入、Shell、Git Tools、KMS、Computer Use 降级 | `functional/test_import.py`;`functional/test_v110.py` V12 |
| 集成链路参数面 | MCP 生命周期与调用、workspace skill 激活、session notes 落盘、bubble 自动放行、配置热重载、plugin marketplace 全链路 | `functional/test_orchestration.py` O11–O16 |
| 嵌入 Python / TS | 真二进制握手、注册工具、并发、审批、打断、进程崩溃;SDK 逻辑和类型 | `tests/sdk-python/`;`tests/sdk-ts/`;`sdks/*/tests/` |
| 原生 Python 集成 | wheel 构建和导入、多轮及工具回合、用量、等待期间释放 GIL | `tests/native-python/` |

## 运行与边界

- `./tests/run_all.sh` 跑能力层、workspace、CLI、双 SDK 和原生 Python;`functional`
  子命令是 CI 功能门禁,workspace 测试由 CI 的独立 job 执行。
- `./tests/run_all.sh functional-layer` 只跑原始协议能力层和测试设施
  自检。脚本会构建当前源码和必需插件;显式指定 `ZHUYE_BIN` 时调用方
  需提供匹配的插件和导入器。缺少前置条件会失败。
- `ZHUYE_LIVE=1 ./tests/run_all.sh live` 使用外部真实模型,需要网络和
  可用服务;默认不运行,限流或模型不支持工具可能导致跳过。
- 平台特有沙箱、真实设备权限、网络故障、真实模型决策以及外部服务
  的全部状态不能由离线夹具穷举。当前 CI 主要运行 Linux,macOS 和
  Windows 的平台专项需要独立环境。

## 下一批重点

1. 补充更多兼容网关的错误语义和私有协议差异,断言 provider 请求、
   事件和副作用。
2. 补充多代理并发场景下的复杂资源失败和沙箱边界组合。
3. 建立可重复的 macOS / Windows 平台测试与受控真实模型测试,记录
   能力不支持、不可用和断言失败,不要把跳过计为已覆盖。
