# verify-hooks 插件(门控型,默认关)

`verification` / `test_runner` 两个内置 hook 的插件化开启路径。

- **verification**(Stop hook):turn 结束前跑校验命令(默认
  `cargo test`,`[hooks.verification].test_command` 可配),失败否决
  本轮结束并把输出回注。
- **test_runner**(PostToolUse hook):检测 bash 输出中的测试失败特征,
  注入 system-reminder 提醒模型修复。

## 形态:门控型(默认关,双路径)

按「形态判断铁律」,这两个 hook 是领域特定能力(在宿主 cwd 跑 shell),
原本就「config 显式列出才启用」;插件化后**默认仍关**,开启走双路径
(任一等价):

| 路径 | 写法 |
|---|---|
| config(原有,不变) | config `[hooks] enabled = ["verification", "test_runner"]` |
| 插件(新增) | `zhuye plugin install plugins/verify-hooks --scope user && zhuye plugin enable verify-hooks@inline` |

插件路径对 serve / SDK 嵌入场景的 explicit-only 收紧同样生效(注入
发生在收紧之后);hook 参数仍走 `[hooks.verification]` /
`[hooks.test_runner]` 段,插件只决定「是否启用」。

## 关闭

`zhuye plugin disable verify-hooks@inline`(config 路径则从 `[hooks]
enabled` 移除对应名)。
