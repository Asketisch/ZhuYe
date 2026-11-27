# 功能覆盖矩阵(测试 ↔ 功能面映射)

本矩阵记录当前测试对外功能和流程的覆盖情况,未覆盖项见第 8 节。
覆盖口径:每条功能面至少有一个**字段级/行为级断言**(非"退出码 0")的
用例;断言失败即失败。全程离线(`ZHUYE_MODEL=mock`)+ 隔离 HOME。

## 0. 覆盖分层:谁测什么

| 层 | 位置 | 测谁 | 依赖 SDK? | 规模 |
|---|---|---|---|---|
| **能力覆盖层**(主) | `tests/functional/` | **Rust 代码在真产物上的行为**(原始 JSONL wire)与测试设施自检 | ❌ | 432 条(pytest 收集,含设施自检) |
| CLI 参数面 | `tests/rust/*.sh`、能力层 | 18 个子命令的参数矩阵与输出契约 | ❌ | 128+ 断言 + C10–C19(四份 bash 深度套件已全部迁入能力层) |
| **事故回放与语义差分层** | `tests/verification/` | 新旧版本二进制在同一确定性输入上的**行为语义对比**(5 个真实事故回放 + 变异自证) | ❌ | 自测 29 条 + 差分用例 5 条 |
| SDK 绑定层 | `tests/sdk-python/`、`tests/sdk-ts/` | 两个**客户端自己**的行为 | ✅ | Python 19 条 / TS 19 条 |
| SDK 单测 | `sdks/*/tests/` | SDK 纯逻辑(mock serve),随包发布 | ✅ | Python 28 / TS 40 条 |
| 原生 Python 绑定 | `tests/native-python/` | 安装实际构建的 PyO3 wheel,验证 Python 接口 | ✅ | 2 条 |
| crate 单测 | `crates/**/tests/` | 内部单元与集成 | — | 既有 |
| **实模型演练层**(可选) | `tests/live/` | **真实模型**下的引擎行为(协议形状) | ❌ | 10 条 pytest + 28 场景(cli_e2e 套件,本地专用) |

**新增能力断言只落能力覆盖层一处** —— 不要在 SDK 测试里重复写协议/能力断言。
理由:SDK 自身的 bug 会把 Rust 的 bug 遮住(见 §6 已修复项 D1)。

CI 功能门禁构建 KMS、Computer Use(无 real feature)、Shell 和 Git Tools
插件二进制,使已有插件链路用例在干净环境中可运行。SDK 协议文档一致性
检查 `release_sdk.sh check_docs` 也在普通 CI 中执行。

⚠️ **实模型演练层不算能力覆盖** —— 它断言的是**协议形状与运行时行为**
(事件序 / 终态 / usage 字段 / 工具成对),**不断言具体回答文本**。
能力断言仍然只在 `tests/functional/`。两层不重叠、不互相替代:
能力覆盖层是 CI 强制门禁(离线、确定性),实模型层是可选手动/夜间门禁
(需外网与免费额度,默认 SKIP)。

## 1. 能力 × 覆盖层矩阵

