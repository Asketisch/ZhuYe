# browser —— 受控浏览器插件(F39)

Agent 可控的浏览器能力:MCP server 经 **CDP(Chrome DevTools Protocol)** 驱动
本机 Chrome/Edge/Chromium,仅 loopback + 隔离 profile,带 F39 元素寻址
(`@eN` 引用 + 快照绑定,旧 ref 显式失效)。

## 工具

| 工具 | 作用 |
|---|---|
| `browser_navigate` | 导航到 URL(等 load 完成) |
| `browser_snapshot` | 可及性快照 → `@eN` 引用树形文本(首行带版本号) |
| `browser_click` | 点击元素(引用 + 快照版本) |
| `browser_type` | 填写输入框(聚焦 → 全选 → 覆盖) |
| `browser_press_key` | 组合键(`ctrl+a` / `enter`) |
| `browser_scroll` | 滚轮(正 dy = 下) |
| `browser_screenshot` | 页面截图(JPEG) |
| `browser_close` | 关闭受控标签页 |

## 安装

```bash
./scripts/build_browser_plugin.sh
zhuye plugin install plugins/browser --scope user
zhuye plugin enable browser@inline
```

## 安全边界

- **仅回环**:CDP 端点只接受 `127.0.0.1` / `localhost` / `::1`
  (`BrowserSecurity.loopback_only` 强制校验)。
- **单 target**:`Target.getTargets` 的 page 数 > 1 拒绝连接(防误操作多标签)。
- **隔离 profile**:`$ZHUYE_HOME/browser-profile`,绝不触碰用户主 profile;
  `--disable-extensions`,不带用户书签/同步。
- **权限 floor**:MCP 工具天然 Prompt 级(`FloorEnforcingTool`),宿主审批。
- `require_token` v1 不启用(原生 CDP 无鉴权机制;token 代理为后续项)。

## 发现契约

profile 目录下的 `DevToolsActivePort` 文件(首行端口、次行 browser ws 路径)
是桌面 browser_backend 与本插件共享的发现契约:谁先启动浏览器谁持有,
后到方 attach 同一实例。可用 `ZHUYE_BROWSER_EXECUTABLE` 覆盖可执行文件路径。
