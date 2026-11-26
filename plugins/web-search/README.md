# web-search 插件(门控型)

`web_search` 工具的插件化交付:通过 Brave Search API 执行 Web 搜索,
结果 URL 经消毒后注入上下文,`Auto` 审批(只读无副作用)。

## 形态:门控型(默认开启)

按「形态判断铁律」,web_search 绑定 Brave 第三方 API,属外部系统接入,
应插件化;但它此前是无条件内置工具,为保外部用法零影响(工具名 /
审批 / 默认可用性不变),采用**门控型**(先例 `goal-queue`):工具本体
留在引擎 crate(`zhuye-tools`),注册与否由本插件启用状态决定。

- **默认开启**(`BUNDLED_PLUGIN_DEFAULTS`):空配置下行为与插件化之前
  完全一致。
- **API key**:沿用 `[web_search]` 配置段或 `BRAVE_API_KEY` 环境变量,
  语义不变。

## 关闭方式(两个入口,粒度不同)

| 入口 | 写法 | 效果 |
|---|---|---|
| 插件级(注册门控) | `zhuye plugin disable web-search@inline`,或 config `[plugins].disabled_plugins = ["web-search@inline"]` | 工具**不进注册表**:tools 面消失、`tool_search` 搜不到、调用报 tool not found |
| 工具名级(可见性过滤) | config `[active] disallowed_tools = ["web_search"]` | 工具仍注册,仅 **LLM 不可见**(三层过滤模型的第二层) |

三层叠加顺序:注册门控 → 可见性过滤(`[active] tools` / `disallowed_tools`)→
执行期审批(权限规则 / floor)。

## 安装 / 启用

门控判定不依赖安装;安装是为了 `plugin enable` 管理与 `/web-search`
命令挂载:

```bash
zhuye plugin install plugins/web-search --scope user
zhuye plugin enable web-search@inline
```

## 后续演进

`zhuye-tools/src/guards/web_guard.rs` 的多后端 failover 链
(DuckDuckGo 免 key / SearXNG / Brave / Tavily / Custom)已预铺设,
后续作为本插件的能力演进项迁入(多后端聚合 / 免 key 降级)。