| 能力域 | 能力覆盖层 | CLI 参数面 | SDK 绑定层 | crate 单测 | 实模型层 |
|---|---|---|---|---|---|
| 基础通信(握手/单轮/多轮/事件流/终态/工具参数增量/整段消息) | D1–D5、D9–D10、D13 | cli_core | K1–K2、K5 | ✅(build/build_async 对齐断言) | L1–L4 |
| EventMsg 全变体线格式 | —(逐场景事件断言见能力层) | — | 双 SDK 共用最小及可选字段样本,验证透传和未知字段 | ✅(`tests/contracts/events.json` 55 变体 + 完整字段样本 + serde roundtrip) | — |
| Prompt cache 保温(排程/重放/观测门控/零污染/脚本行不消耗) | `tests/functional/runtime/test_cache_warming.py`(默认关闭零行为、scheduled/executed 字段级、`no_cache_hits` 决策跳过、rollout 零新增、第二轮脚本行不被偷) | — | 双 SDK `CacheWarming` 类型 + README 事件表 | ✅(zhuye-llm `evaluate` 表驱动 + mock `warm_replay` 旁路;zhuye-core 调度器 8 条:重放形状/取消/代际 stale/关闭 no-op/scheduled 事件/不可重放/事件字段/父令牌级联) | — |
| 流锚点恢复(可重试流错误 + 可见增量 → 丢弃留痕 + 4 恢复事件 + 瞬态分类) | mock 流错误场景隐含覆盖(v1.10 重试);wire 层 `tests/sse_timeline.rs`(时间线吐帧 / 中途断流 TCP) | — | `StreamRecovery` 类型 + README 事件表 | ✅(`stream.rs` recovery_tests:事件序 / discarded 字符数 / 瞬态分类;协议 serde roundtrip) | — |
| 压缩 rapid-refill 熔断(窗口 3 步 / 连续 3 次 → `compact_rapid_refill`) | — | — | — | ✅(`zhuye-compact/src/refill.rs` 表驱动 5 条:窗口/升级/清零/同步号) | — |
| 空/可疑模型结果防护(0 输出 + input>0 → `suspicious_empty_model_result`) | — | — | — | ✅(守卫条件单测随 core lib) | — |
| Steer 输入字节上限(200KB wire 实测 → `steer_input_too_large`) | — | — | — | ✅(协议常量 + 分支条件) | — |
| 工具结果预算(宿主原始结果、模型侧头尾预览及偏移、工作区溢出或中间截断；bash 分流丢弃标记) | ✅(`test_spill_readback.py` / `test_v1x_reliability.py`) | — | — | ✅(`result_budget.rs` 边界与非文本保护) | — |
| stop_turn 级联(工具 metadata 请求结算后收口) | — | — | — | ✅(tool_exec 置 force_final_answer 分支) | — |
| strict schema 折叠(不支持约束写进 description,折不掉降级) | — | — | — | ✅(`schema_fold.rs` 4 条:保留/移除/组合子放弃/嵌套) | — |
| tool-defer 按需加载(MCP 目录公告、注册门控、目标审批和非法转发拒绝) | ✅(`test_v1x_reliability.py`:门控、嵌套拒绝、MCP 审批与拒绝) | — | — | ✅(registry deferred_tool_names / queue 展开分支) | — |
| Prompt Snapshot(resume system 字节冻结 / 代际失效 / todos 拼回) | — | — | — | ✅(model_call 应用/捕获分支;`resume::last_prompt_snapshot`) | — |
| 输入守恒 ledger(终态 InputDisposition / 崩溃窗口对账 / 永不自动重放) | — | — | — | ✅(`resume::unsettled_input_turns` + submission_loop 终态分支) | — |
| provider 能力回归矩阵(文本/工具/JSON/上下文锚定 4 维考卷) | — | — | — | — | `tests/live/test_live_provider_matrix.py`(ZHUYE_LIVE 门控,默认 skip) |
| models.dev 目录后台刷新(24h TTL / mock 与 ZHUYE_MODELS_DEV_REFRESH=0 跳过) | mock 模型零网络 | — | — | ✅(headless spawn_models_dev_refresh 分支) | — |
| 协议信封与 Op 随机往返 | — | — | — | ✅(proptest:Op / Submission / Event serde) | — |
| 真实任务效果与版本比较 | 本地网关驱动真进程的评分设施自检 | — | — | ✅(zhuye-eval 评分器) | `agent.quality`:12 个合成任务、默认三次重复、客观产物验收及失败轨迹;需显式启用 |
| 质量门禁判定(observe/enforce) | — | — | — | ✅(`tests/eval/eval_policy.py` 纯函数 + `test_policy.py` 合成报告 24 条;退化可阻断、infra 不误判) | — |
| 已修复事故回放与语义差分(Ask/Deny 击穿 / 重定向写 / 默认档语义 / 崩溃恢复 / 工具错误恢复) | `agent.contract.differential`:新旧二进制重放同一 mock 输入,比较回合终态/工具序列/审批/文件副作用/事件类型;INC-002 独立验证审批与拒绝后的文件状态;需 ZHUYE_DIFF=1 | — | — | ✅(`tests/verification/test_differential.py`:9 类变异及两版同错必被抓 + 语料自洽) | — |
| Protocol v0 加变体的差分批准通道(wire 事件类型 + rollout 记录类型) | — | — | — | ✅(`event:<TYPE>` / `rollout:<TYPE>` 逐条批准:未批准的新类型必被抓、基线丢失不可豁免、剔除已批准类型后顺序与条数仍严格) | — |
| 提示词选择的持久化落盘 | — | — | — | ✅(会话启动落 `prompt_selection_changed` 且 `selection: null`;`prompt_command select` 后追加携带实际选择) | — |
| 协议事件变异回归(截断 / 字节替换 / 判别子破坏 / 多种子多重变异) | — | — | — | ✅(`event_mutation_fuzz`,默认 1 万次可配置) | — |
| Rollout 只读目录写失败 | — | — | — | ✅(返回错误、已有会话文件字节不变) | — |
| Rollout 磁盘满写失败 | — | — | — | ✅(`/dev/full` 真实 ENOSPC,返回错误且不 panic) | — |
| 跨实例任务存储锁竞争 | — | — | — | ✅(`file_store_lock_contention`:ID 唯一、两任务均落盘) | — |
| 会话中毒检测(输出、错误率、超时及传输异常) | D10 | — | — | ✅(完整文本指纹、回合滑动窗口及迭代上限) | — |
| 并发与路由 | S1–S3 | — | K15(双 SDK 真二进制隔离),TS 写入前快速响应及并发 EOF | — | — |
| 串行回合调度 | 真协议验证问答与混合文本分流、历史顺序、排队中断及回退清空 | — | — | ✅(FIFO、无 recorder 历史、默认并发、中断及关闭) | — |
| 任务树预算 | 真协议验证预留失败、真实重试结算、父子并发共享账本、空闲保温结算快照与子代理派发上限 | — | — | ✅(原子预留、超额、摘要、截止、最终答复、隔离失败释放派发额度) | — |
| 来源与动态上下文 | 真协议验证未知来源模式拒绝启动、工作区污染观察、加强模式、恢复、动态待办压缩重连及回退、同步/异步子代理来源交接 | — | — | ✅(模式校验及门面、来源分类、污染状态、待办快照并发提交顺序) | — |
| 同批工具读写屏障 | 本地网关驱动 read → write → read,核对两次回执及最终文件 | — | — | ✅(读写实际顺序、连续安全调用并发) | — |
| 工具执行严格记录与恢复对账 | 真实进程在审批等待、执行中、产生副作用后和终态落盘后强杀,核对记录、未知事件与副作用产物 | — | 双 SDK 使用共享事件样本验证透传与未知事件降级 | ✅(无可靠 recorder / 写失败拒绝、终态写失败通知、跨回合 ID 隔离、无效 UTF-8 恢复、异步门面的插件子会话独立账本和严格记录继承) | — |
| 子代理工作区隔离 | 本地网关验证 worktree 创建和子会话落盘目录;非 Git 目录或无提交时核对工具错误和未派发 | — | — | ✅(显式隔离、创建失败释放并发槽位、共享工作区继承与工具来源、异步门面 Git 根目录探测失败拒绝派发) | — |
| 并行远程工具、逆序回执与会话恢复 | 场景测试(本地 OpenAI SSE 网关,完整及交错参数分片、记录落盘和恢复后请求) | — | — | OpenAI 分片重组 | — |
| Responses 并行工具、截断及故障恢复 | 真实进程 + 本地 Responses 网关:交错分片、工具历史、重启恢复、输出上限、未完成工具、错误与配额切换 | — | — | ✅(标识和最终参数一致性) | — |
| Anthropic 并行工具 | 本地 Anthropic 网关交错 content block 分片,验证逆序回执、参数归属和后续请求历史 | — | — | ✅(index/id 归属与非法标识) | — |
| Anthropic 流内错误和提前断流 | 本地网关模拟过载、限流及终态前断流,断言凭证切换和后续对话 | — | — | ✅ | — |
| 真实 HTTP 限流后凭证切换及下一轮 | 场景测试(本地网关返回 429、备用凭证及后续上下文) | — | — | 路由策略与 provider 单测 | 可选实模型演练 |
| 长会话摘要压缩及恢复 | 本地网关记录摘要请求、落盘摘要、重启后增量摘要和模型请求 | — | — | ✅(压缩策略及回放单测) | — |
| 读取、审批、修改和验证 | 场景测试(批准后真实修改与命令验证;拒绝时文件不变且理由回流) | — | — | 工具权限及审批单测 | — |
| 中断在飞远程工具或待审批写入并继续会话 | 场景测试(取消终态、落盘、迟到回执或批准及下一轮隔离;进程强杀后的缺失回执明确提示结果未知和可能的副作用) | — | 双 SDK 打断用例 | 队列级取消与其他回合隔离 | — |
| 中断父回合或定向取消子代理 | 本地网关阻塞子代理,验证父回合终态、子代理状态、连接关闭及后续回合 | — | — | 启动中取消释放槽位、无状态中心取消令牌 | — |
| 子代理失败与上下文传递 | 调用前/部分回复后失败、重启恢复、临时故障恢复;默认隔离、尾部消息和工具配对 | — | — | 上下文边界与工具访问隔离 | — |
| 已完成子代理续派 | 真协议验证主代理获得子会话 ID,跨父回合续派后子代理看到之前的输入和回答,结构化结果正文保持不变 | — | — | ✅(无 recorder 历史、同角色校验、并发拒绝、任务树预算) | — |
| 多级委派与定向取消 | 父→子→孙成功和失败传播、最终回复不混入过程文本;分别中断父回合、子代理、孙代理并核对终态与连接 | — | — | ✅ | — |
| 并行子代理混合结果 | 两个同角色子代理同时执行,一个成功一个失败;父代理收到配对结果并继续推理 | — | — | ✅ | — |
| 异步子代理派发与回合挂起激活 | `mode="async"` 立即返回占位回执;父 agent 在子代理运行期间继续推进;回合在无后续动作处挂起,子代理结果**落盘**并注入上下文后**同轮**续跑(全程单次 `turn_complete`),下一轮历史里仍能看到结果;中断挂起中的父回合**级联取消**在途异步子代理(状态 `cancelled`、模型连接关闭),陈旧条目不波及后续回合 | — | — | ✅(在飞表:终态 / 按回合放弃 / 迟到终态丢弃 / 等待者唤醒;后台任务随父回合取消丢弃子代理;`check_stop` 落盘注入、单条消息、超长预算、取消直接收尾;`GraphNode::is_natural_stop`) | — |
| 中断模型连接及流式输出 | 响应头前和输出中途取消、断开连接、单一终态、部分文本落盘及续跑 | — | 双 SDK 打断用例 | ✅ | — |
| 模型流提前结束与部分输出 | 三种 provider 的干净 EOF、连接截断及流内错误;**v1.10:可见输出后同样自动重试**(先发 `agent_message_reset` 清半成品,重试输出不拼接);不可重试错误才落盘报错并续跑;重试烧满预算以 `error + turn_aborted` 收场 | — | 双 SDK 验证错误后继续 | ✅ | — |
| 无效工具参数 | 三种 provider 返回损坏 JSON 时,有副作用的宿主工具不执行、错误回流模型,同批有效工具正常运行 | — | — | ✅ | — |
| 内置工具调用与 OS 沙箱 | T1–T2、T8–T27、T30–T38(exec 全工具驱动) | —(tools_drive 已迁入能力层) | — | ✅(路径固定与文件替换) | T1–T3 |
| 文件句柄与 LSP 路径 | T24 | — | — | ✅(目录替换、读取大小上限、越界链接及 LSP 文档内容) | — |
| 技能面(内置包开关 / 文件 skill 激活) | SK1–SK4、O12(exec 路径) | —(integrations 已迁能力层) | — | ✅ | — |
| 远程工具(注册/往返/失败/超时/MCP 图片) | T3–T4、T21、T23、X4 | —(serve_ops 已迁能力层) | K3、K8、K13(双 SDK 图片) | ✅ | — |
| MCP 对端断开 | 工具调用时子进程退出：错误结果及回合完成 | — | — | `test_runtime_hardening.py`:真实 stdio 子进程断开后共享取消信号触发 | — |
| MCP 初始化超时 | 无响应服务在握手或首次列工具时超时，失败事件含服务名、错误和重试标志，会话可继续；HTTP 失败不虚报重试 | — | — | `test_runtime_hardening.py`:本地子进程分别阻塞 initialize 和 tools/list；HTTP 超时不虚报重试 | — |
| 评测流水线离线自检 | 固定 12 条真实任务在 mock provider 下跑通沙箱 / 验收 / 评分 / 门禁全链路,产出成功率、耗时、工具调用与前后差值 | — | — | `tests/eval/run_offline.sh`(并入 functional 套件);真模型基线见 docs/eval-baseline.md | — |
| MCP 客户端 OAuth(http) | 本地 HTTP 服务要求 Bearer:凭据齐备时自动发现端点并换 token、握手成功;无凭据时明确上报「需要授权」 | — | — | 本地 HTTP 服务(裸 TCP)验证元数据发现 → token 端点 → 带 token 握手 | — |
| MCP 工具清单动态刷新 | 服务端 `notifications/tools/list_changed` 后：新工具可调用、下线工具不可见 | — | — | `test_runtime_hardening.py` + mock `MOCK_LIST_CHANGED` | — |
| MCP 用户交互请求(elicitation) | 宿主收到 `mcp_elicitation`（form 字段逐项 / link 链接）并回 `mcp_elicitation_response`，工具结果带回用户输入；宿主不回执时按 TTL 以 `cancel` 了结，回合不再挂起 | — | — | `test_runtime_hardening.py`:本地 stdio 服务在工具调用中发起 `elicitation/create`，回执与超时两条路径 | — |
| MCP 资源与取消 | 本地服务资源列表、混合文本和二进制、嵌入资源逐字段传递；中断时服务端收到对应请求 ID 的取消通知 | — | — | `test_runtime_hardening.py`:stdio 资源往返 + 无响应读取取消及服务端通知 | — |
| 审批交互(approve / deny / bubble / shell 规则) | T5–T7、shell 动态语法与只读副作用命令审批、O14(bubble)；`test_runtime_hardening.py` 覆盖动态语法宽放行仍需审批 | —(integrations 已迁能力层) | — | ✅ | — |
| 敏感路径保护(元数据只读) | 可写根内 `.git` 不可写、普通文件可写;平台无法强制时如实上报并在宿主强制要求下失败关闭；bash `metadata.path_protection` 上报 enforced/unsupported | — | — | macOS Seatbelt deny 规则真机验证;Linux 报告 `unsupported`(规则按位合并)并在 `strict_paths` 下拒绝执行；`test_runtime_hardening.py` 校验 metadata | — |
| Linux 子进程网络沙箱 | 前台 bash 本地 TCP 禁止与显式允许 | — | — | seccomp 对 TCP/UDP/Unix socket、继承描述符、io_uring 及失败关闭；前后台策略一致；`test_runtime_hardening.py` 前台策略端到端 | aarch64 真机容器；x86_64 待实际执行 |
| Linux 工作区外写入 | 缺失授权目录时，即使非严格且允许联网也拒绝运行 | — | — | Landlock ABI v3 真实子进程直接 truncate：外部拒绝、内部允许；符号链接越界拒绝、不同严格与网络配置下缺失授权目录禁止启动、`/dev/null` 可写；`test_runtime_hardening.py` 非严格联网失败关闭 | aarch64 真机容器；x86_64 模拟器不支持 Landlock 时验证失败关闭 |
| Unix 命令后代清理 | 前台超时、中断及宿主断开后，同组后台后代不得继续写入 | — | — | `test_runtime_hardening.py`:异步执行任务丢弃与后台会话取消均清理同组后代 | 主动脱组的后代与 Windows 进程树待覆盖 |
| 模型工具输出预算与溢出读回 | 远程工具大输出、宿主原文与模型预览、溢出文件、跨进程恢复及 UTF-8 字节分页 | — | — | 文本预算、图片保留、写入失败和符号链接拒绝 | — |
| 审批与中断交错 | 固定种子重排批准、拒绝和中断;验证单一终态、迟到审批及文件副作用 | — | — | 等待器释放 | — |
| 子代理审批转发 | 子代理 `write` 的请求进入父提交;批准后落盘、拒绝后无副作用 | — | — | ✅(会话共享 waiter / session allow) | — |
| 子代理人工输入转发 | 子代理自由文本与结构化提问进入父提交,标准 response Op 能唤醒工具并继续 | — | — | ✅(共享 question/input waiter) | — |
| 子代理权限与沙箱继承 | 父级 `write` 拒绝规则在子代理生效,不弹审批且无文件副作用 | — | — | ✅(父配置安全字段继承) | — |
| 子代理文件系统边界 | 父级放行 `write` 时,工作区外仍拒绝且无副作用,工作区内写入成功 | — | — | ✅(父配置安全字段继承) | — |
| 会话状态 Op(22 Op)+ Activity 审计落盘和轮转 | A1–A17、A25–A29(A1b 验证拒绝副作用，A1c 验证规划模式与独立审批策略) | —(serve_ops 已迁能力层) | K4、K9;双 SDK 回合设置类型检查 | ✅(activity 7 例) | — |
| 重连与恢复(resume / -c / -r / 崩溃 / EOF) | R1–R6 | cli_core(exec --resume) | — | ✅ | P2 |
| 工具在途崩溃后恢复 | 本地网关与真进程:已落盘历史可见、恢复后工具调用和回执配对、下一轮成功 | — | — | — | — |
| 容错与失败恢复 | X1–X6 | — | K7 | ✅ | X1 |
| 持久化(rollout / 截断 / 索引 / 标题 / 导出 / cron store / cron 触发 / autopilot) | P1–P15、C13(session 生命周期) | — | — | ✅ | P1–P2 |
| 循环守卫参数预览脱敏与停滞阻断(库调用方注入) | —(CLI 未暴露配置) | — | — | ✅(提醒、阻断、进展后再次停滞及零长度窗口) | — |
| 独立审计库(跨进程记录、顺序与损坏文件保护) | —(库接口无 serve 入口) | — | — | ✅(多进程及自定义 sink) | — |
| 加密密钥存储(CLI 接线 + 损坏字段处理) | C19(secrets 全链路) | — | — | ✅(zhuye-secrets) | — |
| 文件权限规则并发持久化 | —(库接口无 serve 入口) | — | — | ✅(多实例并发更新) | — |
| Autopilot 调度(首次到期 / 并发占位 / 发送失败) | — | — | — | ✅(zhuye-stream) | — |
| KMS 知识库插件(install → serve --plugin → MCP 读写 → 磁盘落盘) | I12 | —(独立二进制 `zhuye-kms-server`) | — | ✅(zhuye-kms 28 例) | — |
| Computer Use 插件(headless 降级及无效输入拒绝;真机截屏走 real feature 手动验) | I13、I23 | —(独立二进制 `zhuye-computer-use-server`) | — | ✅(降级 2 例) | — |
| Browser 插件 shim(降级态 wire 协议全字段:initialize / tools/list 上游命名 / isError 修复指引 / stdin 关闭即退出;真机 open→snapshot→click 走捆绑二进制手动验) | F39(v0.2 起页面寻址语义归上游 agent-browser,submodule 钉版) | —(独立二进制 `zhuye-browser-server`) | — | ✅(降级 6 例 + crate 单测 12 例) | — |
| Shell 会话插件(run → read → kill 生命周期及后台子进程清理) | I14、I16 | —(独立二进制 `zhuye-shell-server`) | — | ✅(zhuye-shell 7 例) | — |
| Git 工作流插件(分支/PR 探测 + tag checkpoint + Stop 自动 checkpoint hook 永远退出 0) | I15 | —(独立二进制 `zhuye-git-tools-server` + `zhuye-cp-hook`) | — | ✅(zhuye-git 21 例 + git-tools 11 例) | — |
| Markdown Git 指令与字面代码隔离 | —(库接口无 serve 入口) | — | — | ✅(代码块、行内代码、链接与引用定义、转义和参数校验) | — |
| Git 后台命令输出上限与超时 | I15(大仓库截断时统计不可用) | — | — | ✅(双输出流有界读取及超时终止) | — |
| Git checkpoint 标签失败与回退安全快照 | I15、T20(子目录快照、干净提交可回退、安全快照失败停止) | — | — | ✅(拒绝回退、保留普通提交及子目录回退恢复) | — |
| 插件 Hook 持久化禁用及恢复 | C8 | — | — | ✅(延迟注册及重新注册) | — |
| 插件显式加载(`serve --plugin` 覆盖 config) | I7 | — | K9(Python)/ K10(TS,plugins) | — | — |
| 门控型 bundled 插件(默认态零变化金测试 / disabled_plugins 注册门控 / disallowed_tools 分层叠加 / verify-hooks 双路径 Stop 否决续作) | G1–G4(`test_gated_plugins.py`) | — | — | ✅(zhuye-plugin bundled 3 例 + zhuye-tools builtin 门控 2 例 + zhuye-hooks enable_hook_names 1 例 + headless goal_queue 判定 2 例) | — |
| Python SDK 选项位置参数兼容性 | — | — | ✅(原参数顺序及新增插件选项的 SDK 单测) | — | — |
| TS prompt 本地超时 | —(仅客户端行为) | — | ✅(慢握手、慢写入、并发及迟到事件、非法超时值) | — | — |
| Rust Builder 旧版 `with_defaults` 构造链、取消和会话订阅 | —(库接口无 serve 入口) | — | — | ✅(公开接口集成测试;mock 离线运行构造、线程 ID、取消与订阅用例) | — |
| Rust 同步与异步 Builder 的脱敏配置 | —(库接口无 serve 入口) | — | — | ✅(真实工具输出及自定义脱敏标记) | — |
| PyO3 wheel 与同步调用 | —(库接口无 serve 入口) | — | wheel 导入、配置、多轮、工具回合、用量及释放 GIL | ✅(类型与接口单测) | — |
| SDK 新能力面(side-channel 查询、终态及交互;双 SDK 真二进制绑定) | A13、A15–A24 | — | K10(查询)、K11(终态)、K12(审批和提问),双侧 | ✅(侧通道继承沙箱、脱敏、权限模式、分类器、启动上下文及回执清理) | — |
| 编排(子代理/讨论/任务/团队/squad 委派/流水线/goal) | O1–O10、C15(task/team)、C18(pipeline/discussion) | cli_modules | — | ✅(squad / actor、跨进程认领与任务并发更新) | — |
| CLI 能力面与深度参数面 | C1–C9、C10–C19(原 cli_deep.sh 194 断言迁入 + secrets 接线) | —(四份深度套件已全部迁入能力层) | — | — | — |
| 模型发现(清单接口 / 免费判定 / 动态 model) | — | models_* | — | ✅ | — |
| 模型计价(费率表公开 / 费率卡 / cost 聚合) | — | — | — | zhuye-llm pricing(6 例) | — |
| git 能力族(checkpoint tag+安全快照 / 分支 PR 探测 / 指令解析) | — | — | — | zhuye-git(16 例)+ zhuye-tools builtin_tools(1 例) | — |
| 缺陷修复回归(预算耗尽显性化 / 配置隔离 / 工具裁切 / 提示词关闭 / mock 显性化) | ✔(12 条,见 `test_bugfixes.py`) | config(show 的 ZHUYE_HOME 断言) | — | ✅(provider 层非 SSE 校验 / 版本段拼接 / 采样参数默认值有 wiremock 回归) | — |
| 提示词选择与会话恢复 | A12(默认、手动、智能回退、无效方案、重扫及恢复) | — | — | ✅(提示词解析与请求组装) | — |
| 模型自动输出预算与配置优先级 | ✔(本地 HTTP 网关断言模型、输出字段、中文输入余量,见 `test_bugfixes.py`) | — | — | ✅(模型上限、余量、未知型号及请求体) | — |
| 提示注入检测库(凭据外传规则) | —(尚无运行时入口) | — | — | ✅(zhuye-sanitize 检测及误报边界) | — |
| v1.10 功能:goal+队列插件 / 上下文窗口 / 思考强度分级 / 空 key | V1–V10 | — | ✅(effort 阶梯字面量双侧断言) | ✅(llm 能力表钳制 / override 表 / builder 免 key / queue 调度) | — |
| v1.11 基建#9:AGENTS.md 默认读取(内置 UserPromptSubmit hook 注入 `## Project Instructions`) | ✔(4 条,见 `test_agents_md.py`:默认注入 / 用户级+项目级顺序 / `[active] agents_md=false` 关闭 / 无文件不注入空段) | —(serve 内部行为,非 CLI 参数) | —(无 Op/EventMsg 变更) | ✅(zhuye-prompt 段落插桩 3 例;zhuye-exec builtin_hooks 8 例:来源标注 / 空白与缺失 / 超长截断 / 合并顺序 / 单源与双缺失 / hook 首轮注入次轮放行 / 无文件不注入 / 跨会话召回 hook 命中·空查询·排除自身;zhuye-config 三态解析 3 例 None/false/true) | — |
| v1.11 基建#11:显式完全托管模式(`exec/serve --full-auto`/`--yolo`,Bubble 启动 + 初始模式通告 + Prompt 工具 bubble 自动批准) | ✔(5 条,见 `test_full_auto.py`:启动通告 / 别名等效 / ZHUYE_APPROVALS=1 下 write 免阻塞自动批准 / 默认不发通告 / 与 --plan-mode 互斥) | ✅(clap 互斥由用例 5 直接驱动) | —(复用既有 PermissionModeChanged / PermissionBubble 事件,无新变体) | ✅(zhuye-exec headless `initial_permission_mode` 纯函数 1 例:三分支与 full_auto>plan_mode 优先级;zhuye-core `with_initial_permission_mode` Bubble 例;submission_loop 通告分支由功能层覆盖) | — |
| v1.11 基建#6:队列支持多 goal(`queue add_goal` + driver 探针驱动目标模式执行) | ✔(2 条,见 `test_goal_queue.py`:单 goal EnterGoalMode→met→ExitGoalMode→done 字段级 / 两 goal 严格串行 started_at 先后) | —(工具参数面) | —(无新 Op/EventMsg 变体;复用 EnterGoalMode/ExitGoalMode) | ✅(zhuye-stream goal watcher 4 例:跑到底 / 双 goal 串行 / 无探针拒绝 / 失败结清;zhuye-exec queue_tool add_goal 3 例:roundtrip 字段透传 / 参数校验 / Prompt 权限;zhuye-goal outcome 记忆) | — |
| v1.11 基建#8:deepagents 式记忆(topic 多文件 wiki / consolidate 去重 / 跨会话笔记召回 / 后台自动整理) | ✔(5 条,见 `test_memory_v11.py`:topic 工具全链路含落盘路径 / consolidate 合并统计 / 跨会话召回含原文 / 零相关不注入 / env 节流后台整理) | —(工具参数面) | —(无新 Op/EventMsg 变体;召回经既有 system-reminder 通道) | ✅(zhuye-memory topics 5 例 + consolidate 7 例;zhuye-notes recall 6 例;memory_tool topic/consolidate 4 例;MetaKind 新变体;zhuye-exec headless 节流 env 解析 1 例:缺省 / 0 关闭 / 非法回退 / 上限钳制 / 空白容忍) | — |
| v1.11 M5a:SQLite 记忆后端(`ZHUYE_MEMORY_BACKEND=sqlite`:条目版本库 / FTS5 中文检索 / MEMORY.md 投影 / 墓碑防复活 / notes workspace 归属) | ✔(3 条,见 `test_memory_v11.py`:sqlite 中文检索与投影含跨进程 system 注入 / 删除后投影不复活含 list 存活面 / sqlite 模式跨会话笔记归属召回含 workspace 字段落盘) | —(工具参数面:memory 工具 schema 未变,行为经环境变量切换) | —(无新 Op/EventMsg 变体;MemoryAccess 审计复用既有 record) | ✅(zhuye-memory sqlite 模块 24 例:schema 幂等与 meta / change 端口生命周期·墓碑·CAS·重名 / retrieve 中文·单字·门控·时间窗·墓碑过滤·预算装箱·MATCH 表达式 / project 首次导入幂等·外部编辑·墓碑拦截·投影导出 / store 读写删复活·乐观锁·作用域隔离·trait 路由;bm25 CJK 分词 6 例:纯中文 bigram / 混合不跨类 / 全半角折叠 / 标点分隔 / 中文查询命中 / 旧用例不回归;zhuye-notes 归属 1 例:scoped 召回过滤与旧口径并存) | —(默认关闭,`ZHUYE_MEMORY_BACKEND=sqlite` 显式启用;后端切换不改协议) |
| v1.11 基建#13:GitHub topic 插件发现(`marketplace discover --topic`,默认 zhuye-plugin) | ✔(2 条,见 `test_plugin_discover.py`:参数面字段级 / 网络解析链路条件跑) | ✅(clap 参数面由用例 1 直接驱动) | —(纯 CLI 展示面,无 Op/EventMsg) | ✅(zhuye-plugin discover 4 例:空 topic 拒绝 / URL 构造与 limit 钳制 0→1·超限→100 / 响应全字段解析与空白描述过滤·topics 缺省 / 坏 JSON 与空清单;非 2xx 错误映射为网络路径,由条件网络用例覆盖) | — |
| v1.11 桌面#3:复杂度分级路由(`[routing.main.complexity]` effort 档位→模型映射,model_call 自动选模型) | ✔(2 条,见 `test_complexity_routing.py`:low 档命中映射模型 / 未配置档位回退 primary / 无映射模型不变) | —(config 参数面,由 zhuye-config 单测承担) | —(无新 Op/EventMsg 变体;复用 SetEffort) | ✅(zhuye-config 复杂度解析 2 例:非空档位提取 / 空白丢弃;zhuye-core `effort_model_route` 查表 1 例:命中 / 未配置 / 空表 / Clone 共享 RwLock 热重载可见;ReasoningEffortMirror 增 Hash derive) | — |
| v1.11 F6:组合透明性(`zhuye doctor --dump-config` 生效配置 JSON 快照,密钥脱敏) | ✔(2 条,见 `test_dump_config.py`:空配置结构 + 密钥值不出现在整份 JSON / anthropic 段生效反映 provider·model_spec·凭据 label·has_api_key;runner 门禁下的 env 隔离口径见用例内注释) | ✅(doctor 子命令参数面) | —(CLI 只读诊断面,无 Op/EventMsg) | ✅(zhuye-cli doctor 2 例:凭据脱敏只出布尔·条目无 api_key 字段·段级与池双路径 / 路由与 complexity 档位映射·无 routing 段不 panic) | — |
| v1.11 F5:大输出溢出落盘(spill)与读回路径 | ✔(2 条,见 `test_spill_readback.py`:stdout+stderr 合并超限 → 已捕获内容落盘 + 分流丢弃标记 / 模型回执首尾与 UTF-8 偏移 / 恢复后分页重建落盘文本) | —(工具内部行为) | —(无 Op/EventMsg 变更) | ✅(zhuye-tools bash 1 例:oversized_output_spilled_to_disk 含路径不变量与截断标记) | — |

