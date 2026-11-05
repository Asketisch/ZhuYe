# computer-use —— 屏幕与输入插件

xcap(截屏)+ enigo(鼠标键盘)。MCP 工具天然 `Prompt` 权限下限,
每次调用弹宿主审批。

## 安装(三步)

```bash
./scripts/build_computer_use_plugin.sh             # 默认 --features real
zhuye plugin install plugins/computer-use --scope user
zhuye plugin enable computer-use@inline
```

## 权限要求

- macOS:系统设置 → 隐私与安全 → **屏幕录制**(截图)+ **辅助功能**(键鼠)。
- 未授权 / headless / 未开 `real` feature 构建 → 工具统一返回
  `Unavailable`(isError 文案,不 panic)。

## MCP 工具(全名 `mcp__plugin:computer-use@inline:computer__<tool>`)

| 工具 | 说明 |
|---|---|
| `computer_screenshot` | 截主屏并返回 PNG 图片 |
| `computer_click` | 绝对坐标点击(button 可选) |
| `computer_type` | 键入文本 |
| `computer_key` | 组合键(`ctrl+c`) |
| `computer_scroll` | 滚轮(dx/dy) |

## slash 命令

`/computer <动作描述>` —— 见 `commands/computer.md`。
