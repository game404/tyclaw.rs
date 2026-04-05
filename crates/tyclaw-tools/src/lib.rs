//! 工具注册与执行层。
//!
//! 本 crate 定义了工具系统的核心抽象：
//! - `Tool` trait：所有工具必须实现的接口
//! - `ToolRegistry`：工具的注册、查找和执行管理器
//! - 内置工具实现：文件系统操作（读/写/编辑/列目录）和 Shell 命令执行

/// 工具基础模块 —— Tool trait、RiskLevel、参数类型转换和验证
pub mod base;

/// 工具注册表模块 —— 管理工具的注册、查找和执行
pub mod registry;

/// 工具执行器模块 —— 将直接执行和 sandbox 路由解耦
pub mod executor;

/// 文件系统工具 —— 读文件、写文件、编辑文件、列目录
pub mod filesystem;

/// Shell 命令执行工具 —— 带安全防护的命令执行
pub mod shell;

/// 交互工具 —— ask_user 允许 Agent 中途向用户提问
pub mod interaction;

/// 轻量文件操作和搜索工具 —— 多模型模式下主控 LLM 使用
pub mod fileops;

/// 定时/延迟任务 —— TimerService 调度引擎 + TimerTool LLM 工具
pub mod timer;

/// Web 工具 —— web_search 搜索 + web_fetch 内容抓取
pub mod web;

// 重新导出核心类型
pub use base::{RiskLevel, Tool};
pub use executor::{
    DirectToolExecutor, FullToolExecutor, SandboxAwareToolExecutor, ToolExecutor, CURRENT_USER_ROLE,
};
pub use fileops::{CopyFileTool, GlobTool, GrepSearchTool, MkdirTool, MoveFileTool};
pub use filesystem::{
    ApplyPatchTool, DeleteFileTool, EditFileTool, ListDirTool, PendingFileStore, ReadFileTool,
    SendFileTool, WriteFileTool, CURRENT_REQUEST_ID,
};
pub use interaction::AskUserTool;
pub use registry::ToolRegistry;
pub use shell::ExecTool;
pub use timer::TimerTool;
pub use tyclaw_tool_abi::{
    AllowAllGate, GatePolicy, PathMount, Sandbox, SandboxDirEntry, SandboxExecResult, SandboxPool,
    ToolDefinitionProvider, ToolParams, ToolRuntime,
};
pub use web::{WebFetchTool, WebSearchConfig, WebSearchTool};
