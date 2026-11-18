# tests/verification/ —— 事故回放与核心语义差分

本目录承载 VERIFICATION_ENHANCEMENT_PLAN.md 的 P1 层:把已修复事故变成
**长期可执行**的差分用例,并用「已知缺陷 + 正常控制组」证明验证器自身有效。

## 组成

| 路径 | 职责 |
|---|---|
| `differential.py` | 差分执行器:驱动 baseline/candidate 两个真实 `serve` 二进制,产出语义投影并比较 |
| `test_differential.py` | 纯函数层自测(变异注入)与语料库自洽检查;不调用二进制,PR 必跑 |
| `cases/*.json` | 确定性差分用例:mock 脚本 + 场景参数 + 批准差异清单 |
| `incidents/*.json` | 事故元数据:真实来源提交、oracle、触发路径、控制组标记 |
| `reliability.json` | diff 触发的可靠性规则表(触发路径 → 应追加的定向任务) |
| `select_reliability.py` | 选择器:按 git 基线或文件清单输出应**追加**的定向检查(不替代完整选集) |

选择器用法:

```bash
python3 tests/verification/select_reliability.py --base origin/develop
printf 'crates/protocol/src/lib.rs\n' | python3 tests/verification/select_reliability.py --from-file -
```

## 设计原则

1. **语义差分,而非全文 JSON 对比**:比较回合终态、回复文本、工具调用序列、
   审批请求、错误事件、文件副作用与 rollout 记录类型。session id、时间戳、
   耗时、临时路径等易变字段从不进入投影。`submission_closed` 位于回合终态后,
   是否被采集到取决于接收线程调度,不用于事件类型差分。
2. **Protocol v0 兼容**:候选新引入的事件类型必须逐条出现在用例的
   `approved_differences`(格式 `event:<TYPE>`);基线事件类型丢失不接受豁免。
3. **mock 模型 + 隔离沙箱**:与能力覆盖层(`tests/functional/wire.py`)同一套
   夹具,离线可重复,不消耗模型额度。
4. **验证器自证**:每个维度(终态/工具/审批/文件/事件类型)都有对应的注入
   变异测试;变异必被抓、控制组必零分歧。
5. **事故预期独立判定**:INC-002 要求候选触发 bash 审批且拒绝后没有 out.txt,
   即使基线有相同缺陷也会失败。两侧事件与投影分别写入输出目录的
   `baseline/` 和 `candidate/`。

## 运行

```bash
# 纯函数自测(不需要二进制,agent.contract.verification)
python3 tests/verification/test_differential.py

# 真实差分(需要两个版本的二进制,agent.contract.differential 条件任务)
python3 tests/verification/differential.py \
    --baseline target/release/zhuye --candidate target/debug/zhuye \
    --output target/verification
python3 tests/runner.py run --task agent.contract.differential  # 需 ZHUYE_DIFF=1
```

## 添加新事故

1. 确认可在 mock 模型下确定性复现(不依赖真实模型、真实网络、竞态时序);
2. 新增 `cases/<id>.json`(kind 必须在 `differential.KNOWN_KINDS` 中;新 kind
   需同步实现 driver)与 `incidents/INC-xxx.json`(来源提交号必填,自测会用
   `git rev-parse --verify` 校验真实存在);
3. 用例的 `source_incident` 与事故的 `case_file` 必须双向闭环;
4. 若新用例涉及 Protocol v0 合法新增事件,在用例 `approved_differences`
   里逐条列出,不得批量豁免;
5. 更新本 README 与 `tests/COVERAGE.md`。

单次事故立即补用例;只有重复出现且可由 diff 机械识别的问题才提炼为
`reliability.json` 的通用触发规则。
