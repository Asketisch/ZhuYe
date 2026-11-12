"""test_registry.py —— Reflect-Agent(基础 Runtime + 双 SDK)任务注册表。

任务 ID 与 PLAN.md 第 2 章(以及公共基座 B 系列)对应;每条映射到
仓库现有测试入口,不复制测试逻辑。coverage 语义:

- mapped       已有实现映射,命令真实执行
- pending      PLAN 已登记、仓库尚无对应实现(报告为「未覆盖」,不算通过)
- conditional  条件能力(如 live 层),未启用时报「条件不适用」
"""

# --------------------------------------------------------------------------- #
# 任务定义
# --------------------------------------------------------------------------- #

TASKS: dict[str, dict] = {
    # ---- 公共基座(PLAN §1.5,B01~B16 的调度器侧) ----
    "shared.harness": {
        "builtin": "shared.harness",
        "note": "测试基座自检:Python 版本 / 工具链可用性 / 报告目录可写",
    },

    # ---- 静态、构建与公开接口(PLAN §2.1,A-S01~A-S10) ----
    "agent.static": {
        # A-S01 格式与 lint:fmt 检查 + clippy -D warnings
        "command": ["cargo", "fmt", "--all", "--check"],
        "note": "A-S01(前半):cargo fmt 检查;clippy 由 agent.build 承担",
    },
    "agent.build": {
        # A-S01(后半)+ A-S02:clippy 全目标 + 全 workspace 编译
        "command": ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
        "note": "A-S01/A-S02:clippy -D warnings(全目标编译随之覆盖)",
    },
    "agent.docs": {
        # A-S08 协议文档门禁 + A-S10 发布检查(dry-run)
        "command": ["scripts/release_sdk.sh", "check_docs"],
        "note": "A-S08:双 README 与双类型层的 Op 收录校验",
    },
    "agent.release": {
        # A-S10 / A-Q13~A-Q16 的发布前检查入口:版本一致性 + 双包构建(dry-run)
        "command": ["scripts/release_sdk.sh", "check"],
        "note": "A-S10:版本一致性 + 双包构建检查(不发布)",
        "uses_artifact": True,
    },

    # ---- Runtime 单元与组件(PLAN §2.2,A-U01~A-U20) ----
    "agent.unit.engine": {
        # A-U01~A-U08:引擎回路、取消、并发 —— cargo 单测承担
        "command": ["cargo", "test", "-p", "zhuye-core"],
        "note": "A-U01~A-U08:StateGraph 引擎单元测试",
    },
    "agent.unit.context": {
        # A-U11~A-U15:上下文组装 / 裁剪 / 压缩 / prompt 开关
        "command": ["cargo", "test", "-p", "zhuye-compact", "-p", "zhuye-prompt"],
        "note": "A-U11~A-U15:压缩与 prompt crate 单元测试",
    },
    "agent.unit.config": {
        # A-U16~A-U18:配置优先级 / 热更新 / 计量
        "command": ["cargo", "test", "-p", "zhuye-config"],
        "note": "A-U16~A-U18:配置 crate 单元测试",
    },

    # ---- 公共协议与 headless CLI(PLAN §2.4,A-C01~A-C10) ----
    "agent.contract.protocol": {
        # A-C01~A-C08:真实 serve 的 wire 断言(能力覆盖层,字段级)
        "command": ["tests/run_all.sh", "functional-layer"],
        "note": "A-C01~A-C08:能力覆盖层(Python 打真二进制,原始 wire)",
        "timeout_s": 3600,
    },
    "agent.contract.event": {
        # A-C01~A-C08 的事件形状底线:Rust enum ↔ 全变体样本 ↔ serde roundtrip
        "command": ["cargo", "test", "-p", "zhuye-protocol", "--test", "event_contract"],
        "note": "EventMsg 50 个已完成变体的全量线格式契约",
    },
    "agent.contract.properties": {
        # 协议信封与 Op 的属性测试:随机文本 / 序列形状下 serde 往返不漂移
        "command": ["cargo", "test", "-p", "zhuye-protocol", "--test", "protocol_properties"],
        "note": "Op / Submission / Event 属性往返",
    },
    "agent.contract.cli": {
        # A-C09~A-C10:CLI 参数矩阵(旧套件保留参数面)
        "command": ["tests/run_all.sh", "rust"],
        "note": "A-C09~A-C10:CLI 参数面(cargo test + bash 参数矩阵)",
        "timeout_s": 3600,
    },

    # ---- 工具、权限与真实副作用(PLAN §2.5,A-I01~A-I22) ----
    "agent.integration.tools": {
        # 能力覆盖层断言到字段级,工具/审批统一走该入口
        "command": ["tests/run_all.sh", "functional"],
        "note": "A-I01~A-I22:能力覆盖层 + Rust 参数面 + 双 SDK 绑定层",
        "timeout_s": 5400,
    },
    "agent.integration.approval": {
        # 审批用例在能力覆盖层内,独立任务便于定向执行
        "command": ["tests/run_all.sh", "functional-layer"],
        "note": "A-I12~A-I16:审批与权限(能力覆盖层内含)",
        "timeout_s": 3600,
    },

    # ---- 扩展、记忆与多 Agent 编排(PLAN §2.6,A-X01~A-X30) ----
    "agent.integration.extensions": {
        "command": ["tests/run_all.sh", "functional-layer"],
        "note": "A-X01~A-X13:MCP/LSP/skill/插件/WASM/memory/KMS 能力面",
        "timeout_s": 3600,
    },
    "agent.integration.orchestration": {
        # A-X19~A-X26:讨论 / pipeline / goal 编排冒烟在 scripts/discussion_smoke.sh
        "command": ["scripts/discussion_smoke.sh"],
        "note": "A-X19~A-X26:discussion 编排冒烟(其余编排随 functional)",
        "timeout_s": 1800,
    },

    # ---- 持久化、恢复与故障(PLAN §2.7,A-R01~A-R12) ----
    "agent.integration.persistence": {
        "command": ["tests/run_all.sh", "functional-layer"],
        "note": "A-R01~A-R12:会话持久化 / 恢复(能力覆盖层内含)",
        "timeout_s": 3600,
    },
    "agent.fault": {
        "command": [
            "cargo", "test", "-p", "zhuye-rollout", "--test", "resume_loads_history",
            "record_fails_on",
        ],
        "note": "A-R02~A-R11 只读目录与真实 ENOSPC 磁盘满故障",
    },
    "agent.fault.lock": {
        "command": ["cargo", "test", "-p", "zhuye-task", "--test", "file_store_lock_contention"],
        "note": "跨 FileTaskStore 并发锁竞争:ID 唯一且任务不互相覆盖",
    },

    # ---- Provider 与网络契约(PLAN §2.3,A-P01~A-P14) ----
    "agent.contract.provider": {
        "command": ["cargo", "test", "-p", "zhuye-llm"],
        "note": "A-P01~A-P06:provider 请求转换与流式解析单元测试",
    },
    "agent.integration.provider": {
        # 代理 / TLS / 真实端点矩阵为 P1/条件项
        "coverage": "conditional",
        "note": "A-P07~A-P14:HTTP 错误 / 代理矩阵 —— 需真实端点(ZHUYE_LIVE=1)",
    },

    # ---- 双 SDK 与 PyO3 绑定(PLAN §2.8,A-K01~A-K22) ----
    "sdk.ts.unit": {
        "command": ["npm", "--prefix", "sdks/typescript", "test"],
        "note": "A-K01~A-K11(TS 侧):mock serve 纯逻辑",
        "timeout_s": 1200,
    },
    "sdk.python.unit": {
        "command": ["python3", "-m", "pytest", "sdks/python/tests", "-q"],
        "note": "A-K01~A-K11, A-K14~A-K15(Python 侧):mock serve 纯逻辑",
        "timeout_s": 1200,
    },
    "sdk.binding": {
        # A-K12~A-K13:双 SDK 打真二进制
        "command": ["tests/run_all.sh", "sdk-python"],
        "note": "A-K12(TS 侧由 sdk.binding.ts 承担,此处为 Python 绑定层)",
        "timeout_s": 3600,
    },
    "sdk.binding.ts": {
        "command": ["tests/run_all.sh", "sdk-ts"],
        "note": "A-K12/A-K13(TS 侧):真二进制绑定层",
        "timeout_s": 3600,
    },
    "sdk.native-python": {
        # A-K19~A-K22:PyO3 wheel 构建与验证
        "command": ["tests/run_all.sh", "native-python"],
        "note": "A-K19~A-K22:构建 wheel 并验证原生绑定",
        "timeout_s": 3600,
    },

    # ---- 安全、属性、性能与评测(PLAN §2.9,A-Q01~A-Q19) ----
    "agent.security": {
        # A-Q01~A-Q05:权限 / 路径 / 凭据隔离 —— 能力覆盖层含安全断言
        "command": ["tests/run_all.sh", "functional-layer"],
        "note": "A-Q01~A-Q05:安全相关能力断言(能力覆盖层内含)",
        "timeout_s": 3600,
    },
    "agent.fuzz": {
        "command": ["cargo", "test", "-p", "zhuye-protocol", "--test", "event_mutation_fuzz"],
        "note": "A-Q07:协议事件确定性变异 + 万级可配置多种子多重变异",
    },
    "agent.performance": {
        "command": ["scripts/bench.sh"],
        "note": "A-Q10~A-Q12:性能基线(冷启动 / 首事件 / RSS)",
        "timeout_s": 1800,
    },
    "agent.soak": {
        # A-Q08:高负载下重复执行配置 env 锁用例;长会话链路由 functional 承担。
        "command": ["tests/rust/env_lock_stress.sh"],
        "note": "A-Q08:高负载 env 锁压测;长会话压缩/重启随 functional 执行",
        "timeout_s": 3600,
        "os": ["linux", "darwin"],
    },
    "agent.eval": {
        # A-Q17~A-Q19:live 层,默认 SKIP,需 ZHUYE_LIVE=1
        "coverage": "conditional",
        "command": ["tests/run_all.sh", "live"],
        "note": "A-Q17~A-Q19:实模型演练层(需 ZHUYE_LIVE=1 与真实凭据)",
        "enabled": False,
    },
    "agent.e2e": {
        "command": ["scripts/e2e.sh"],
        "note": "13 步端到端冒烟:构建 + examples + headless + test + clippy",
        "timeout_s": 5400,
    },
}

