# lsp 插件(门控型,配置驱动激活)

语言服务器(LSP)集成的插件化交付:`lsp` 工具(定义 / 引用 / 诊断 /
hover,tree-sitter 之外的结构化代码导航)+ `LspConnectionManager`
(server 进程池、diff 重启)。

## 形态:门控型(激活始终需要配置)

按「形态判断铁律」,LSP 是外部系统接入(language server 子进程),
此前已由 `[lsp_servers]` 配置门控(可启停目标已达成)。插件化保持
**config 语义不变**:

- **激活**:`~/.zhuye/config.toml` 配置 `[lsp_servers.<name>]`
  (command / args / languages 等;`zhuye lsp add` 可代写)→ 启动时
  拉起 server 并注册 `lsp` 工具。**没有 server 定义时插件启用也无事
  可做**(插件不携带 server 定义)。
- **粗粒度开关(新增)**:`zhuye plugin disable lsp@inline`,或 config
  `[plugins].disabled_plugins` 含 `"lsp"` → 即使 `[lsp_servers]` 有
  配置也**整体关闭**:lsp 工具不注册、server 不启动。

| 入口 | 效果 |
|---|---|
| `[lsp_servers]` 配置存在(默认态) | LSP 启用(与插件化之前一致) |
| 插件显式禁用 | LSP 整体关闭 |
| 插件启用(enable) | 无独立效果的显式表态(激活仍看配置) |

## CLI

`zhuye lsp add / list / remove` 只读写 `[lsp_servers]` 段,参数面不变。
