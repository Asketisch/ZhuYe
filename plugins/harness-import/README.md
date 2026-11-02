# harness-import —— 外部 Harness 历史会话导入插件

把 Claude Code / Codex / ZCode 的历史对话导入为烛夜原生会话
(`~/.zhuye/sessions/YYYY/MM/DD/<thread_id>.jsonl`),导入产物即是普通
会话:`zhuye session ls` 可见、`zhuye exec --resume <id>` 可续跑、
Desktop 按工作区归属展示。

能力核心是独立 crate `crates/integrations/zhuye-import`;本插件只做
**打包与接入**:一个双模式二进制 + 一条 slash 命令。

## 安装

```bash
# 1. 构建并装配二进制到插件 bin/(bin/ 内容不入库)
./scripts/build_import_plugin.sh

# 2. 安装 + 启用
zhuye plugin install plugins/harness-import --scope user
zhuye plugin enable harness-import@inline

# 3. 重启 zhuye exec / serve(或等热重载),即可在对话里使用
```

## 使用方式

| 方式 | 入口 | 说明 |
|---|---|---|
| 对话内(CLI / Desktop) | `/harness:import <路径>` | 命令展开 → 模型调用 MCP 工具,一次审批 |
| 对话内(直接说) | 「帮我把 Codex 会话导进来」 | 模型自行调用 `import_session` / `list_importable` |
| 终端直跑(批量) | `bin/zhuye-import <路径> --from auto` | 不经 LLM,适合脚本与桌面端导入向导 |

MCP 工具:

- `import_session(path, source_kind?, dry_run?, include_thinking?, force?)`
  —— 导入文件或目录(目录递归收集 `.jsonl` 逐个嗅探);
- `list_importable(source_kind)` —— 扫描该 Harness 本机默认安装位置。

直跑模式示例:

```bash
bin/zhuye-import ~/.codex/sessions --all --from codex --dry-run  # 预览
bin/zhuye-import ~/.claude/projects/*/*.jsonl                    # 自动嗅探导入
bin/zhuye-import --from zcode ...                                # ZCode 见下
```

## 保真边界

- 保留:turn 结构、可见文本、工具调用与结果、token 用量、压缩摘要、
  工作区归属;源 id 是合法 UUID 时原样保留(重复导入天然幂等)。
- 默认丢弃:思考链(`include_thinking` / `--include-thinking` 保留)、
  Harness 注入的环境脚手架(`keep_injected` / `--keep-injected` 保留;
  保留时**前置到紧随的真实用户输入所在轮**,不独立成轮)、
  Claude Code 子代理轨迹(`--include-sidechain` 保留)。
- ZCode:历史存于 `~/.zcode/cli/db/db.sqlite`,导入时先拷贝到临时
  目录只读解析,不写源库;MCP 工具入口不暴露 SQLite 路径,批量导入
  ZCode 请用直跑模式的 `list_importable` + 单会话导入流程。

## 已知限制

- DeepSeek Harness 暂未支持(架构已留 adapter 位,见
  `zhuye-import/src/adapter.rs`)。
- Codex 加密 reasoning 无法解密,记 warning 跳过。
