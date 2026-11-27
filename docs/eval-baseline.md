# 真实任务评测:基线与前后对比

本文件说明如何用仓库内既有设施(「固定任务集 + 客观验收 + 评分 + 质量门禁」)
建立**可重复**的基线,并在改动前后比较成功率、耗时与成本。

## 组成

| 部件 | 位置 | 作用 |
|---|---|---|
| 固定任务集 | `tests/eval/cases.json` | 18 条任务:修复、合并、长上下文恢复、不可信文件、同批读写、隔离失败、中断恢复与并发回合;带 `sha256` 指纹写进报告 |
| 运行器 | `tests/eval/run.py` | 同一模型、同一提示词方案下跑「基线二进制 vs 候选二进制」,相邻执行每对同任务样本并轮换先后,记录结果、延迟、token、工具调用及失败轨迹 |
| 验收器 | `tests/eval/run.py`(`verify`) | **不信模型自述**:独立跑外部程序验证产物与受保护文件 |
| 评分器 | `crates/abilities/zhuye-eval`(`zhuye-eval-score`) | 回复质量与工具预算断言 |
| 质量门禁 | `tests/eval/policy.json` + `tests/eval/eval_policy.py` | 纯函数判定:成功率退化 / 整例退化 / 耗时或 token 回退 / 样本量 |

## 离线自检(无凭据、无费用、CI 可跑)

```bash
bash tests/eval/run_offline.sh          # 或 ./tests/run_all.sh eval-offline
```

用 mock provider 把任务「正确地做完」,验证沙箱 / 验收 / 评分 / 门禁整条链路
可用。**离线自检的成功率衡量的是流水线,不是模型能力** —— 它不能当质量基线。

## 真实基线(需要模型凭据)

```bash
export ZHUYE_EVAL_BASELINE=/path/to/baseline-zhuye     # 改动前的二进制
export ZHUYE_EVAL_CANDIDATE=/path/to/candidate-zhuye   # 改动后的二进制
export ZHUYE_EVAL_MODEL=<固定模型 id>
export ZHUYE_EVAL_BASE_URL=<provider 地址>
export ZHUYE_EVAL_API_KEY=<凭据>
export ZHUYE_EVAL_REPETITIONS=3
bash tests/eval/run_quality.sh
```

产物落在 `target/eval-quality/report.json`(可用 `ZHUYE_EVAL_OUTPUT` 改):

- `versions.<role>.cases.<case>` —— 每例的 `pass/fail/infra_error`、`mean_elapsed_s`、
  `input_tokens` / `output_tokens`、`cost_usd`(给了单价时)、`tool_calls` / `tool_errors`;
- `versions.<role>.categories.<kind>` 汇总同类别的任务。用例与类别均包含
  `success_rate`、`cache_hit_rate`、缓存读写 token 及独立的保温调用统计。
- `versions.<role>.results[]` —— 每条的失败原因与脱敏事件轨迹(`events.json`);
- `comparison.<case>` 与 `category_comparison.<kind>` 提供前后差值,包括
  `pass_delta`、`mean_elapsed_delta_s`、`input_tokens_delta` / `output_tokens_delta`、
  `tool_calls_delta`、`tool_errors_delta`。如一侧存在基础设施错误,仍保留结果计数差,
  质量和使用量差值记为 `null`。

门禁模式由 `ZHUYE_EVAL_MODE` 控制:`observe`(默认)只记录 `would_block`,`enforce`
在退化或样本不足时返回非零退出码。阈值在 `tests/eval/policy.json`;按其说明,
至少积累 10 次成对真实样本后再校准并转入 `enforce`。

评测器在隔离的合成任务中以 `manual_approve` 回应计划交互。规划结束后保留逐工具
审批策略,收到单独的工具审批请求时明确拒绝。恢复会话只处理新收到的请求。

长上下文任务固定使用 1000 token 触发阈值,要求重启前观察到模型摘要事件,
重启后才创建符合约定的文件。摘要事件表示摘要已应用,保留的近期消息仍会占用上下文,
因此应另看报告中的 token 变化。

## 记录基线的要求

1. 固定模型 id、provider 地址、提示词方案(`--baseline-profile` / `--candidate-profile`)与
   重复次数 —— 换任何一个都会让前后对比失去意义;
2. 归档 `report.json` 与被比较的两个二进制的 `sha256`(报告里已带);
3. 报告里出现的 `infra_error` 必须先排除(基础设施问题不算质量退化);
4. 通过 `ZHUYE_EVAL_INPUT_PRICE` 与 `ZHUYE_EVAL_OUTPUT_PRICE` 提供每百万 token 的美元单价
   (运行器参数为 `--input-price` 与 `--output-price`)。缺少单价或 usage 时,`cost_usd` 为 `null`。
   输入统一按输入单价估算,缓存读写 token 独立统计。以完整回合累计用量为准,
   中断回合使用退出前收到的模型调用回执。估算范围由会话事件覆盖,
   未上报的用量以及独立的缓存保温与摘要调用可能产生额外费用。