## 2. 能力覆盖层明细(`tests/functional/`)

### D 组 基础通信与事件流(`test_serve.py`)
- **D1 握手**:`session_configured` 是全局事件(`id=""`),`model=mock/mock-1`、
  `provider=mock`、含 `session_id` / `approval_policy` / `sandbox_policy`。
- **D2 单轮事件序列**:`turn_started` → `agent_message_delta` **×2**(mock 把
  文本拆两段)→ `token_count` → `turn_complete`;`status=="success"`(wire 真值,
  不是类型注释里的 `ok`);`usage` 四字段齐全;`turn_id` 首尾一致。
- **D3 多轮上下文**:两轮拿到 mock 脚本第 1、2 行。
- **D4 token_count**:含 `input_tokens`/`output_tokens`,且至少一条带
  `model` / `provider` 维度(v1.x stats 字段)。
- **D5 终态语义**:turn 类以 `turn_complete` 收尾(其后可能跟一条
  `submission_closed`);非 turn 类只有 `submission_closed`。
- **D9 工具参数增量**(v1.5 additive 变体):模型生成工具参数期间逐段转发
  `tool_call_args_delta`;字段级断言 `call_id` + `delta` 非空、分片按到达
  顺序拼接等于完整参数 JSON、`call_id` 与 `tool_call_begin` 一致、时序在
  `tool_call_begin` 之后。夹具用 mock 的 `tool_call_delta` 脚本行制造
  **分片形态**参数下发(整包形态没有 delta 可转发)。
