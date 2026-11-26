# zhuye-kms AGENTS.md

> **何时读我**:改动本地知识库 wiki(页面读写 / grep 检索 / dream 固化 / 平台 I/O 后端 / MCP server 二进制)时,读本篇。

## 本 crate 是什么

基于 grep 的本地知识库 wiki(KMS):知识库以 Markdown 存于
`~/.zhuye/kms/<name>/pages/*.md`,提供 list / get / search / dream(会话洞察
固化)等操作。交付形态是独立二进制 `zhuye-kms-server`(MCP stdio),由
`plugins/kms` 挂载,**不进 SDK facade**。

## 核心文件导航

| 路径 | 一句话职责 |
|---|---|
| `src/lib.rs` | `KnowledgeManager`(全部操作的入口)+ 公开类型 `WikiInfo` / `Page` / `SearchResult` / `DreamResult` / `KmsError` |
| `src/browse.rs` | `list_wikis` / `search`(grep)—— 以 `impl KnowledgeManager` 分文件承载方法 |
| `src/dream.rs` | `dream`:从会话提取的洞察写入 `dream-insights` wiki |
| `src/helpers.rs` | `SaveMode`(Overwrite / Create / IfUnchanged 乐观锁)+ frontmatter 提取 helper |
| `src/path_guard.rs` | `validate_name`(名称字符 / 长度 / 平台保留名)+ `reject_links`(拒绝符号链接逃逸) |
| `src/tests.rs` | `KnowledgeManager` 集成测试(`#[cfg(test)] mod tests`) |
| `src/platform/mod.rs` | 平台 I/O 后端分派;两套实现的语义差异由本层吸收,上层只面对同一套语义 |
| `src/platform/unix/` | Unix 后端:目录句柄 + `rustix` openat 相对操作(`admin` 建/删、`browse` 列举、`page` 读写;`page/tests.rs` 锁定目录替换攻击场景) |
| `src/platform/windows/` | Windows 后端:不允许删除共享的目录句柄固定路径 + 重解析点校验(同 admin/browse/page 三分) |
| `src/bin/zhuye-kms-server.rs` | MCP stdio server(`kms_*` 工具面),手写 JSON-RPC,stdout 只出协议响应 |

## 本地契约与同步点

- **模块全私有,仅类型公开**:子模块一律 `mod`(`browse` / `dream` / `helpers` /
  `path_guard` / `platform` 均无 `pub`),对外只走 `zhuye_kms::KnowledgeManager`
  的方法面与 `lib.rs` 定义的公开类型 —— 内部文件布局可自由重组,下游不受影响;
  新增能力时先加方法,不要把内部模块改成 `pub`。
- **所有名称进文件系统前必须过 `path_guard`**:写路径(`create_wiki` /
  `delete_wiki` / `save_page*`)持 `write_lock`(parking_lot Mutex)串行化,
  且先 `validate_name` + `reject_links`;读路径同样校验。绕过守卫 = 路径穿越 /
  符号链接逃逸直接暴露给 MCP 工具调用方。
- **平台差异只在 `platform/` 内消化**:Unix 用 openat 不跟随符号链接防 TOCTOU
  与目录替换攻击;Windows 固定目录句柄 + 打开后检查重解析点。改任一侧实现时,
  `platform/unix/page/tests.rs` 的「pages 目录被替换后写入不得重定向」用例必须
  保持通过,新增攻击场景在同处补对称用例。
- **页面保存三态**:`save_page`(覆盖)/ `create_page`(存在即不覆盖)/
  `save_page_if_unchanged`(内容与读取时不一致报 `PageChanged`)—— GUI 乐观锁
  依赖第三态,不得合并或改变语义。
- **无显式 title/tags 时原样保存正文**,以支持对含 frontmatter 的完整页面做
  往返编辑(见 `save_page_inner` 注释);加了元数据才重组 frontmatter。

## 禁止事项

1. 不要把 `platform::*` 或 `path_guard` 的函数改成公开 —— 外部能力只经
   `KnowledgeManager` 方法暴露。
2. 不要在 `KnowledgeManager` 之外直接拼 `~/.zhuye/kms` 路径做 IO —— 落盘位置
   统一走 `zhuye_config::data_path_or_relative(&["kms"])`。
3. `zhuye-kms-server` 二进制不要进 SDK facade 或注册进核心工具注册表(与
   computer-use / shell / git-tools 等插件能力本体同口径)。

## 深入阅读

- [crates/integrations/AGENTS.md](../AGENTS.md) —— 本层定位与「KMS 知识库」契约节
- [docs/plugin-dev-guide.md](../../../docs/plugin-dev-guide.md) —— 插件挂载形态
