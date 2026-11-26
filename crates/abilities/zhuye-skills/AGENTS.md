# zhuye-skills AGENTS.md

> **何时读我**:改动 SKILL.md 解析 / 技能目录与激活 / 内置技能包 / 市场安装与供应链安全时,读本篇。

## 本 crate 是什么

SKILL.md(带 YAML frontmatter 的 Markdown)的扫描、加载与工具激活:启动时
`SkillsCatalog::scan` 遍历配置目录解析全部 SKILL.md,渲染文本索引由 `pre_loop`
以 system-reminder 注入;LLM 调 `LoadSkillTool` 激活技能后,其声明的 `tools:`
列表才对模型可见。另含市场发现 / 安装 / 供应链安全(F27)与技能长尾(F58)。

## 核心文件导航

| 路径 | 一句话职责 |
|---|---|
| `src/lib.rs` | crate 门面:公开 API 扁平 re-export |
| `src/model.rs` | `SkillMeta`(解析后的 SKILL.md)与 `SkillError` |
| `src/loader.rs` | frontmatter 解析器(`parse_skill_file` / `parse_skill_str`) |
| `src/scanner.rs` | `scan_skills_dirs`:递归找 SKILL.md,解析失败记日志跳过 |
| `src/catalog.rs` | `SkillsCatalog`:持有解析结果与激活集合;`render_catalog` 渲染索引、`active_tool_names` 过滤可见工具 |
| `src/discovery.rs` | 路径条件激活:workspace 相对路径命中 `when_paths` glob 时自动激活 |
| `src/bundled.rs`(+`src/bundled/*.md`) | 15 项编译期内置技能元数据,启动时可 merge 进 catalog |
| `src/tool.rs` | `LoadSkillTool`(按名激活)+ `ReadSkillResourceTool`(读技能附属资源) |
| `src/longtail.rs` | F58:CLI 帮助生成技能 / 输出合同 / 价值度量 / 技能链 |
| `src/market/directory.rs` | skills.sh 目录发现:sitemap 全量索引拉本地缓存,本地字符串匹配搜索,无认证依赖 |
| `src/market/marketplace.rs` | 安装:GitHub 单仓形态(只拉 SKILL.md)与 skills.sh 目录生态(tarball 整目录提取) |
| `src/market/supply_chain.rs` | F27:锁文件 `SkillsLock` / sha256 完整性校验 / 安装前安全扫描(block / warn 两级) |

## 本地契约与同步点

- **激活模型是「先注入索引、后显式激活」**:catalog 索引经 `pre_loop` 注入 →
  LLM 调 `LoadSkillTool` → 该 skill 的 `tools:` 标记为激活态 →
  `SkillsCatalog::active_tool_names` 才把它放行进 LLM 可见工具 schema。
  "always-on" 工具名(无需激活即可见)由**调用方**在 `SkillsCatalog::new`
  设置,本 crate 不自带权威清单。
- **市场链路方向固定**:发现(`directory`)→ 安装(`marketplace`)→ 校验
  (`supply_chain`),`market/mod.rs` 明示这条链;安装动作不自带安全检查,
  必须由调用方先过 `scan_skill` / 锁文件校验。
- **安装后无注册表**:落盘 `<out_dir>/<name>/SKILL.md` 后由既有扫描链
  (`scan_skills_dirs`)自然发现 —— 不要引入注册表或旁路注册入口。
- **锁文件单一实现**:F58 与 F27 曾各实现一套结构相同的锁,现已合并为 F27 的
  `SkillsLock`(`longtail.rs` 内有显式说明)—— 不要再新建第二套锁结构。
- **单仓形态不拉附属文件**:`install_from_github` 只取 SKILL.md 原文,scripts /
  references 由用户按需手动补(渐进披露,避免盲目拉取可执行内容);目录生态
  形态才整目录提取。

## 禁止事项

1. 不要绕过 `supply_chain` 直接执行市场安装(锁校验 + 安装前扫描是引入外部
   技能的强制前置)。
2. 不要把解析失败的 SKILL.md 变成硬错误 —— 约定是记日志跳过,不中断整体扫描。
3. `bundled/` 下的 md 资源只随 `bundled.rs` 的清单扩展,两者必须同步增删。

## 深入阅读

- [crates/abilities/AGENTS.md](../AGENTS.md) —— 本层定位与 skills 速览行
- [docs/features.md](../../../docs/features.md) —— skills 目录的产品定位
