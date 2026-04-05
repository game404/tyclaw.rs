//! Web 工具模块 —— web_search 搜索引擎 + web_fetch URL 内容抓取。

pub mod fetch;
pub mod html;
pub mod search;
pub mod security;

pub use fetch::WebFetchTool;
pub use search::{WebSearchConfig, WebSearchTool};
