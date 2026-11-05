# git-tools —— git 工作流插件

复用框架 `zhuye-git` 能力族(分支/PR 探测 + tag 化 checkpoint)。

## 安装(三步)

```bash
./scripts/build_git_tools_plugin.sh
zhuye plugin install plugins/git-tools --scope user
zhuye plugin enable git-tools@inline
```

## MCP 工具(全名 `mcp__plugin:git-tools@inline:git__<tool>`)

| 工具 | 说明 |
|---|---|
| `git_branch_summary` | 分支 + 与默认分支的 diff 统计 + 打开的 PR(gh;尽力而为) |
| `git_checkpoint_create` | 打 tag 快照(可 rewind;无改动返回 no-changes) |

## Stop 自动 checkpoint(可选)

`hooks/hooks.json` 挂了 Stop hook(`zhuye-cp-hook`):每轮 turn 结束自动
打快照。hook **永远退出 0** —— 外部 hook fail-closed(非零 = Deny),
而 Stop 的 Deny 语义是强制续跑;checkpoint 失败不能拦截 turn。不需要
自动快照时在 `zhuye plugin` 管理里去掉 hooks 能力即可。

## slash 命令

- `/git-summary [cwd]` —— 探测汇总;
- `/checkpoint [标签]` —— 显式快照。