- **D13 整段消息变体**(`agent_message`,非流式 provider 形态):mock
  脚本行 `{"type":"agent_message",...}` → 恰好一条 `agent_message`
  全文事件、无任何 `agent_message_delta`、先于 token_count 且
  turn_complete 收尾;下一轮 `request_echo` 的 messages 快照含该
  assistant 全文 —— 整段文本与 delta 路径等价累积进模型历史。

### S 组 并发与路由(`test_serve.py`)
- **S1 隔离**:并发提交两条 `user_input` → 两条流的事件 id 各自归属、
  互不串台;各成一个独立 turn(turn_id 不同);两条 mock 回复各用一次。
- **S2 独立收尾**:并发两个 turn 各自恰好一次 `turn_started` /
  `turn_complete`,状态 `success`。
- **S3 收尾不变量**:每条 submission 恰好一条 `submission_closed`。

### T 组 工具与审批(`test_tools.py`)
- **T1** 内置 `echo` → `tool_call_begin` → `tool_call_end`(回显文本、
  `is_error=false`)。
- **T2** 模型调未注册工具 → `tool_call_end(is_error=true, 含 tool not found)`,
  turn 不死。
- **T3** 远程工具往返:`register_tools` → 模型调用 → `tool_execution_request`
  (全局,带 `tool`/`args`)→ 回执 → `tool_call_end`(输出透传)。
- **T4** 远程工具失败回执 → `tool_call_end(is_error=true)`。
- **T5** 工具审批全链路(`ZHUYE_APPROVALS=1` + MCP 工具):`approval_request`
  (request_id / kind.tool_name / risk)→ 回 `tool_approval` → 工具真执行 →
  `turn_complete`。
- **T6** 审批 deny → 工具**未执行**、`tool_call_end(is_error=true, 含 deny 理由)`、
  turn 仍收尾。
- **T7** Bash 显式拒绝规则覆盖换行、后台执行附加命令(错误字段与无副作用)。
- **T8** OS 沙箱拒绝 workspace 外写入时,错误标记与非零退出码正确返回。
- **T9** macOS Seatbelt 按真实 HOME 拦截假密钥读取,不把星号当成 subpath 通配符。
- **T10** stdout/stderr 同时截断后,`tool_call_end` 仍返回失败、原始退出码与捕获字节数。
- **T11** 内置 `memory` 工具 write → read → remove 全链路:`MEMORY.md` 的
  `## <key>` 条目块落盘、读回正文、删除后条目消失;rollout 追加
  `MemoryAccess::Write` 审计记录(agent_type / turn_id 字段级)。
- **T11 整份记忆保护**:旧格式或无法拆分的文档拒绝条目写入和删除,断言工具错误与文件原文不变。
- **T12** `[sandbox].os_level = false` 显式关闭端到端生效(此前只桥接
  `Some(true)`,用户关不掉);env `ZHUYE_SANDBOX_OS_LEVEL=1` 逃生口压掉
  config。目标取 OS 可写但沙箱 allow 列表外的路径(macOS /Users/Shared)。

- **T13** 未设置沙箱开关时,模型命令对工作区外可写路径的写入被拒,断言错误标记、退出码和文件不存在。

- **T14** 不可信命令的沙箱初始化失败时,即使关闭严格开关也拒绝执行,断言错误与无副作用。

- **T15** 资源拒绝规则仅拒绝匹配 URL,不匹配时仍进入审批并透传拒绝理由。

- **T16** 会话级放行后,匹配资源拒绝规则的同一工具仍被拒绝。

- **T17** 无交互审批门时,无关工具正常执行,资源拒绝、强制询问及默认风险硬拒仍生效。

- **T18** 绝对路径拒绝规则覆盖相对路径与符号链接,同时不阻止无关路径。

- **T19** 资源拒绝规则的 glob 无效时匹配工具失败关闭,无关工具不受影响。

- **T24** 文件工具拒绝指向工作区外的符号链接,断言错误事件及外部文件内容不变。

- **T25** 用户级权限文件遵循 `ZHUYE_HOME` 隔离，空值回退 `HOME`，断言实际工具拒绝或放行结果。

- **T26** 用户权限文件损坏时拒绝工具，文件修复后恢复按规则执行。

- **T27** 不同 `ZHUYE_HOME` 的用户记忆分别读写，不使用默认用户目录或另一数据目录的内容。

- **T29** 后台 bash 任务全链路(wire 真二进制):`run_in_background` 立即回执
  `task_id`(metadata 字段级);成功任务在下一回合边界注入模型上下文
  (经 mock `request_echo` 的 messages 快照断言注入文本含任务 id、输出与
  `[exit_code: 0]`),注入后队列排空;失败任务不走边界注入,经
  `background_status` 查询可见(`[<id>] failed` + 原始退出码)。

### T30–T38 组 exec 内置工具全量驱动(`test_tools.py`,原 `tests/rust/tools_drive.sh` 40 断言迁入)
- **T30** 文件系统组:write/read/edit/grep/glob/echo 单 exec 串行驱动
  (edit 回执为 unified diff 结构块,断言落 raw JSON;edit 落盘经二次 read
  与文件内容双重验证);delete_file 独立回合并断言文件消失。
- **T31** read_before_edit hook 四态:新建 write 放行 / 已存在未读 edit
  Deny(含 "never read" 理由)/ 读后 edit 放行且落盘 / 读后外部改动
  (mtime 漂移)edit Deny。
- **T32** bash OS 沙箱:workspace 内写成功;越界写(仓库根目标)被拦,
  拒绝信息回流且文件不落地(不把 is_error 差异当失败,不变量为不落地)。
- **T33** ast + 信息类:list_languages 含 rust / search 命中 alpha /
  get_context_remaining 报 token / tool_search 命中 grep / brief 渲染正文。
- **T34** image_view 返回 image/png 块;web_fetch 回环地址优雅报错
  (blocked range);web_search 缺 BRAVE_API_KEY 优雅报错。
- **T35** notebook_edit:list 放行返回单元格;insert 未读被 hook Deny。
- **T36** git worktree:EnterWorktree → ExitWorktree 全链路,
  `.git/worktrees/zhuye-wt-*` 真实建立。
- **T37** checkpoint list 正常;rewind 无参优雅报错且提示 sha。
- **T38** ask_user 非法参数 / request_human_input 无审批门均优雅报错,
  headless 下 turn 正常收尾。
- **T39** prompt injection guard 运行时接线(`[prompt_guard] enabled`,
  env 可压开):显式关闭对照(`enabled = false` 注入短语原样通过);开启后
  Block 裁决把 echo 输出整体替换为 guard 错误(`metadata.prompt_guard.
  verdict="block"` + 规则码 + 指纹,原文不回流);Review 裁决正文保留、
  metadata 标注 `verdict="review"` 与规则码;Allow 输出与 metadata 完全
  不变。**F01 起默认开启的行为由 F01 组承担**(默认翻转后本条对照段
  改为显式关闭)。

### F01 组 prompt_guard 默认开启与工具定义扫描(`test_prompt_guard_default.py`)
- **F01a** 默认(无 `[prompt_guard]` 配置)注入输出被 Block 拦截
  (`verdict="block"` + `PI_OVERRIDE_DIRECT` + 64 位 hex 指纹,原文不回流)。
- **F01b** 默认开启下 Review 标注(正文保留 + `PI_ROLE_TAKEOVER`)与
  Allow 完全不动(metadata 无 `prompt_guard` 键)。
- **F01c** 注入语料回归:block 族 6 样本(override / exfil system prompt /
  secrecy×2 / credential exfil)逐条命中预期规则码;review 族 3 样本
  (角色接管×2 + 零宽字符)逐条标注不拦截。
- **F01d** 边界:`ZHUYE_PROMPT_GUARD=0` 压关放行;`[prompt_guard]
  enabled = false` 显式关放行;`config=false + env=1` 时 env 胜仍拦截
  (优先级序 env > config > 默认开启)。
- **F01e** 失败路径:同回合 Block 后续工具照常执行(guard 不毒化队列)、
  `turn_complete` 正常到达。
- **F01f** 权限交互:`ZHUYE_APPROVALS=1` 下 MCP 工具审批照走,批准后
  输出仍被 guard 拦截(权限与 guard 两层独立)。
- **F01g** 工具定义扫描:恶意 description 的 MCP 工具(自建 evil stdio
  server)默认拒绝注册(调用报 `tool not found`,server 正常启动
  `tool_count=1` 排除未起好假阳性);`ZHUYE_PROMPT_GUARD=0` 时可注册
  可调用。
- **F01h** `Op::QueryGuardStats`:不开 turn 应答 `guard_stats` 事件
  (enabled=true + 拦截后 `outputs_blocked=1`)+ `submission_closed`
  收尾;guard 关闭时 `enabled=false` 且计数全零。

### SK 组 技能面(`test_skills.py`)
- **SK1** 默认配置(不写 `[active] bundled_skills`)→ 内置 skill 不在
  catalog:`load_skill("code-review")` → `tool_call_end(is_error=true, 含
  skill not found: code-review)`,turn 不死。
- **SK2** `[active] bundled_skills = true` → 同一调用成功,`tool_call_end`
  输出含正文标记(`thorough code reviewer`)。
- **SK3** 默认配置下 workspace 文件 skill(`.zhuye/skills/file-skill/`)
  依旧可加载(正文标记命中)—— 开关只作用于编译期内置包。
- **SK4** 用户技能随 `ZHUYE_HOME` 隔离，空值回退默认用户目录。

