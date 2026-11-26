# zhuye-memory AGENTS.md

> **何时读我**:改动持久记忆(三 scope 存储 / 检索 / 条目编辑 / SQLite 版本库 / 生命周期与画像)时,读本篇。

## 本 crate 是什么

三 scope(project / user / session)持久 memory:Markdown 文件承载 project 与
user scope,session 仅内存;检索走分节 BM25 与混合召回,写侧有确定性整理与
自动抽取。路线图 M4 起步,v1.11 M5a 引入 SQLite 条目版本库作为新模式权威存储。

## 核心文件导航

| 路径 | 一句话职责 |
|---|---|
| `src/lib.rs` | crate 门面:公开 API **全走根级扁平 re-export** |
| `src/model.rs` | `MemoryScope` 三 scope 枚举与 `MemoryError` |
| `src/store.rs` | `MemoryStore` trait + `FileMemoryStore`(Markdown)+ `InMemoryStore`(session)+ `truncate_for_injection` |
| `src/backend.rs` | 后端选择与装配:`ZHUYE_MEMORY_BACKEND=sqlite` 显式开新模式,默认保持 Markdown 旧行为 |
| `src/edit.rs`(+`src/edit/tests.rs`) | 条目级编辑:`## <key>` 标题块增 / 删 / 整份替换,乐观锁 + 歧义检测 |
| `src/sqlite/` | SQLite 条目版本库(M5a):`schema`(版本表 + 墓碑 + FTS5)、`change`(事务变更端口)、`project`(MEMORY.md 投影)、`retrieve`(FTS5 检索) |
| `src/retrieval/sections.rs` | 分节检索:MEMORY.md 按 `## 标题` 拆条目,BM25 选相关 + 最近使用(复用 zhuye-bm25) |
| `src/retrieval/hybrid.rs` | F16 混合检索:BM25 + 向量 RRF 融合,三档降级(无向量库 / 无 embedding 也能跑) |
| `src/lifecycle/extract.rs` | F17 会话结束自动提炼事实,管理去重 / 冲突 / 分层 / 衰减 / 驱逐 |
| `src/lifecycle/consolidate.rs` | 确定性整理(同名合并 / 空条目剔除),零 LLM 成本 |
| `src/lifecycle/evolution.rs` | F18 技能自进化:复盘沉淀技能候选(review 门)+ 错误-解法缓存 |
| `src/insight/persona.rs` | F19 用户画像(偏好默认 pending 待审批)+ Persona / SOUL 注入 |
| `src/insight/longtail.rs` | F59 记忆长尾:时态边 / 证据接地 / 命名槽 / MemFS 导出 |
| `src/insight/topics.rs` | 多文件记忆目录:`topics/<name>.md` 承载长主题,MEMORY.md 保持精炼索引 |
| `src/storage/editor.rs` | `MemoryEditor` 写侧端口:所有写入消费方经此 trait,不感知底层是文件还是 SQLite |
| `src/storage/scope.rs` | 三 scope 路径解析(project 落 workspace、user 落 home、session 无文件) |

## 本地契约与同步点

- **外部全走根级扁平 re-export**:公开类型与方法全部 `pub use` 在 `src/lib.rs`
  根(`SqliteMemoryStore` / `HybridRetriever` / `MemoryEditor` / `Persona` 等),
  调用方不得深入 `sqlite::` / `insight::` 子模块路径 —— 子目录属内部布局,
  restructure 时自由重组,深入引用会跟着断。
- **SQLite 是条目权威库,MEMORY.md 是可编辑投影**(`sqlite/project.rs`):首次
  打开幂等导入旧 MEMORY.md,此后导出为投影;**墓碑条目不得复活**(默认查询与
  自动导入都跳过,仅 `revive = true` 显式授权可重建);条目内容更新 = 同事务
  关旧版本 + 追加新版本,不就地覆盖;跨进程并发靠 revision CAS,冲突返回
  `MemoryError::RevisionConflict`,调用方须重读后重新决策。
- **写侧唯一权威写入链是 `MemoryEditor`**(plan §2):`MemoryTool` 等消费方经
  此 trait 操作,**不感知**底层后端;新增写入入口必须收敛到该接口,不得直接写
  MEMORY.md 或绕过 `sqlite::change` 的事务端口。
- **后端切换只换装配不换调用点**(`backend.rs`):新旧两路径共享
  `MemoryStore::retrieve` 检索路由 + `MemoryEditor`,改任一侧接口时另一侧必须
  同步保持形状一致。
- **注入上限 `MAX_MEMORY_INJECT_CHARS` = 8000**(`storage/scope.rs`),与宿主
  注入口径同源;改值要同步调用方。
- **检索打分复用 `zhuye-bm25`**(层契约「检索只此一份」),不要在本 crate 另写
  BM25 排序实现。

## 禁止事项

1. 不要绕过扁平 re-export 从下游 `use zhuye_memory::sqlite::...`。
2. 不要在自动流程里复活墓碑条目或就地覆盖已发布版本。
3. `Session` scope 不要落文件(设计上仅内存,未来经 rollout 持久化是独立决策)。

## 深入阅读

- [crates/abilities/AGENTS.md](../AGENTS.md) —— 本层契约(含记忆存储契约与 BM25 共用)
- [docs/features.md](../../../docs/features.md) —— 跨会话记忆的产品定位
