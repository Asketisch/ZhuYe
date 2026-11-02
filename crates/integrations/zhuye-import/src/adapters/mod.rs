//! 各 Harness 的具体解析器(每家一个模块,与 `zhuye-llm` 的
//! per-provider 组织方式同构)。

pub mod claude_code;
pub mod codex;
// DeepSeek 等后续来源在此追加模块,并在 `adapter::adapter_for` 注册。
pub mod zcode;