### A 组 会话状态 Op(`test_serve.py`)
`user_input`+`thread_settings`(四字段) / `set_effort`(静默) / 权限模式切换
(全局 `permission_mode_changed` + 持久化轨迹) / `rewind`(`turn_rewound`) /
`interrupt`(`turn_aborted`,reason 是带 `type` 的对象) / `compact`
(`context_compacted`) / goal 模式(进出静默) / plan 模式(`plan_request` →
`plan_ready` → 切权限模式)/ 四种孤儿回执(不崩、收尾、会话存活) /
`steer`(静默,消息合并进下一轮 user message)/ `query_subagents`
(`subagent_status`)/ `prompt_command`(`prompt_list` / `prompt_switched`);
A15–A24 覆盖 side-channel 终态、权限、审批、工作目录及配置继承;A25–A27 覆盖审计轮转、关闭配置及跨进程写入。

原 `tests/rust/serve_ops.sh` 迁入补齐的两个缺口:
- **A28** `shutdown` op → `shutdown_complete` 优雅退出:非 turn 静默语义
  (无 turn 事件、无 error),此前仅由 fixture close() 隐式覆盖。
- **A29** `plan_approval revise` → `plan_rejected` 指向原 plan_id,留在
  plan 模式(不切 accept_edits);二次 `exit_plan_mode` 生成独立 plan_id;
  会话存活。实测时序:rejection 在 approval 的 `submission_closed` 之后
  到达(collect 首个终态即返回),用例显式轮询并记录该行为。
- **A30** 内联 `question_answer`(user_input item):与 dedicated Op 同
  语义解除挂起的 `ask_user_question` gate,工具回执含 `"selected":[0]`,
  原 turn 完成、回答方 submission 自身也正常收尾、无 error。
- **A31** 内联 `local_image`:按路径读盘转内联 Image 块,与文本合并进
  同一 user 消息(经 `request_echo` messages 快照断言占位与文本共存);
  接线同时覆盖 `skill` 注入与 not-found 说明(core 单测字段级)。

### R 组 重连与恢复(`test_recovery.py`)
- **R1 `--resume <thread id>`**:恢复会话并**续写同一份** rollout(历史仍在)。
- **R2 `-c`** / **R3 `-r 1`**:续最近一次会话。
- **R4 进程崩溃**:挂起的 submission **收不到终态事件** —— 客户端靠 stdout
  EOF 感知断开(wire 上没有"会话结束"事件,SDK 层负责兜底)。
- **R6 resume 侧通道**:恢复后的会话仍能启动侧通道并收到完整完成事件。
- **R5 stdin EOF**:进程退出码 0(EOF 等价于 shutdown)。

### X 组 容错(`test_recovery.py`)
坏行 → `invalid_submission` 且会话存活 / 空工具名 → `tool_name_invalid` /
撞内置名 → `tool_name_conflict` / 远程工具超时 → 错误输出含 `timed out`
且 turn 收尾 / 未知 op → `invalid_submission` 且会话存活 / 错误事件是全局
(`id=""`),不进 turn 流。

### P 组 持久化(`test_persistence.py`)
`session_meta` 首行 + `message` 记录集 / 权限模式 `from→to+at` 轨迹 /
rewind 物理截断(记录减少 + `.rewind-<ts>.bak` 备份)/ `zhuye session ls`
能列出该会话。P5 覆盖旧日期会话通过 thread id、内部 session id、`-c`、`-r 1` 恢复后续写原文件。
P6 覆盖标题精化:block 数组 content(`[{"type":"text",...}]`)提取 text 块派生
标题(不再泄漏 JSON 键名),`session rename` 自定义名经 `resolve_display_title`
优先于派生标题(`session show` 的 `Title:` 行字段级断言)。P6b 验证改名不会跟随侧写符号链接并修改外部文件。P7 覆盖 `session
export` 完整转写稿:13 变体渲染 + 文末 Usage 段(per-model 表 / token 总量),
不经 4000 字符截断;`--out` 原子写落盘。P7b 验证目标为命名管道时及时报错，且原管道不变。
P9 覆盖损坏的 cron 存储在创建失败后仍保留原内容;P10 覆盖禁用任务跨进程
恢复后重新启用时重新计算并持久化下一次触发时间;P11 覆盖写盘失败时创建
返回错误并回滚内存状态。
P12 覆盖 cron 超大步长和非法范围,断言工具结果及登记任务数。
P13 验证 `ZHUYE_HOME` 隔离会话文件、列表和恢复续写。
P14 使用 1 秒 tick 验证到期 cron 注入真实 submission,产生独立 turn 并更新 last_fired / next_fire。
P15 覆盖 Autopilot 真实调度:`~/.zhuye/autopilot/config.json` opt-in
(mtime 回拨消除首跑竞态 + `ZHUYE_AUTOPILOT_TICK_SECS=1` 加速)→ 到期
注入真实新回合并完成(mock 回复标记进 wire);`history.json` 落盘恰一条
运行记录且终态回填 `success`(camelCase 字段级:id / executedAtMs / status)。

### O 组 编排(`test_orchestration.py`)
子代理 `call_explorer`(子代理执行 → 回执进历史)/ 讨论编排(transcript +
Result + rounds_completed)/ 任务与团队(create → ls)/ 流水线错误路径
(no runner / team not found)/ goal 模式与其他 op 共存。
O6 验证任务更新后的字段与元数据;O7 验证跨进程并发创建的 ID 和文件;
O8 拒绝绝对路径及多级列表 ID(任务管理器单测同时覆盖内存后端);O9 验证损坏、落后或丢失的高水位不会覆盖旧任务;O10 验证超大任务输出的读取上限和截断字段。

O11–O16 组集成链路(原 `tests/rust/integrations.sh` 44 断言迁入):
- **O11** MCP 生命周期与 in-turn 调用:`mcp_server_started`(fsx,
  tool_names 含 `mcp__fsx__echo`、tool_count==2)/ 坏 server →
  `mcp_server_failed` 含错误;`mcp_tool_invoked`(全局事件,server/tool
  字段)+ `tool_call_end` 回传 echo 输出;turn 完成。
- **O12** workspace skill(exec 路径):`load_skill` 回执含正文 marker
  与激活工具 `echo`;不存在的 skill 优雅报错(skill not found)。
- **O13** session notes:`add_session_note` 成功;`~/.zhuye/session-notes/`
  JSONL 落盘、内容含 marker、每行含 created_at。
- **O14** bubble 权限模式:Prompt 权限 MCP 工具触发 `permission_bubble`
  (tool_name / risk 字段)后自动放行执行并回传输出;turn 完成。
- **O15** 配置热重载:追加 `[compact]` 段 → `config_reloaded`(path 指向
  config.toml、sections_changed 含 compact);重载后会话存活。
- **O16** plugin marketplace:add → install → enable → exec 启动期
  `plugin_loaded`(plugin=hello@integ-mk、version=0.1.0);turn 完成。

### OTLP 真实上报(`test_telemetry_otlp.py`)
- **O1** `[analytics]` opt-in + 进程内 OTLP/HTTP collector(线程化 HTTP
  server):serve 启动 exporter 后,真实 turn 的 span 被批量上报 ——
  POST `/v1/traces`、`application/x-protobuf`、payload 原始字节含
  `service.name=zhuye-agent` 资源与 `agent.turn` span 名。附注:
  `RUST_LOG` 全局过滤同样作用于 OTLP 层(用例显式 info)。

### V 组 v1.10 功能面(`test_v110.py`)

<!-- WASM 插件能力行:V12 -->
- **V1 思考强度配置**:`config set model.reasoning_effort ultra/Max` 落盘
  字段级断言、非法档位拒绝(文案含合法值清单)、`config ls` 可见、
  `unset` 清空;
- **V2 set_effort 新档位**:`set_effort max` / `ultra` Op 提交无 error
  事件、后续 turn 正常收尾(SDK / serve 面);
- **V3 上下文窗口**:`config set model.context_window 307200` → exec 的
  `session_configured.context_window_size == 307200`(字段级);
- **V4 覆盖表归一化**:`[context_windows] "Mock" = 204800` 对模型
  `mock/mock-1` 前缀命中,且 per-model 条目优先于 `[model].context_window`;
- **V5 空 key**:anthropic 模型组 `api_key=""` 无 base_url → 未配置(对照);
  加显式 `base_url` → doctor 识别活跃组为 anthropic;
- **V6 插件门控对照**:未启用 goal-queue → `queue` 工具 tool not found;
- **V7 队列全链路**:plugin install + enable → mock 调 `queue add/start`
  成功 → 任务落 `~/.zhuye/queue/<thread_id>.json`(id / prompt / name
  字段级)→ 出队注入的 turn 以 TurnComplete 收尾后状态 `done`
  (started_at / finished_at 记录)。结清信号是 per-turn 事件监视
  (`QueueScheduler::with_turn_submitter` + `TurnHandle::into_receiver`,
  按任务 id 归因,TurnComplete / TurnAborted / 通道关闭三类终态都结清)
  —— 曾用 Stop hook 作唯一信号,绕过 check_stop 的结束路径(凭证耗尽 /
  预算耗尽 / Interrupt)会让 active 永久卡 Running;该路径的单测在
  `zhuye-stream::queue::tests`(submitter_tick_settles_on_turn_terminal_events
  / settle_task_is_id_attributed / submitter_closed_channel_marks_failed);
- **V8 goal 工具**:`goal status` 报未激活;`goal enter` 提交 Op(输出
  回显目标)、无 `GOAL_NO_CLIENT` 错误;
### 缺陷修复回归(`test_bugfixes.py`)
- **预算耗尽显性化(P-13)**:迭代预算 / token 预算耗尽且**零文本**时,
  turn 在 `turn_complete` 之前显式发 `error` 事件,字段级断言
  `code == "iteration_budget_exhausted"` / `"token_budget_exhausted"`、
  `details.iterations`、error 先于 turn_complete、turn status 仍为
  `max_iterations` / `token_budget_exceeded`;反向守护:预算耗尽但强制
  收口产出了文本(合法完成)**不得**误报 error。
- **配置隔离(P-06)**:`ZHUYE_HOME` 目录下的 config.toml 被 `config show`
  读到(`trigger_tokens=7777` 命中、`$HOME` 那份的 1111 不出现)——
  配置与 session-notes 同一套 `ZHUYE_HOME` 语义。
- **工具裁切(P-09)**:`[active] tools` 白名单 → 引擎发给模型的工具集恰为
  白名单(mock `request_echo` 观测,字段级);`[active] disallowed_tools`
  黑名单 → bash 消失、read 与客户端远程工具保留;无配置 → 内置工具
  默认全量暴露(向后兼容)。
- **提示词关闭(P-08)**:`.zhuye/prompts/default.md` 覆盖 `## system.plan-mode`
  为 `off` → 引擎 system prompt 不含 `## Plan Mode` / `ExitPlanMode`,
  自定义 `## system.main` 全文生效;未覆盖时 Plan Mode 段保持既有行为。
- **mock 配置错误显性化(P-02)**:mock 脚本全部行解析失败 / 路径不存在 → 首次模型调用
  发 `error` 事件 `code == "INVALID_REQUEST"`,message 含失败原因,
  不再静默回退固定回复。
- **mock 回复可辨识(P-01/P-05)**:无脚本回退文案自带「mock 固定回复」可辨识标记。
- **model 前缀剥离(P-10)**:`ZHUYE_MODEL=openai/m1` 前缀识别与剥离(config 层
  单测 `strip_known_provider_prefix` + CLI `config show` 有效模型联动)。
- provider 层的 **P-11**(非 SSE 响应显式报错)/ **P-16**(base_url 版本段
  拼接)/ **P-12**(temperature / max_tokens 段配置)落在
  `zhuye-llm` 的 wiremock 回归(openai_non_sse_* / join_endpoint_* /
  openai_config_temperature_*),不在离线 serve 套件(无真实网关)。

