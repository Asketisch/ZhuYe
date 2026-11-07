# goal-queue 插件

目标模式 + 队列模式的交付插件(v1.10 功能2)。

## 功能

- **goal(目标模式)**:`goal` 工具(`enter` / `exit` / `status`)。
  经 submission 通道提交 `Op::EnterGoalMode` / `Op::ExitGoalMode`,
  复用引擎全部语义:独立 LLM 每轮自校验、可选 verify 命令 AND 条件、
  3-strike 阻塞判定、token 预算。
- **queue(队列模式)**:`queue` 工具(`add` / `list` / `remove` /
  `clear` / `start` / `pause` / `status`)。FIFO 任务队列,driver 串行
  消费:前一条任务的 turn 结束(TurnComplete)后自动把下一条作为新
  turn 注入。持久化 `~/.zhuye/queue/<session>.json`,重启后未完成任务
  自动重新排队。

## 安装 / 启用

```bash
zhuye plugin install plugins/goal-queue --scope user
zhuye plugin enable goal-queue@inline
```

启用后 headless(`zhuye exec`)/ serve / SDK 会话即注册 `goal` / `queue`
工具并启动队列 driver;slash 命令 `/goal-queue:goal` / `/goal-queue:queue`
引导模型使用这两个工具。

## 设计说明

- goal 的引擎路径(TUI 的 `/goal` slash → Op)不受插件影响;插件是给
  headless / SDK 侧的模型补上工具入口。
- 队列完成信号是事件驱动的:driver 订阅 thread 事件流,收到
  `TurnComplete` 时结清当前 active 任务(`Success` → done,否则 failed
  带 note)并立即尝试出队下一条。
