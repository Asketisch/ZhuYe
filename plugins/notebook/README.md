# notebook 插件(门控型)

`notebook_edit` 工具的插件化交付:对 Jupyter `.ipynb` 做 cell 级
编辑(replace / insert / delete),与 `read_before_edit` hook 的
`WRITE_TOOLS` 白名单联动(未 read 过的 notebook 会被拦截)。

## 形态:门控型(默认开启)

按「形态判断铁律」,Jupyter cell 编辑属领域特定能力(数据科学场景),
应插件化;为保外部用法零影响(工具名 / hook 联动 / 默认可用性不变),
采用**门控型**(先例 `goal-queue`):工具本体留在引擎 crate
(`zhuye-tools`),注册与否由本插件启用状态决定,**默认开启**。

## 关闭方式(两个入口,粒度不同)

| 入口 | 写法 | 效果 |
|---|---|---|
| 插件级(注册门控) | `zhuye plugin disable notebook@inline`,或 config `[plugins].disabled_plugins = ["notebook@inline"]` | 工具**不进注册表**:`tool_search` 搜不到、调用报 tool not found |
| 工具名级(可见性过滤) | config `[active] disallowed_tools = ["notebook_edit"]` | 工具仍注册,仅 **LLM 不可见** |

三层叠加顺序:注册门控 → 可见性过滤 → 执行期审批。

## 安装 / 启用

门控判定不依赖安装;安装是为了 `plugin enable` 管理与 `/notebook`
命令挂载:

```bash
zhuye plugin install plugins/notebook --scope user
zhuye plugin enable notebook@inline
```
