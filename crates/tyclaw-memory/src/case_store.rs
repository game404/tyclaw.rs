//! 结构化案例记录的持久化存储，按工作区隔离。
//!
//! 每个工作区有独立的 cases.json 文件，存储该工作区内所有已解决的案例。

use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::warn;
use uuid::Uuid;

/// 案例记录结构 —— 记录一次已解决问题的完整信息。
///
/// 字段说明：
/// - `case_id`: 案例唯一标识符（UUID 前12位 或 SHA256 哈希前12位）
/// - `question`: 用户提出的原始问题
/// - `workspace_id`: 所属工作区 ID（多租户隔离）
/// - `timestamp`: 创建时间（RFC 3339 格式）
/// - `category`: 问题分类（如 "build_error"、"config_issue"）
/// - `root_cause`: 根因分析结果
/// - `solution`: 解决方案描述
/// - `evidence`: 证据列表（如相关日志片段）
/// - `modules`: 涉及的模块/组件列表
/// - `tools_used`: 解决过程中使用的工具列表
/// - `duration_seconds`: 解决耗时（秒）
/// - `user_id`: 提问用户 ID
/// - `feedback`: 可选的用户反馈
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseRecord {
    pub case_id: String,
    pub question: String,
    pub workspace_id: String,
    pub timestamp: String,
    pub category: String,
    pub root_cause: String,
    pub solution: String,
    pub evidence: Vec<String>,
    pub modules: Vec<String>,
    pub tools_used: Vec<String>,
    pub duration_seconds: f64,
    pub user_id: String,
    pub feedback: Option<String>,
}

impl CaseRecord {
    /// 创建一个新的案例记录，自动生成 UUID 和时间戳。
    ///
    /// 其他字段初始化为空值，后续通过提取器填充。
    pub fn new(question: impl Into<String>, workspace_id: impl Into<String>) -> Self {
        Self {
            case_id: Uuid::new_v4().to_string()[..12].to_string(), // 取 UUID 前12位作为 ID
            question: question.into(),
            workspace_id: workspace_id.into(),
            timestamp: Local::now().to_rfc3339(), // 当前本地时间
            category: String::new(),
            root_cause: String::new(),
            solution: String::new(),
            evidence: Vec::new(),
            modules: Vec::new(),
            tools_used: Vec::new(),
            duration_seconds: 0.0,
            user_id: String::new(),
            feedback: None,
        }
    }
}

/// JSON 文件案例存储器。
///
/// 两层结构：
/// - 全局共享：`{global_cases_dir}/cases.json`
/// - Workspace 私有：`{workspace_cases_dir}/cases.json`（由调用方提供路径）
pub struct CaseStore {
    /// 全局案例目录（`{root}/cases`）
    global_dir: PathBuf,
}

impl CaseStore {
    /// 创建新的 CaseStore。`global_dir` 是全局案例目录。
    pub fn new(global_dir: impl AsRef<Path>) -> Self {
        let dir = global_dir.as_ref().to_path_buf();
        let _ = fs::create_dir_all(&dir);
        Self { global_dir: dir }
    }

    /// 全局案例文件路径。
    fn global_path(&self) -> PathBuf {
        self.global_dir.join("cases.json")
    }

    /// 从指定路径加载案例记录。
    fn load_from(path: &Path) -> Vec<CaseRecord> {
        if !path.exists() {
            return Vec::new();
        }
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!(path = %path.display(), error = %e, "Failed to parse cases");
                Vec::new()
            }),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to read cases");
                Vec::new()
            }
        }
    }

    /// 将案例写入指定路径。
    fn save_to(path: &Path, records: &[CaseRecord]) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(records) {
            let _ = fs::write(path, text);
        }
    }

    /// 追加一条案例到 workspace 私有存储。
    ///
    /// `workspace_cases_dir` 由调用方通过 WorkspaceManager 提供。
    pub fn save(&self, case: &CaseRecord, workspace_cases_dir: &Path) {
        let path = workspace_cases_dir.join("cases.json");
        let mut records = Self::load_from(&path);
        records.push(case.clone());
        Self::save_to(&path, &records);
    }

    /// 返回所有案例（全局 + workspace 合并），按时间倒序。
    pub fn list_merged(&self, workspace_cases_dir: &Path) -> Vec<CaseRecord> {
        let mut cases = Self::load_from(&self.global_path());
        cases.extend(Self::load_from(&workspace_cases_dir.join("cases.json")));
        cases.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        cases
    }

    /// 全局案例数量。
    pub fn global_count(&self) -> usize {
        Self::load_from(&self.global_path()).len()
    }

    /// 加载固定案例（pinned cases）。
    ///
    /// 合并全局 + workspace 两层 pinned_cases.json。
    pub fn list_pinned(&self, workspace_cases_dir: &Path) -> Vec<CaseRecord> {
        let mut pinned = Vec::new();
        pinned.extend(Self::load_json_file(&self.global_dir.join("pinned_cases.json")));
        pinned.extend(Self::load_json_file(&workspace_cases_dir.join("pinned_cases.json")));
        pinned
    }

    /// 从指定路径加载 JSON 案例文件（容错）。
    fn load_json_file(path: &Path) -> Vec<CaseRecord> {
        if !path.exists() {
            return Vec::new();
        }
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!(path = %path.display(), error = %e, "Failed to parse pinned cases");
                Vec::new()
            }),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to read pinned cases");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// 测试：保存到 workspace 私有后能正确列出
    #[test]
    fn test_save_and_list() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("cases");
        let ws_cases = tmp.path().join("ws1_cases");
        std::fs::create_dir_all(&ws_cases).unwrap();
        let store = CaseStore::new(&global_dir);

        let mut case = CaseRecord::new("Why does build fail?", "ws1");
        case.root_cause = "Missing dependency".into();
        store.save(&case, &ws_cases);

        let cases = store.list_merged(&ws_cases);
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].question, "Why does build fail?");
    }

    /// 测试：全局 + workspace 合并
    #[test]
    fn test_merged() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("cases");
        std::fs::create_dir_all(&global_dir).unwrap();
        let ws_cases = tmp.path().join("ws1_cases");
        std::fs::create_dir_all(&ws_cases).unwrap();

        let store = CaseStore::new(&global_dir);

        // 写一条到 workspace
        store.save(&CaseRecord::new("q1", "ws1"), &ws_cases);

        // 手动写一条到全局
        let global_case = CaseRecord::new("global_q", "global");
        CaseStore::save_to(&global_dir.join("cases.json"), &[global_case]);

        let merged = store.list_merged(&ws_cases);
        assert_eq!(merged.len(), 2);
    }
}