### V 组补:v1.x WASM 插件(多语言生态,`test_v110.py`)

- V12:Rust 组件(demo_echo 工具 + wasm hook)经 demo 插件端到端 ——
  安装/启用 → 进程内实例化 → mock 调 `demo_echo` 的 `tool_call_end`
  回显输入文本且 `is_error=false`(覆盖注册 → 模型可见 → invoke →
  输出全链路);serve stderr 出现 `wasm hook registered` /
  `wasm tool registered`(hooks.json 的 `wasm` 字段曾在解析层被丢弃、
  `WasmHook::events()` 曾返回空切片,wasm hook 从未注册 / 派发过 ——
  两处断言即防回归)。样例产物随插件入库(重建用
  `scripts/build_plugin_demo_wasm.sh`)。宿主-组件契约闭环(allow /
  inject / matcher 不命中 Allow / 超时 Deny / describe / invoke)与
  决策协议双键兼容(`action` / `decision`、`block` / `message` 别名)、
  工具输出缺省字段(`metadata` / `elapsed_ms` 可省)由 `zhuye-wasm`
  单测覆盖;hooks.json `wasm` 字段存活与 `${PLUGIN_ROOT}` 展开由
  `zhuye-plugin::capabilities::hooks` 单测覆盖。

### C 组 CLI 能力面(`test_cli.py`)
`serve` 三个恢复入口 / `exec` 输出每行是合法 JSON 且含 turn 事件 /
`session ls` + `show` 报错路径 / `doctor` + `version` 输出契约 /
`session search` 跨会话全文搜索(命中 / 未命中 / `--json` 字段)/
`exec` 退出码映射(成功 0 / turn 失败 1 / plan mode 未产出计划 122)。
C7b 覆盖 hook 启停对损坏配置的保护、未知字段与注释保留,以及并发更新；
C9 覆盖插件安装、列表与运行时按 `ZHUYE_HOME` 隔离。

### C10–C19 组 CLI 深度参数面(`test_cli.py`,原 `tests/rust/cli_deep.sh` 194 断言迁入 + secrets 接线)
- **C10** login:4 provider 路径 / 缺 key 报错(含 api_key 提示)/ 短 key
  告警与 `--force` 静默 / `config show` 脱敏与 `--reveal`。
- **C11** config:set 回显 / ls 新值与 `<unset>` / 非法 key 列出允许清单 /
  unset 复位 / show 打印 ZhuyeConfig 头。
- **C12** mcp:stdio+http add / `--command`×`--url` 互斥(rc=2)/ ls 标注
  传输 / show(stdio/http)/ `test` 真 mock server 列出双工具、坏 binary
  报 failed to start / remove / registry ls 三条目 + 有效 id + unknown 报错。
- **C13** session:双 ID 前缀解析回归(thread / internal 8 与 12 位前缀 /
  无匹配报错)/ 受控 fixture 前缀歧义(matches 2)/ 文件名 id ≠ 内部 id
  双向定位 / ls 过滤与 limit / rename / export(stdout 与 --out)/ fork /
  rm 拒绝保留 + `--yes` 删除。
- **C14** traces:model-io 落盘 / show 前缀(Calls / model / usage /
  request)/ ls 含 mock / 不存在前缀报 no trace session matches。
- **C15** task/team:create 回显 Task #1 / show 缺 `--list` rc=2 / update
  状态与标题 / 非数字 id clap 报错 / team create→ls→show→sync→delete
  (拒绝保留 + `--yes`)/ stop 物理删除 + purge。
- **C16** plugin:marketplace add/ls/缺 manifest 报错 / install / info
  (user scope + version)/ show manifest / enable 写入 config、disable
  移除 / uninstall + not installed / 本地目录 inline 安装 / refresh +
  marketplace remove。
- **C17** workspace(本地 clone / `file://` + `--depth 1` shallow 标记 /
  坏 URL 报 git clone)、update(版本 + cargo install 提示 + check-only)、
  doctor(常规段 + network probe skipped + `--check-network`)、lsp(空配置
  提示 / 未配置报错 / 配置后 list+status)、security audit 报告头。
- **C18** pipeline 错误路径(team not found / 自定义 label no runner)与
  discussion mock 驱动(Transcript / Result / rounds_completed /
  transcript_len / collab_message / `--output` 文件含完整报告)/
  discussion ls 空列表与 `--discussion-id`。
- **C19** `secrets` 子命令(加密密钥存储的 CLI 接线):set 只读 stdin、
  明文不落盘;get 往返一致;list 只出条目名;set 空输入报错;delete
  拒绝保留 + `--yes` 删除 + 缺失报错;损坏 store fail-closed(corrupt /
  authentication failed);machine key 权限过宽打开即失败(unix)。

## 3. CLI 子命令(18 个,不含 help)

| 子命令 | 冒烟(cli_core/cli_modules) | 深度断言位置 |
|---|---|---|
| `exec` | ✅ | 能力层 C2/C6(exec JSONL 字段级与退出码映射)、T30–T38(全工具驱动) |
| `serve` | ✅ | **能力层 A 组 22 Op 全量**(原 serve_ops.sh 已迁入)+ O11/O14/O15(MCP/bubble/热重载);`--plugin` 显式加载见能力层 I7 |
| `login` | ✅ | 能力层 C10:4 provider 路径、缺 key 报错、短 key 告警、`--force`、api_key 脱敏 |
| `mcp` | ✅ | 能力层 C12:stdio+http 双传输 add/互斥报错/ls/show/remove/`test`(真 mock server 成功+坏 binary 失败)/registry 三路径 |
| `config` | ✅ | 能力层 C11:set/unset/ls/show/`--reveal`/非法 key 报错 |
| `session` | ✅ | 能力层 C13:双 ID 前缀解析回归(文件名前缀+内部 id 前缀+歧义报错)/rename/export/fork/rm/search;能力层 P4/C3/C5 |
| `traces` | ✅ | 能力层 C14:ls/show 前缀/不存在报错 |
| `doctor` | ✅ | 能力层 C4/C17:常规 + `--check-network`(mock provider DNS 失败仍 rc=0) |
| `plugin` | ✅ | 能力层 C16:marketplace add/ls/refresh/remove + 安装 + info/show/enable/disable/uninstall 全链路 |
| `lsp` | ✅ | 能力层 C17:空配置提示/未配置 status 报错/配置后 list+status |
| `task` | ✅ | 能力层 C15:create/ls/show/update/stop/purge + team 全链路 + 非数字 id clap 报错;能力层 O3 |
| `pipeline` | ✅ | 能力层 C18 错误路径(team 不存在/自定义 label 无 runner —— 产品限制,见 §5);能力层 O4 |
| `discussion` | ✅ | 能力层 C18:mock 驱动真多 agent(sequential,transcript 非空)+ `--output`;能力层 O2 |
| `security` | ✅ | cli_modules(cargo audit 封装,断言输出结构) |
| `workspace` | ✅ | 能力层 C17:ls/clone 本地仓库/`--depth` 浅克隆/坏 URL 报错 |
| `update` | ✅ | 能力层 C17:常规 + `--check-only` |
| `secrets` | — | 能力层 C19:set(stdin)/ get 往返 / list 不泄露值 / delete 确认与 `--yes` / 损坏 store fail-closed / machine key 权限 fail-closed(unix) |
| `version` | ✅ | cli_core(断言版本号格式);能力层 C4 |

## 4. 集成面

集成链路 44 断言已迁入能力层 `test_orchestration.py` O11–O16(原 `integrations.sh`):
- **MCP**:server 生命周期(`mcp_server_started` 含 tool_names/tool_count、
  坏 server → `mcp_server_failed`)+ in-turn 调用(`mcp_tool_invoked` +
  `tool_call_end` 回传 echo 输出);能力层 T5/T6 另覆盖审批链路。
- **skill**:工作区 `SKILL.md` 扫描 + `load_skill` 激活(正文 + 激活工具
  列表);不存在的 skill 优雅报错。内置 skill 包默认关闭
  (`[active] bundled_skills`,能力层 test_skills.py:SK1 默认拒载 / SK2 开启后
  正文可激活 / SK3 文件 skill 不受开关影响)。
- **notes**:`add_session_note` → `~/.zhuye/session-notes/<thread>.jsonl`
  落盘(内容 + JSON 合法性)。
- **bubble 审批**:Bubble 模式下 Prompt 权限工具 → `permission_bubble`
  非阻塞通知 + 自动放行执行。
- **配置热重载**:`config_reloaded`(path + sections_changed)+ 重载后会话存活。
- **plugin**:marketplace add/install/enable → 启动期 `plugin_loaded`
  (plugin 名/version/skill_count)。

### SDK 绑定层(K 组,`tests/sdk-python/test_sdk_real_bin.py` + `tests/sdk-ts/real-bin.test.mjs`)

- K14:rescan_extensions additive Op(真二进制,双 SDK 字段级)——
  `Op::RescanExtensions` → `extensions_rescanned`(skills 计数非负、
  commands=false、submission_closed 收尾);逻辑层同型用例
  (python `test_protocol.py` / TS `protocol.test.ts`)+ V11(serve 直发)
  三层互补。

真二进制 + mock provider,断言 **SDK 客户端自身的行为**(服务端语义归
能力覆盖层;历史证明只有这一层能发现协议漂移 —— mock 是自己写的假
服务端,不随真协议演进):

- **K1** `spawn` 完成 `session_configured` 握手才返回;
- **K2** 单轮 prompt 文本回流(K2b 多轮上下文保持);
- **K3** 远程工具:注册 → LLM 调用 → 客户端 handler 执行 → 回执 →
  `tool_call_end`;
- **K4** 非 turn op(set_effort)迭代器以 `submission_closed` 收尾
  (v1.3 前会永久挂起);
- **K5** interrupt 打断在飞 turn(`turn_aborted`);
- **K6** close 幂等 + 子进程优雅退出;
- **K7/K7b** 坏流(非 JSONL)握手超时抛错 / 二进制不存在转可等待
  rejection;
- **K8** handler 抛异常 → `is_error=true` 错误回执;
- **K9** 全局事件读取口(`next_global_event` / `nextGlobalEvent`);
  py 另有 K9b 插件选项(`--plugin` 显式加载 → `plugin_loaded`);
