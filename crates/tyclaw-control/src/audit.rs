//! 全局审计日志系统 —— 按天分文件，JSON Lines 格式。
//!
//! 所有 workspace 的审计记录写入同一个目录，按日期分文件：
//! `{audit_dir}/2026-04-04.jsonl`
//! 每条记录包含 workspace_key 和 session_id，便于按维度查询。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 审计日志条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: DateTime<Utc>,
    pub workspace_key: String,
    pub session_id: String,
    pub user_id: String,
    #[serde(default)]
    pub user_name: String,
    pub channel: String,
    pub request: String,
    pub tool_calls: Vec<serde_json::Value>,
    /// 本次请求中调用的 skill 列表（从 exec 命令中自动提取）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills_used: Vec<serde_json::Value>,
    pub final_response: Option<String>,
    pub total_duration: Option<f64>,
    pub token_usage: Option<serde_json::Value>,
}

/// 全局审计日志管理器 —— 按天分文件追加写入。
///
/// 存储结构：`{audit_dir}/YYYY-MM-DD.jsonl`
pub struct AuditLog {
    audit_dir: PathBuf,
}

impl AuditLog {
    pub fn new(audit_dir: impl AsRef<Path>) -> Self {
        Self {
            audit_dir: audit_dir.as_ref().to_path_buf(),
        }
    }

    /// 当天的审计日志文件路径。
    fn today_file(&self) -> PathBuf {
        let date = Utc::now().format("%Y-%m-%d").to_string();
        self.audit_dir.join(format!("{date}.jsonl"))
    }

    /// 指定日期的审计日志文件路径。
    fn date_file(&self, date: &str) -> PathBuf {
        self.audit_dir.join(format!("{date}.jsonl"))
    }

    /// 追加一条审计日志。
    pub fn log(&self, entry: &AuditEntry) -> Result<(), std::io::Error> {
        let path = self.today_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// 查询审计日志。
    ///
    /// 支持按 workspace_key、user_id 过滤，限制返回条数。
    /// `date` 指定查询哪天的日志（格式 "YYYY-MM-DD"），None 查当天。
    pub fn query(
        &self,
        date: Option<&str>,
        workspace_key: Option<&str>,
        user_id: Option<&str>,
        limit: usize,
    ) -> Vec<AuditEntry> {
        let path = match date {
            Some(d) => self.date_file(d),
            None => self.today_file(),
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let mut entries: Vec<AuditEntry> = content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|e: &AuditEntry| {
                workspace_key.map_or(true, |wk| e.workspace_key == wk)
                    && user_id.map_or(true, |uid| e.user_id == uid)
            })
            .collect();

        entries.reverse();
        entries.truncate(limit);
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entry(workspace_key: &str, user_id: &str) -> AuditEntry {
        AuditEntry {
            timestamp: Utc::now(),
            workspace_key: workspace_key.into(),
            session_id: "s_test_001".into(),
            user_id: user_id.into(),
            user_name: "test_user".into(),
            channel: "cli".into(),
            request: "test request".into(),
            tool_calls: vec![],
            skills_used: vec![],
            final_response: Some("done".into()),
            total_duration: Some(1.5),
            token_usage: None,
        }
    }

    #[test]
    fn test_log_and_query() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        log.log(&make_entry("alice", "user_a")).unwrap();
        log.log(&make_entry("alice", "user_b")).unwrap();
        log.log(&make_entry("bob", "user_a")).unwrap();

        // 查全部
        let all = log.query(None, None, None, 100);
        assert_eq!(all.len(), 3);

        // 按 workspace_key 过滤
        let alice = log.query(None, Some("alice"), None, 100);
        assert_eq!(alice.len(), 2);

        // 按 user_id 过滤
        let user_a = log.query(None, None, Some("user_a"), 100);
        assert_eq!(user_a.len(), 2);

        // 组合过滤
        let alice_a = log.query(None, Some("alice"), Some("user_a"), 100);
        assert_eq!(alice_a.len(), 1);
    }

    #[test]
    fn test_query_empty() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        let result = log.query(None, None, None, 100);
        assert!(result.is_empty());
    }
}
