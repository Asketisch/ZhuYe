# 一轮对话里发生了什么

> 本文讲烛夜处理一次用户输入的运行原理。更底层的实现——四节点
> StateGraph、节点职责、门面映射、恢复策略——见
> [architecture.md](architecture.md)。

你给一句话，引擎把它和当前的工具清单一起发给模型。模型要么回话，要么要求调用某个工具；引擎执行工具、把结果送回模型，循环继续。模型不再要工具的时候，这轮就结束了。

中间没有黑魔法。整个循环就是 `PreLoop → ModelCall → ToolExec → PreLoop → CheckStop`，每一步都往外发事件，你的界面照着事件画就行。

事件走 stdout，日志走 stderr（`zhuye exec | jq` 不被日志污染）。

模型不再发工具调用时经 CheckStop 结束 turn——Stop hook 可以否决、强制续跑。事件以 JSONL 流写出，客户端（TUI / exec / SDK）统一通过同一套 Submission / Event 协议通信。
