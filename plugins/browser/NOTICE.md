# NOTICE —— 随插件分发的第三方组件

本插件 `bin/agent-browser`(构建脚本从 submodule 产出,可选捆绑)包含
第三方项目 agent-browser 的二进制。

- 源码:`third_party/agent-browser`(git submodule,URL 见仓库根
  `.gitmodules`),上游仓库 `github.com/vercel-labs/agent-browser`
- 许可证:Apache-2.0(全文见随二进制分发的 `bin/agent-browser.LICENSE`,
  构建时从 submodule 拷入)
- 版本策略:**钉住上游 release tag**(submodule 指针即版本),升级 =
  显式 bump 指针 + 提交说明(见 README「升级上游」);不做任何源码修改
  (无补丁,无本地变更,Apache-2.0 第 4(b) 条的修改声明因此不适用)
- 本插件 Rust 部分(shim)为本仓库原始代码,与上游仅通过 stdio MCP
  协议与 `AGENT_BROWSER_CDP` / `AGENT_BROWSER_SESSION` 环境变量交互,
  不链接其任何代码
