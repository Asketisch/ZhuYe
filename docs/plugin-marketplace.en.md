# ZhuYe Plugin Marketplace

<p align="center">  <a href="plugin-marketplace.md">中文</a> · <b>English</b>  </p>

> A marketplace is a **collection of plugins**: one JSON manifest
> listing multiple plugin entries, each entry telling users where to pull
> from. Both the publisher-side format and the user-side flow are here.
> For how to write plugins themselves see
> [`plugin-dev-guide.md`](plugin-dev-guide.md) (hands-on tutorial); for
> the install / enable perspective see [`plugins.md`](plugins.md).

## 1. manifest format (publisher-side)

If you are publishing a marketplace, put
`.claude-plugin/marketplace.json` (JSON, not TOML) in your repo:

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

| Field | Required | Description |
|---|---|---|
| `name` | yes | marketplace name; must not impersonate `inline` / `builtin` |
| `owner` | yes | `PluginAuthor`, `name` required |
| `plugins` | yes | array of entries |
| `force_remove_deleted_plugins` | no (default `false`) | defined but **not consumed at runtime**; nothing auto-uninstalls today, do not rely on it |

Each entry (`PluginMarketplaceEntry`):

- `#[serde(flatten)] manifest` — **the full manifest fields are flattened
  into the entry**, and the entry can override fields from the
  plugin's own `plugin.json`;
- `source` — **required**, the marketplace's sole job: telling users
  where to pull the plugin from;
- `category` / `tags` — for searching;
- `strict` (default `true`) — `true` = the plugin directory must carry
  its own manifest; `false` = the manifest flattened in the entry is
  the complete definition.

## 2. source types

`source` variants (`#[serde(tag = "source", rename_all = "kebab-case")]`):

| `source` value | Extra fields | Fetch method |
|---|---|---|
| `github` | `repo` (owner/name required), `ref?`, `sha?` | converted to a git shallow clone |
| `git` | `url` (required), `ref?`, `sha?`, `path?` (subdirectory) | `git clone --depth 1` |
| `url` | `url` (required), `headers?` | HTTP GET, the response body is marketplace.json |
| `file` | `path` (required) | local `.json` file |
| `directory` | `path` (required) | local directory, zero IO, only verifies the manifest exists |

## 3. User side: registering, installing, refreshing

```bash
# Register a marketplace source and fetch its manifest
zhuye plugin marketplace add my-tools --from github --repo me/zhuye-plugins
zhuye plugin marketplace add my-tools --from git    --url https://…  --ref v1
zhuye plugin marketplace add my-tools --from url    --url https://…/marketplace.json
zhuye plugin marketplace add my-tools --from file   --path ./marketplace.json
zhuye plugin marketplace add my-tools --from directory --path ./plugins

zhuye plugin marketplace ls                        # list known marketplaces
zhuye plugin marketplace refresh --name my-tools   # incremental refresh
zhuye plugin marketplace refresh                   # refresh all with auto_update = true
zhuye plugin marketplace discover --topic devtools # discover by topic (with --limit)
zhuye plugin marketplace remove my-tools           # remove (really deletes the cache directory)
```

Flow:

1. `marketplace add` fetches and caches `.claude-plugin/marketplace.json`;
2. `zhuye plugin install <name>@<mkt>` finds the entry in the cache,
   pulls the plugin body per the entry's `source`, and **installs the
   dependency closure in DFS post-order** (dependencies first, the root
   plugin last);
3. a single failed dependency is only recorded in the report's failed
   block, never blocking the whole tree; an already-installed one counts
   as an idempotent success;
4. `refresh [--name]` updates incrementally (overwriting the known
   cache); without `--name` only `auto_update = true` ones refresh.

Local paths as dependency sources only support the `file` / `directory`
source kinds; dependencies of other sources must be fetched
separately first.

Registered marketplaces can also be written into
`[plugins.marketplaces.<name>]` in `~/.zhuye/config.toml` — but that
section **has no runtime consumption point today**; the actual
marketplace state is maintained by the CLI reading/writing
`~/.zhuye/plugins/marketplaces.json`, and `zhuye plugin marketplace ...`
is the source of truth. See the state-model section in
[`plugins.md`](plugins.md) for details.

## 5. ⚠️ Dangerous behavior: the directory source

When registering with `--from directory --path <dir>`, the directory is
**not copied**; `<dir>` itself is directly recorded as the install
location. Running `zhuye plugin marketplace remove` makes the CLI run
`remove_dir_all` on that path (code reference:
`crates/runtime/zhuye-cli/src/plugin/marketplace.rs`) — **your provided
source directory, along with everything in it, gets deleted**. For local
directory experiments use the `file` source (single JSON) or `cp -r` a
copy first and register the `directory` source on the copy.

## 4. Errors

See the [`plugins.md` error quick reference table](plugins.md#排障) for
`PluginError` variants related to fetching and parsing.