- **K10(side-channel,WP9)** `submit_op/submit({"type":
  "query_side_channels"})` → 真实 serve 回 `side_channel_status`
  (channels 快照数组)+ `submission_closed` 收尾 —— exec 已装配
  driver(WP13),查询有真实应答。两侧 mock 层(sdks/*/tests)另有
  同型用例与 `side_channel_started` 透传断言。

  注:TS 侧 K10 编号历史上被 plugins 用例占用,side-channel 用例在
  ts 文件内同名标注 K10;对照表见 §7 用例规模汇总。

## 5. 已知产品限制(测试断言当前行为,防退化)

- ~~`pipeline run`:plan 预设模板硬编码 `{{input.audience}}`~~(v1.6 已修复:
  planner 预设只依赖 `{{topic}}`,CLI `--input k=v` 注入模板 inputs,
  build_factory 从配置引导 registry 使 mock 可离线命中);
  `pipeline team` 失败节点存在时退出非 0(v1.6 对齐 CI 语义)。
- `discussion run`:concurrent 模式受 SubAgentFactory 深度上限约束;
  无 provider 时降级 `run_noop`(退出 0,Result JSON 仍含 outcome)。
- `approval_request` / `ask_user*` 的**交互式工具路径**需要 TUI/modal;
  headless 下只验孤儿回执的 wire 级健壮性。serve + `ZHUYE_APPROVALS=1`
  可触发真实 gate(能力层 T5/T6 已覆盖)。
- `web_search` / `web_fetch` 离线时优雅报错(无 provider key / 无网络)。
- **并发 turn 是真并发**:core 对每个 `UserInput` 提交 `tokio::spawn` 一个
  独立 task(`submission_loop.rs:621`),两个 turn 的模型调用先后不保证 ——
  已与 `sdks/PROTOCOL.md` §4 的并发语义对齐(见 §6 D5)。
- **三个 id 互不相同**:`session_configured.session_id`、rollout 内
  `session_meta.session_id`、会话**文件名**里的 thread id 是三个不同的
  uuid(exec/serve 路径未注入 `with_session_id`)。`--resume` 支持**文件名**
  thread id 与 SessionMeta 内部 id;`zhuye session ls` 显示后者(P5 覆盖双 ID)。
- **旧版双 SDK `ThreadSettings` 字段**:为类型兼容保留的 `effort` /
  `permission_mode` 不被服务端读取;文档提示分别使用独立 Op。双 SDK
  已补齐服务端支持的 `approval_policy` / `sandbox_policy` /
  `max_tool_concurrency` 类型,能力层 A1b 验证回合级拒绝确实生效。

## 6. 已知缺陷与修复状态

### 待修复

当前没有记录在此处的待修复 SDK 缺陷。

### 已修复或经复核排除

- `background_status` 工具注册但模型不可见:builtin 注册清单与
  skills catalog 的 always_on 白名单都没有收录它,真二进制里 bash
  `run_in_background` 回执承诺的「可用 background_status 查询」会落空
  (tool not found);引擎测试手工注册工具绕过了 curated 过滤,由 T29
  wire 用例实测暴露。已同时补入注册清单与 always_on,mock
  `request_echo` 快照同步增加 messages 字段供上下文注入断言。
- PyO3 绑定:wheel 可构建并安装,版本来自 workspace;agent 持有后台运行时,
  正常终态用量不再重复累加。原生 Python 测试验证多轮、工具回合、
  实际 HTTP 等待期间的 GIL 释放和精确用量。
- TS `prompt` 超时:从调用起计时,等待握手或写入及消费事件均受限;
  超时清理本地监听且不自动中断服务端回合,避免影响并发请求。
- 压缩摘要恢复:回合从 rollout 重建历史时恢复上一版摘要状态,后续
  LLM 压缩使用增量摘要;场景测试检查重启后的摘要请求与模型请求。
- 子代理取消:父回合中断会取消在途子代理;启动期间取消也释放并发槽位,
  未配置状态中心时丢弃子代理仍会取消其令牌。
- 模型请求取消:响应头、重试退避和尾部用量等待均响应取消;流式中断
  仅发送一次终态,已输出文本在终态前落盘,下一轮可恢复。
- Responses 流按 output index / item id 归属工具参数,调用与结果分别
  编码为独立输入项;输出截断、流内错误及不完整工具调用不再伪装成
  成功。凭证冷却和配额按路由池名归集。
- Anthropic HTTP 200 流内错误及缺少 message_stop 的断流会进入错误和
  重试路径,不再被当作空的成功回复。
- 模型流在可见输出后失败时(v1.10)自动重试:重试前发 `agent_message_reset`
  让客户端清空半成品缓冲,重试从头生成,两次输出不拼接;不可重试错误
  (4xx / 取消 / 上下文超限)仍保留已输出文本落盘。未完成流不能执行工具。
- 失败回合(v1.10)必定以 `error` + `turn_aborted(AbortReason::Error)` 收场
  (含重试预算耗尽);此前只发 `error`,客户端 turn 状态永远停在 streaming,
  停止按钮点击后因 turn_id 配对不到而静默失效。
- 单凭证 / 单模型配置下(v1.10)同凭证重试不再被 per-cred 上限提前打死,
  自动重试可烧满 `[routing] max_attempts`(默认 10)。
- 无效工具 JSON 不再被替换为 `null` 后执行;该调用生成错误回执给
  模型,同批有效工具不受影响。
- 父会话与子代理共享审批 waiter、会话许可和提问 waiter;子代理需要审批
  或人工输入的事件转发到父客户端,回执能唤醒实际等待的 gate。
- 子代理继承父会话权限 resolver、permission mode、YOLO 分类与沙箱配置;
  显式拒绝不能再被子代理的默认 Auto 模式绕过。
- 子代理仅在收到 `turn_complete` 且提取到结果时标记成功;中途错误不会把部分
  回复当作成功,失败与取消状态分别保留。父上下文按请求传递并修复
  截断边界的工具配对,选中的回执与后续来源说明一起传递。伪造来源说明不扩大
  历史共享范围,同批其他工具仍受自身历史范围限制。工具调用前的
  过程文本不会被拼入子代理最终结果。
- 多工具调用和中断恢复:真实 serve 经本机 SSE 模型网关并发调用远程
  工具,逆序回执后验证 rollout 的调用、结果以及重启后的模型请求;
  回合中断会取消远程等待并清理旧回执,独立回合不受影响,终态前会
  持久化已产生的调用与结果。对应 `functional/protocol/test_scenarios.py`。
- SDK 并发与断线:TS 在写入前注册 submission 监听,并在 EOF 唤醒所有
  全局等待者;Python submission 迭代器在 EOF 后重复读取持续返回结束。
  受控流测试覆盖快速响应和多个等待者,双 SDK 的 K15 使用真二进制
  检查并发提交的事件隔离;双 SDK 的 `prompt()` 在收到终态前断线时
  抛错,不会将部分回复作为成功答案返回。K16 以真二进制验证进程
  在远程工具执行时崩溃,且 TS 延迟工具回执不会因 EPIPE 使宿主崩溃;
  TS 关闭或 stdout 断开后提交直接报错,不返回永久挂起的迭代器。

- D1:Python `close()` 在写锁内发送 shutdown,禁止后续写入;mock 区分 shutdown 与仅 EOF 退出,验证关闭协议与幂等性。
- D2/D3:双 SDK 的 turn 状态、结构化中止原因、plan 审批选择与 Python 审批 wire 类型已与 Rust 对齐。
- D4:Python 新增 `next_global_event(timeout_secs)` 公开读取口;K9 验证绑定层全局事件路由,逻辑测试覆盖超时与 EOF 重复读取。
- D5:协议文档改为并发 turn 与按 submission id 路由。
- D6:此前已修正测试断言并纳入 CI;本轮审批用例改为等待 MCP 就绪事件,权限轨迹按记录的 `at` 时间戳还原,避免误把异步落盘顺序当成状态变化顺序;examples 冒烟逐一检查全部六个产物,缺失任一个时重新构建,构建失败计入失败;TS mock 测试为每个 SDK 实例提供独占流,未知事件断言检查实际透传字段,避免争抢握手造成未处理拒绝;配额持久化用例迁入独立集成测试进程,隔离全局路径变量与其他 tracker 的并发写入。
- D7:流式输出分支保留内部状态行用于退出码解析,只从增量事件中过滤它;T8/T9 已验证 OS 沙箱拒绝后的 `is_error=true` 与非零退出码;T10 验证双流截断、状态行丢失后仍回退到原命令退出码。
- D8:当前 `classify_action` 对 `InvalidRequest` 返回 `GiveUp`,不会进入构造冷却原因的分支;旧记录所述一小时冷却不成立。
- D10:双 SDK 新增 `ToolCallOutputDelta` 类型,逻辑测试覆盖输出增量字段透传及未知变体兼容。
- D11:TS SDK 新增 `nextGlobalEvent` 公开读取口(此前 `id=""` 全局事件被静默丢弃),并补齐 K5/K7/K7b/K8/K9 真二进制用例与 Python 侧对齐;TS `prompt()` 遇 `turn_aborted` 改为抛错(不再静默返回部分文本);Python 补 `SubmissionClosed` 类型声明;TS stdout EOF 现在会唤醒挂起的迭代器(对齐 Python `_on_eof`);发布名落定为 `zhuye`,`__version__` 纳入四处版本联动。
- D12:SDK 随包分发平台二进制(参考 npm 平台子包 / ruff 模式)—— npm 主包按平台安装 `zhuye-<slug>` 子包(optional dependencies,清单由 CI 动态生成),Python 切 maturin 出平台 wheel(二进制以 `zhuye/bin/` 包数据单份携带);双 SDK 二进制解析链统一为 `options.bin → ZHUYE_BIN → 内嵌二进制 → PATH`,平台不匹配回退 PATH;安装冒烟与发布后 registry 验证升级为断言内嵌二进制 `--version` 可执行。
- Python 3.10 兼容性:移除 Python 3.11 才提供的 `typing.NotRequired` 依赖,用 `TypedDict` 继承区分必填与可选字段,保持零运行时依赖。

### 缺陷修复记录(2026-10,实测问题清单全 17 项)

- **P-01/P-05**:mock 默认回退文案自带「mock 固定回复」标记,离线测试
  不再假绿。
- **P-02**:mock 脚本路径不存在 / 全部行解析失败 → 首次模型调用显式
  `INVALID_REQUEST`(engine → `EventMsg::Error` → SDK 异常),不再静默
  降级。
- **P-03**:serve 的 tracing 日志在 stderr 非 tty 时不再输出 ANSI 色码;
  双 SDK 支持 stderr 接管(Python `on_stderr` 回调 / `forward_stderr=False`,
  TS `onStderr` 回调),不再硬编码直通宿主 stderr。
- **P-06**:`ZHUYE_HOME/config.toml` 成为配置读取的第一优先路径
  (`config show/ls/set`、serve、doctor、login、mcp、plugin 全部经
  `default_config_path`),配置与数据目录同一套 `ZHUYE_HOME` 语义。
- **P-08**:`## system.plan-mode` / `## runtime.active-mode` 覆盖为
  `off`(或 none/disabled/false)即整段关闭 —— 非编码场景不再每请求背
  ~2.3KB 编码代理人格(docs/prompts.md 已记载)。
- **P-09**:新增 `[active] tools` / `[active] disallowed_tools` 主 agent
  工具裁切(与 `.zhuye/agents/*.md` 同语义:白名单 ∩ / 黑名单 ∪),
  serve / SDK 接入方无需写 agent 文件即可摘掉内置工具。
- **P-10**:`ZHUYE_MODEL` 带已知 provider 前缀(mock/openai/anthropic/
  ollama 及别名)时剥前缀;未知前缀(OpenRouter 式 vendor/model)保留。
- **P-11**:流式请求收到非 SSE 响应(HTTP 200 普通 JSON)→ provider 显式
  返回 `LlmError::Provider`(带 content-type 与响应体预览)→
  `EventMsg::Error`,不再伪装成「模型没说话」(wiremock 回归)。
- **P-12**:模型组新增 `temperature` / `max_tokens` 配置项
  (endpoint = openai / anthropic / ollama;Responses 映射为
  `max_output_tokens`,Ollama 映射为 `options.num_predict`),请求侧
  显式值优先(wiremock 回归)。
- **P-13**:迭代预算 / token 预算耗尽且零文本 → 引擎显式发
  `iteration_budget_exhausted` / `token_budget_exhausted` 带码 error 事件,
  SDK `prompt()` 升为异常,可衔接 failover;与「模型正常空回复」不再
  不可区分。
- **P-15**:双 SDK `prompt()` 聚合器在收到全量 `agent_message` 时清空
  delta 缓冲(全量为权威快照),混合下发形态不再产生重复文本。
- **P-16**:base_url 已含版本段(`/v1`、`/api/paas/v4` 等)时不再重复
  追加 `/v1`;OpenAI / Anthropic / Responses / models 端点统一走
  `join_versioned_endpoint`(wiremock + 单测回归)。
- **P-07**:握手期子进程死亡 → Python 轮询 `poll()` 立刻抛错、TS 经
  `ready` promise 在首次使用时立刻抛错,均携带退出码与 **stderr 尾部**
  (真实原因);不再傻等满握手超时报泛化的 timed out。
- **P-04**:CLI `--help` 文案清除「v1.3 SDK / v0.4 / v0.5 / v1.6」等内部
  迭代版本标签(用户可见面与发布版本对齐;代码注释内的历史标注保留)。
- **P-14(观察项,不修)**:多轮后 assistant(tool_call) 被引擎合并为
  单条 assistant + 连续 tool 结果 —— 这是引擎既有持久化/回放形态,
  改动会波及 resume 与 cache 语义;已在缺陷修复回归组记录口径,依赖消息边界的
  网关(prompt cache / 审计重放)若有强诉求再立项。
- **P-17(适配层职责,不修)**:api_key 走 TOML / env 的转义责任在
  接入方(zhuye 不代写 config.toml);docs/sdk.md 已补充结构化接入提示。

### 实模型层观测到的上游限制(非 Zhuye 缺陷)

以下来自 `tests/live/` 对真实免费池的观测,**不计入缺陷**,但影响实模型层的
跑法与断言口径:

| 现象 | 说明 | 应对 |
|---|---|---|
| 429 限流频繁 | 免费池配额是**全局**的;OpenCode 车道按会话计额度,`retry-after` 可达 12h | live 层把 429 判为**跳过**而非失败;矩阵**串行**跑 |
| 部分模型不支持 tools | provider 直接 400(如 `dots-studio/dots-3-note-preview:free`) | 能力探测识别并跳过,不写死黑名单 |
| 首字延迟大 / 思考静默 | 实测首字 >30s、思考阶段可静默 60~70s | v1.10 起超时语义为「建连 + 读空闲」双层(默认 300s):合法长流不再被总时长腰斩;仅无字节静默超过 300s 才需调大 |
| 模型集合随时变 | 上游随时下架某个 id | 清单每次实时拉取;`[routing].fallbacks` 跨模型降级兜底 |
| prompt 可能被上游记录 | 免费池模型卡明示 | 实模型层只用合成数据 |

## 7. 用例规模汇总

| 文件 | 规模 |
|---|---|
| `functional/protocol/test_serve.py` | 39 条(D、S、A 组) |
| `functional/tools/test_tools.py` | 30 条(T 组) |
| `functional/tools/test_prompt_guard_default.py` | 12 条(F01 组:prompt_guard 默认开启 / 注入语料回归 / 工具定义扫描 / guard_stats 查询) |
| `functional/runtime/test_skills.py` | 4 条(SK1–SK4) |
| `functional/persistence/test_recovery.py` | 12 条(R6 + X6) |
| `functional/persistence/test_persistence.py` | 19 条(P 组) |
| `functional/orchestration/test_orchestration.py` | 10 条(O 组) |
| `functional/cli/test_cli.py` | 13 条(C 组) |
| `functional/runtime/test_bugfixes.py` | 20 条(缺陷修复回归) |
| `functional/cli/test_agents_md.py` | 4 条(v1.11 基建#9:AGENTS.md 默认读取) |
| `functional/runtime/test_full_auto.py` | 5 条(v1.11 基建#11:显式完全托管模式) |
| `functional/orchestration/test_goal_queue.py` | 2 条(v1.11 基建#6:队列多 goal) |
| `functional/persistence/test_memory_v11.py` | 8 条(v1.11 基建#8:deepagents 式记忆 + M5a SQLite 后端) |
| `functional/runtime/test_plugin_discover.py` | 2 条(v1.11 基建#13:GitHub topic 插件发现;网络用例 ZHUYE_FUNCTIONAL_NETWORK=1 才跑) |
| `functional/runtime/test_complexity_routing.py` | 2 条(v1.11 桌面#3:复杂度分级路由) |
| `functional/runtime/test_v110.py` | 12 条(V 组) |
| `functional/protocol/test_scenarios.py` | 7 条(并行工具及恢复、限流切换、修改审批、中断及迟到回执) |
| `functional/persistence/test_long_session.py` | 2 条(摘要压缩、正常关闭或崩溃后增量恢复) |
| `functional/persistence/test_crash_recovery_history.py` | 3 条(工具在途、工具结束及 rewind 后崩溃恢复) |
| `functional/runtime/test_approval_sequences.py` | 3 条(固定种子的审批和中断交错) |
| `functional/orchestration/test_multiagent_scenarios.py` | 4 条(响应头前或部分回复后中断、定向取消及后续回合) |
| `functional/orchestration/test_subagent_failure.py` | 3 条(失败传播、部分回复与重启恢复、临时故障恢复) |
| `functional/orchestration/test_subagent_context.py` | 4 条(默认隔离、尾部截取、工具配对与长历史) |
| `functional/orchestration/test_nested_subagents.py` | 5 条(多级成功/失败、三种定向取消路径) |
| `functional/orchestration/test_parallel_subagents.py` | 1 条(并行同角色子代理混合成功/失败) |
| `functional/orchestration/test_async_subagent.py` | 3 条(异步派发占位回执、等待期并行推进、回合挂起后同轮激活续跑;结果写入会话记录并在后续回合保留;中断挂起中的父回合级联取消子代理且不波及后续回合) |
| `functional/orchestration/test_subagent_approval.py` | 2 条(子代理写入审批的批准与拒绝) |
| `functional/orchestration/test_subagent_human_input.py` | 2 条(子代理文本和结构化人工输入) |
| `functional/orchestration/test_subagent_permissions.py` | 1 条(父级写入拒绝规则继承) |
| `functional/orchestration/test_subagent_permissions.py` | 1 条(允许写入仍受工作区边界) |
| `functional/protocol/test_model_cancellation.py` | 2 条(等待响应头及流式输出时中断) |
| `functional/protocol/test_responses_scenarios.py` | 7 条(并行分片、恢复、错误、续写与凭证/配额切换) |
| `functional/protocol/test_anthropic_errors.py` | 3 条(流内过载、限流和提前断流) |
| `functional/protocol/test_anthropic_tools.py` | 1 条(并行工具交错分片与恢复) |
| `functional/protocol/test_stream_failure.py` | 22 条(三种 provider 的断流、截断、流内错误、重试预算耗尽与恢复) |
| `functional/protocol/test_invalid_tool_arguments.py` | 3 条(损坏 JSON 不执行宿主工具,同批有效工具继续) |
| `functional/test_runner.py` | 7 条(测试入口自检) |
| `functional/runtime/test_wire_harness.py` | 8 条(隔离、会话日志、非法输出、提前断线和终态缓冲自检) |
| `functional/cli/test_agents_md.py` | 4 条(v1.11 基建#9:默认注入 / 用户级+项目级顺序 / 显式关闭 / 无文件不注入) |
| `functional/runtime/test_full_auto.py` | 5 条(v1.11 基建#11:启动通告 / yolo 别名 / bubble 自动批准 / 默认不通告 / plan 互斥) |
| `functional/orchestration/test_goal_queue.py` | 2 条(v1.11 基建#6:单 goal 字段级全链 / 多 goal 严格串行) |
| `functional/persistence/test_memory_v11.py` | 8 条(v1.11 基建#8:topic 全链路 / consolidate / 跨会话召回 / 零相关 / 后台节流;M5a:sqlite 中文检索与投影 / 删除不复活 / 笔记归属召回) |
| `functional/runtime/test_plugin_discover.py` | 2 条(v1.11 基建#13:参数面 / 真网络条件跳) |
| `functional/runtime/test_complexity_routing.py` | 2 条(v1.11 桌面#3:档位命中映射 / 回退 primary) |
| `functional/cli/test_dump_config.py` | 2 条(v1.11 F6:结构与脱敏 / 生效配置反映;env 隔离见用例内注释) |
| `functional/tools/test_spill_readback.py` | 2 条(v1.11 F5:大输出溢出落盘 workspace / read 工具读回) |
| **能力覆盖层合计** | **432 条**(pytest `--collect-only` 实测,含设施自检;含早期未逐文件登记的漂移) |
| `live/test_live_basic.py` | 4 条(L1–L4) |
| `live/test_live_tools.py` | 3 条(T1–T3) |
| `live/test_live_persistence.py` | 3 条(P1–P2、X1) |
| **实模型演练层合计** | **10 条**(默认 SKIP,需 `ZHUYE_LIVE=1`) |
| `sdk-python/test_sdk_real_bin.py` | 19 条(K 组) |
| `sdk-ts/real-bin.test.mjs` | 19 条(K 组;K10 编号与 Python 侧历史含义不同) |
| `native-python/test_native.py` | 2 条(真实 wheel、同步调用及线程行为) |
| `rust/cli_core.sh` | 72 断言 |


| `rust/cli_modules.sh` | 46 断言 |
| `rust/examples_e2e.sh` | 10 断言 |
| `rust/env_lock_stress.sh` | zhuye-config env 竞态回归门禁,两层:①**结构门禁**(确定性)`paths::env_lock_discipline_is_structurally_enforced` 用 include_str 机械校验改 env 与读 env 测试同持 `paths::ENV_LOCK`,破坏状态实证确定性失败;②**行为压测**(概率放大)满载连跑 20 轮 lib 二进制采样调度器(`44ae3da`) |
| 另有 cargo test --workspace / clippy / fmt / doc | 既有基线 |

## 8. 未覆盖项(诚实清单)



- **TUI 交互面**(modal / 按键 / 渲染):本仓库已不含 TUI 子命令,无对象。
- **集成入口 I 组用例**(原 `test_import.py`,31 条):随外部 harness 导入
  能力一并下线(commit 634bb8e),文件已删除;集成入口现由
  `runtime/test_v110.py` V12 与各集成冒烟覆盖,原 31 条断言不再存在。
- **外部 LLM provider 的实际行为**:本机 HTTP 网关已覆盖 OpenAI 兼容
  SSE 分片和 HTTP 429 凭证切换,但不能替代外部 provider 的真实限流、
  断流、延迟和模型工具选择;可选实模型层提供补充。
- **OTLP exporter 对接外部 collector 的协议矩阵**(gRPC / gzip / TLS、
  collector 5xx 重试):能力层已用本地 HTTP collector 覆盖 protobuf POST
  的真实上报(见 `test_telemetry_otlp.py`);其余传输变体按需补充。
- **其他兼容网关的交错工具分片**:OpenAI 兼容 SSE、Responses 与
  Anthropic 已有本地网关场景;其他私有协议按需补充。

## 9. bash 参数面 → Python 迁移路线(✅ 已完成)

原 `tests/rust/*.sh` 中四份深度参数面套件已全部迁入能力覆盖层
(`tests/functional/`,Python 驱动真二进制、原始 wire):

| 顺序 | bash 脚本 | 迁入位置 |
|---|---|---|
| 1 | `serve_ops.sh`(42 断言) | `test_serve.py` A 组(缺口补齐为 A28 / A29) |
| 2 | `tools_drive.sh`(40 断言) | `test_tools.py` T30–T38 |
| 3 | `integrations.sh`(44 断言) | `test_orchestration.py` O11–O16 |
| 4 | `cli_deep.sh`(194 断言) | `test_cli.py` C10–C18 |

`tests/rust/run.sh` 现仅保留:workspace 全套、`cli_core.sh` / `cli_modules.sh`
(冒烟 + 模块面)、`examples_e2e.sh`、env 竞态压测。迁移期间「两侧都跑」
的过渡已结束;后续新增 CLI 参数面断言一律写入 `tests/functional/`。
