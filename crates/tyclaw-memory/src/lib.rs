//! 记忆层：案例存储、检索和提取。
//!
//! 本 crate 实现了基于案例的记忆系统，用于：
//! 1. 自动从 Agent 对话中提取已解决的问题案例
//! 2. 持久化存储案例记录（JSON 文件格式）
//! 3. 根据关键词匹配和时间衰减检索相似案例
//!
//! 这使得 Agent 能够从历史经验中学习，为后续类似问题提供参考。

/// 案例存储模块 —— CaseRecord 结构和 JSON 文件持久化
pub mod case_store;

/// 案例提取模块 —— 从问答对中自动识别和提取结构化案例
pub mod extractor;

/// 案例检索模块 —— 基于关键词匹配 + 时间衰减的相似案例搜索
pub mod retriever;

/// 记忆存储模块 —— MEMORY.md + HISTORY.md 双层记忆
pub mod memory_store;

/// 记忆合并模块 —— 基于 token 预算的自动合并策略
pub mod consolidator;

// 重新导出核心类型
pub use case_store::{CaseRecord, CaseStore};
pub use consolidator::MemoryConsolidator;
pub use extractor::{extract_case, looks_like_resolved_issue};
pub use memory_store::{consolidate_with_provider, MemoryStore};
pub use retriever::CaseRetriever;
