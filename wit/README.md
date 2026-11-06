# wit/ — 烛夜 WASM 插件契约(社区接口权威定义)

`plugin.wit` 定义了宿主与 WASM 插件组件之间的语言无关接口。任何能产出
**组件模型 .wasm** 的工具链都可实现:

| 语言 | 工具链 | 成熟度 |
|---|---|---|
| Rust | wit-bindgen(`wasm32-wasip2` target) | 生产可用 |
| TypeScript / JS | jco(componentize-js) | 生产可用 |
| Python | componentize-py | 可用(工具链更新频繁,锁版本) |
| Go | tinygo + wasm-tools | 走在前(实验性) |

ABI 刻意选 **JSON 字符串进出**(而非细粒度 WIT record):动态语言作者
无需生成强类型绑定即可参与;schema 由 `plugin.wit` 注释与
`docs/plugins.md` 定义。

## 版本策略

- 包名 `zhuye:plugin`,语义化版本。**`@0.x` 期间允许 additive 演进**
  (新增 world / 新增可选导出);**破坏性变更升主版本**并保持旧世界
  文件共存(如 `plugin@0.1.0.wit` 与 `plugin@1.0.0.wit` 并存)。
- 宿主按包名 + 版本实例化组件:未知主版本 → 显式报错(不 panic、
  不静默降级);未知次版本(additive)→ 按已知导出使用,缺失导出
  报错。
- 组件导入(WIT `import`)当前为空 —— 插件是纯计算沙箱,无宿主
  能力可调用;未来若开放(如受控 FS 读),以新 world 或显式 import
  演进,并同步本文档。

## 超时与安全语义

- 每次导出调用有宿主施加的 deadline(epoch 中断);超时 = 立即截断。
- hook 决策:超时 / `Err` / 非法 JSON / 缺 `action` 一律 **Deny**
  (fail-closed,与 shell hook 同语义)。
- tool:超时 / `Err` → 工具错误(`is_error = true`),不 panic。
- 权限:WASM 工具走既有外部工具 floor = `Prompt` —— 组件声明 `Auto`
  不会生效(与 MCP 工具同基线)。
- 沙箱边界:组件默认无文件系统、无网络、无环境变量、无时钟以外的
  host 能力。

## 构建一个组件(以 Rust 为例)

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2   # 独立项目,不进主 workspace
# 产物: target/wasm32-wasip2/release/<name>.wasm → 拷进插件 bin/
```

`zhuye plugin new <name> --lang rust|typescript` 可生成完整脚手架
(见 `docs/plugins.md` 的「WASM 插件(多语言)」章节)。
