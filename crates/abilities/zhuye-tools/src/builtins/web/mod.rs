//! 网络取内容工具。
//!
//! - `web_fetch`：抓取网页正文。
//! - `web_search`：多后端搜索。
//! - `url_safety`：URL 清洗与 SSRF 判定（供前两者使用）。

pub mod url_safety;
pub mod web_fetch;
pub mod web_search;
