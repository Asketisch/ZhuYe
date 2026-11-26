//! 人机交互工具 —— 向用户提问并等待应答。
//!
//! - `ask_user` / `ask_user_question`：回合内提问。
//! - `request_human_input`：跨进程挂起等待人工输入。
//! - `brief`：给出一份简报。

pub mod ask_user;
pub mod ask_user_question;
pub mod brief;
pub mod request_human_input;
