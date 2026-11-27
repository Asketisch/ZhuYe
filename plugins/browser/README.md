# browser —— 受控浏览器插件(v0.2,agent-browser 后端)

Agent 可控的浏览器能力:MCP server(`zhuye-browser-server` **shim**)确保
本机浏览器实例后,把页面能力整体交给 **agent-browser**(源码以
`third_party/agent-browser` submodule 钉住,Apache-2.0)—— shim 经
`AGENT_BROWSER_CDP` attach 本机 Chrome/Edge/Chromium(仅回环 + 隔离
profile),再以 stdio 行代理透传上游 MCP 工具面。

## 架构

```
宿主(zhuye-mcp 客户端,工具名 mcp__plugin:browser:*)
  └─ stdio: bin/zhuye-browser-server(shim)
       ├─ ① 确保浏览器:读 $ZHUYE_HOME/browser-profile/DevToolsActivePort
       │    attach 已运行实例(与桌面 browser_backend 跨仓共享契约:
       │    谁先启动谁持有);无实例则启动 headless 新实例(隔离 profile)
       ├─ ② 定位上游:ZHUYE_AGENT_BROWSER → bin/agent-browser(随包捆绑)
       │    → PATH(brew/npm 安装兜底)
       └─ ③ spawn `agent-browser mcp` + AGENT_BROWSER_CDP=<ws 端点> +
            AGENT_BROWSER_SESSION=zhuye-p<pid>-t<secs>(独立 session 隔离
            daemon 生命周期),stdio 双向行代理
```

- **不下载浏览器**:上游对 cdp 端点连接失败只报错、不回退自启/下载
  Chrome for Testing(已对照上游 v0.45.0 源码确认);被控端只用本机浏览器。
- **独立 session**:上游 daemon 按 session 复用且身份不含 cdp 端点,shim
  每次启动用全新 session(`zhuye-` 前缀,不碰用户自用会话),保证 attach
  端点必然与本次拉起的浏览器一致。

## 工具面(上游定义,共 30+ 个)

常用:`agent_browser_open` / `agent_browser_snapshot`(可及性快照,
元素带 `[ref=eN]` 引用)/ `agent_browser_click` / `agent_browser_fill` /
`agent_browser_press` / `agent_browser_scroll` / `agent_browser_screenshot` /
`agent_browser_close`。

进阶(原手写 CDP 版没有的能力):多标签(`agent_browser_tab_*`)、
等待语义(`agent_browser_wait_for_selector` 等)、网络与 cookie/storage、
iframe、文件上传、PDF、JS 求值等 —— 以 `tools/list` 实时返回为准。

## 安装

```bash
./scripts/build_browser_plugin.sh    # shim 必建;submodule 已初始化则捆绑上游
zhuye plugin install plugins/browser --scope user
zhuye plugin enable browser@inline
```

上游二进制获取途径(任选其一,构建脚本自动探测):

1. **随包捆绑**(推荐):`git submodule update --init --depth 1
   third_party/agent-browser` 后运行构建脚本,`bin/agent-browser` 随插件分发;
2. PATH 安装:`brew install agent-browser` 或 `npm i -g agent-browser`;
3. `ZHUYE_AGENT_BROWSER=<路径>` 显式指定。

## 安全边界

- **仅回环 + 隔离 profile**:被控端以 `$ZHUYE_HOME/browser-profile` 启动
  (`--remote-debugging-address=127.0.0.1 --disable-extensions`),绝不触碰
  用户主 profile;shim 只把回环 ws 端点交给上游。
- **权限 floor**:MCP 工具天然 Prompt 级(`FloorEnforcingTool`),每次
  调用走宿主审批。
- **上游 opt-in 策略**:agent-browser 自带 `--allowed-domains`(域名白名单,
  含子资源/WebSocket 拦截)、action policy(default-deny)、content
  boundaries、`--max-output` 等 —— 默认不限制,如需收紧可在 shim 拉起的
  上游 env(`AGENT_BROWSER_ALLOWED_DOMAINS` 等)注入,或在宿主审批层兜底。
- **降级不 panic**:上游缺失 / 浏览器不可用时,shim 自答 MCP,工具调用
  统一 `isError` + 可操作修复提示(三种获取路径全部列明)。
- **无浏览器下载面**:上游以 attach 模式接入,任何情况下不会触发
  Chrome for Testing 下载。

## 发现契约

profile 目录下的 `DevToolsActivePort` 文件(首行端口、次行 browser ws 路径)
是桌面 browser_backend 与本插件共享的发现契约:谁先启动浏览器谁持有,
后到方 attach 同一实例。可用 `ZHUYE_BROWSER_EXECUTABLE` 覆盖可执行文件路径。

## 升级上游

```bash
cd third_party/agent-browser
git fetch --depth 1 origin tag v<新版本> && git checkout v<新版本>
cd ../.. && git add third_party/agent-browser   # bump 指针
./scripts/build_browser_plugin.sh               # 重建捆绑二进制
```

版本钉住策略与分发许可说明见 [NOTICE.md](NOTICE.md)。