# --------------------------------------------------------------------------- #
# 选集(PLAN §5.2 执行阶段 → 任务分组)
# --------------------------------------------------------------------------- #

SUITES: dict[str, list[str]] = {
    # PR 必需门禁:静态、单元、协议、确定性能力、双 SDK、文档、核心安全
    "pr": [
        "agent.static", "agent.build", "agent.docs",
        "agent.unit.engine", "agent.unit.context", "agent.unit.config",
        "agent.contract.protocol", "agent.contract.event", "agent.contract.properties", "agent.contract.cli",
        "sdk.ts.unit", "sdk.python.unit",
    ],
    # 冒烟:少量关键路径(真实文件结果 + 资源清理)
    "smoke": ["agent.contract.protocol", "agent.e2e"],
    # 主分支:PR 层 + 双 SDK 真产物
    "main": [
        "agent.static", "agent.build", "agent.docs",
        "agent.unit.engine", "agent.unit.context", "agent.unit.config",
        "agent.contract.protocol", "agent.contract.event", "agent.contract.properties", "agent.contract.cli",
        "agent.integration.tools", "agent.integration.approval",
        "agent.integration.extensions", "agent.integration.persistence",
        "agent.contract.provider",
        "sdk.ts.unit", "sdk.python.unit", "sdk.binding", "sdk.binding.ts",
        "sdk.native-python", "agent.security",
    ],
    # 夜间:完整故障、编排、fuzz、性能
    "nightly": [
        "agent.integration.orchestration", "agent.integration.provider",
        "agent.fault", "agent.fault.lock", "agent.fuzz", "agent.performance",
        "agent.soak", "agent.eval",
    ],
    # 发布候选:最终产物与版本矩阵
    "release": [
        "agent.docs", "agent.release", "sdk.binding", "sdk.binding.ts",
        "sdk.native-python", "agent.e2e",
    ],
}

