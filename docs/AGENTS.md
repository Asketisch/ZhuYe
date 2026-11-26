# docs/ —— 正式文档目录

**何时读我**:要写或改 `docs/` 下任何文档前;要找某主题的正式说明时。

本目录是**用户面与贡献者面的正式文档**(区别于各目录的 AGENTS.md
—— 那是给 agent 的操作契约,这里是给人读的系统说明)。入口索引在根
[README.md](../README.md) 的文档表;架构总览见
[architecture.md](architecture.md)。

## 主题 × 语言对照(14 主题 × 2 语言 + 1 单语)

| 主题 | 中文 | 英文 |
|---|---|---|
| 架构总览 | [architecture.md](architecture.md) | [architecture.en.md](architecture.en.md) |
| 功能清单与形态判断 | [features.md](features.md) | [features.en.md](features.en.md) |
| 工具与 hooks 协议 | [tools-and-hooks.md](tools-and-hooks.md) | [tools-and-hooks.en.md](tools-and-hooks.en.md) |
| Plan mode | [PLAN_MODE.md](PLAN_MODE.md) | [PLAN_MODE.en.md](PLAN_MODE.en.md) |
| 配置 | [configuration.md](configuration.md) | [configuration.en.md](configuration.en.md) |
| CLI | [cli.md](cli.md) | [cli.en.md](cli.en.md) |
| 插件系统 | [plugins.md](plugins.md) | [plugins.en.md](plugins.en.md) |
| 插件开发指南 | [plugin-dev-guide.md](plugin-dev-guide.md) | [plugin-dev-guide.en.md](plugin-dev-guide.en.md) |
| 插件市场 | [plugin-marketplace.md](plugin-marketplace.md) | [plugin-marketplace.en.md](plugin-marketplace.en.md) |
| 提示词方案 | [prompts.md](prompts.md) | [prompts.en.md](prompts.en.md) |
| 脱敏 | [sanitize.md](sanitize.md) | [sanitize.en.md](sanitize.en.md) |
| SDK | [sdk.md](sdk.md) | [sdk.en.md](sdk.en.md) |
| 发布 | [release.md](release.md) | [release.en.md](release.en.md) |
| 开发流程 | [development.md](development.md) | [development.en.md](development.en.md) |
| 评测基线(单语) | [eval-baseline.md](eval-baseline.md) | — |

## 约定与硬约束

- **双语同步**:除 `eval-baseline.md` 外,每个主题必须同时维护 `.md`
  与 `.en.md` 两份;改其一必须同步另一份(历史全部成对,新主题不许只
  写单语)。
- **链接用相对路径**(`xxx.md` / `../README.md`),不用仓库根绝对路径
  —— 文档会被渲染到仓库外页面。
- **本目录刻意保持平铺**:文档互链与根 README 索引表按「同级兄弟」
  组织;不分子目录。若确需子目录,先同步根 README 索引表与既有互链。
- **门禁**:[check_stale_doc_phrases.py](../scripts/check_stale_doc_phrases.py)
  递归扫描 `docs/**/*.md` 的过期措辞;
  [check_brand_names.py](../scripts/check_brand_names.py) 对
  `docs/prompts.md` / `docs/prompts.en.md` 有**路径限定**的品牌名豁免
  (上游数据集格式标识符)—— 这两个文件**不得移动或改名**,否则豁免
  失效、门禁直接失败。
- 行号会漂移:引用代码位置时优先写「文件 + 符号名」,少写「文件:行号」。

## 禁止事项

- 不在文档里写真实密钥或凭据(门禁与常识双重约束)。
- 不为尚未实现的特性写文档;新特性文档随代码同 PR 落地。
- 不把 AGENTS.md 的操作契约复制进来 —— 两套体系各管各的,交叉用链接。

## 深入阅读

- [../AGENTS.md](../AGENTS.md) —— 根篇(体系导航与开发流程检查表)
- [architecture.md](architecture.md) —— 六层 workspace 与核心数据流
- [features.md](features.md) —— 能力清单 + 形态判断(主循环 vs 插件)决策矩阵
