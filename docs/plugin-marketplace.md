# 烛夜插件市场(Marketplace)

> marketplace 是一个**插件集合**:一个 JSON manifest 列出多个插件条目,
> 每个条目告诉用户去哪儿拉。发布侧格式与使用侧流程都在本文。
> 插件本身怎么写见 [`plugin-dev-guide.md`](plugin-dev-guide.md)（上手教程）与
> [`plugins.md`](plugins.md)（字段级参考）。

## 1. manifest 格式(发布者向)

如果你要发布一个 marketplace,在仓库里放
`.claude-plugin/marketplace.json`(JSON,不是 TOML):

```json
{
  "name": "my-tools",
  "owner": { "name": "me", "url": "https://example.com" },
  "plugins": [
    {
      "name": "formatter",
      "description": "代码格式化插件",
      "source": { "source": "github", "repo": "me/formatter", "ref": "v1" },
      "category": "devtools",
      "tags": ["format"],
      "strict": true
    }
  ]
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | 是 | marketplace 名,不能仿冒 `inline` / `builtin` |
| `owner` | 是 | `PluginAuthor`,`name` 必填 |
| `plugins` | 是 | entry 数组 |
| `force_remove_deleted_plugins` | 否(默认 `false`) | marketplace 里移除的插件自动卸载 |

每个 entry(`PluginMarketplaceEntry`):

- `#[serde(flatten)] manifest` —— **完整 manifest 字段平铺进 entry**,
  entry 可覆盖插件自带 `plugin.json` 的字段;
- `source` —— **必填**,marketplace 的唯一职责:告诉用户去哪儿拉插件;
- `category` / `tags` —— 检索用;
- `strict`(默认 `true`)—— `true` = 插件目录必须自带 manifest;
  `false` = entry 平铺的 manifest 即完整定义。

## 2. source 类型

`source` 的 variant(`#[serde(tag = "source", rename_all = "kebab-case")]`):

| `source` 值 | 附加字段 | 拉取方式 |
|---|---|---|
| `github` | `repo`(owner/name 必填)、`ref?`、`sha?` | 转 git 浅克隆 |
| `git` | `url`(必填)、`ref?`、`sha?`、`path?`(子目录) | `git clone --depth 1` |
| `url` | `url`(必填)、`headers?` | HTTP GET,响应体即 marketplace.json |
| `file` | `path`(必填) | 本地 `.json` 文件 |
| `directory` | `path`(必填) | 本地目录,零 IO,只验证 manifest 存在 |

## 3. 使用侧:注册、安装与刷新

```bash
# 注册 marketplace 源并拉取 manifest
zhuye plugin marketplace add my-tools --from github --repo me/zhuye-plugins
zhuye plugin marketplace add my-tools --from git    --url https://…  --ref v1
zhuye plugin marketplace add my-tools --from url    --url https://…/marketplace.json
zhuye plugin marketplace add my-tools --from file   --path ./marketplace.json
zhuye plugin marketplace add my-tools --from directory --path ./plugins

zhuye plugin marketplace ls                        # 列出已知 marketplace
zhuye plugin marketplace refresh --name my-tools   # 增量刷新
zhuye plugin marketplace refresh                   # 刷全部 auto_update = true 的
zhuye plugin marketplace remove my-tools           # 删除(真删 cache 目录)
```

流程:

1. `marketplace add` 拉取并缓存 `.claude-plugin/marketplace.json`;
2. `zhuye plugin install <name>@<mkt>` 从缓存里找 entry,按 entry 的
   `source` 拉插件本体,**依赖闭包按 DFS 后序安装**(依赖先、根插件后);
3. 单个依赖失败只记入报告的 failed 块,不阻断整树;已装算幂等成功;
4. `refresh [--name]` 增量更新(覆盖已知 cache);不带 `--name` 只刷
   `auto_update = true` 的。

本地路径作为依赖源只支持 `file` / `directory` 两种 source;其他
source 的依赖需先单独 fetch。

已注册的 marketplace 也可以写进 `~/.zhuye/config.toml` 的
`[plugins.marketplaces.<名>]` —— 但该段**当前无运行时消费点**,
marketplace 的实际状态由 CLI 读写 `~/.zhuye/plugins/marketplaces.json`
维护,以 `zhuye plugin marketplace ...` 为准。详见
[`plugins.md`](plugins.md) 的「config.toml 的 `[plugins]` 段」。

## 4. 错误

拉取与解析相关的 `PluginError` 变体见
[`plugins.md` 错误速查表](plugins.md#排障)。