# --------------------------------------------------------------------------- #
# 用例索引:PLAN 用例 ID → 任务(供 --case 定向执行)
# 粒度说明:能力面用例在能力覆盖层内部有独立断言,此处映射到承载任务;
# 未收录的用例 ID 会得到明确报错,而不是静默通过。
# --------------------------------------------------------------------------- #

CASE_INDEX: dict[str, str] = {
    # 公共基座
    **{f"B{i:02d}": "shared.harness" for i in range(1, 17)},
    # 静态与构建
    **{f"A-S{i:02d}": ("agent.docs" if i == 8 else
                        "agent.release" if i == 10 else
                        "agent.static" if i == 1 else "agent.build")
       for i in range(1, 11)},
    # Runtime 单元
    **{f"A-U{i:02d}": ("agent.unit.engine" if i <= 8 else
                        "agent.unit.context" if i <= 15 else "agent.unit.config")
       for i in range(1, 21)},
    # Provider 契约
    **{f"A-P{i:02d}": ("agent.contract.provider" if i <= 6 else
                        "agent.integration.provider")
       for i in range(1, 15)},
    # 协议与 CLI
    **{f"A-C{i:02d}": ("agent.contract.protocol" if i <= 8 else "agent.contract.cli")
       for i in range(1, 11)},
    # 工具与权限
    **{f"A-I{i:02d}": ("agent.integration.approval" if 12 <= i <= 16
                        else "agent.integration.tools")
       for i in range(1, 23)},
    # 扩展与编排
    **{f"A-X{i:02d}": ("agent.integration.orchestration" if 19 <= i <= 26
                        else "agent.integration.extensions")
       for i in range(1, 31)},
    # 持久化与故障
    **{f"A-R{i:02d}": ("agent.fault" if 2 <= i <= 11
                        else "agent.integration.persistence")
       for i in range(1, 13)},
    # SDK
    **{f"A-K{i:02d}": ("sdk.native-python" if i >= 19 else
                        "sdk.python.unit" if i in (14, 15) else
                        "sdk.binding" if i in (12, 13) else "sdk.ts.unit")
       for i in range(1, 23)},
    # 安全与性能
    **{f"A-Q{i:02d}": ("agent.security" if i <= 5 else
                        "agent.fuzz" if i == 7 else
                        "agent.soak" if i == 8 else
                        "agent.performance" if i <= 12 else
                        "agent.release" if i <= 16 else "agent.eval")
       for i in range(1, 20)},
}
