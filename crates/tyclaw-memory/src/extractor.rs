//! 从 Agent 的问答对话中提取结构化案例记录。
//!
//! 使用正则表达式匹配中英文关键词，自动识别：
//! - 是否为已解决的问题（contains 修复/解决/fixed/resolved 等关键词）
//! - 根因（root cause）
//! - 解决方案（solution）
//! - 涉及的模块

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::case_store::CaseRecord;

// 使用 lazy_static 宏延迟初始化正则表达式，避免重复编译
lazy_static::lazy_static! {
    /// 判断回答是否"看起来像已解决的问题"的正则模式。
    /// 匹配中英文的修复/解决相关关键词。
    static ref RESOLVED_PATTERN: Regex = Regex::new(
        r"(?i)修复|已修|已解决|解决了|fixed|resolved|solution|root\s*cause|原因[是为]|问题出在|问题定位|caused\s+by"
    ).unwrap();

    /// 根因提取的正则模式列表。
    /// 按优先级排列，匹配到第一个就返回。
    /// 包含中英文的多种表达方式：
    /// - "root cause: ..."
    /// - "问题出在..."
    /// - "原因是/为..."
    /// - "caused by..."
    /// - "因为/由于..."
    static ref CAUSE_PATTERNS: Vec<Regex> = vec![
        Regex::new(r"(?i)([^。.\n]*root\s*cause[^。.\n]*)").unwrap(),
        Regex::new(r"([^。.\n]*问题出在[^。.\n]*)").unwrap(),
        Regex::new(r"([^。.\n]*原因[是为][^。.\n]*)").unwrap(),
        Regex::new(r"([^。.\n]*问题定位[^。.\n]*)").unwrap(),
        Regex::new(r"(?i)([^。.\n]*caused\s+by[^。.\n]*)").unwrap(),
        Regex::new(r"([^。.\n]*因为[^。.\n]*)").unwrap(),
        Regex::new(r"([^。.\n]*由于[^。.\n]*)").unwrap(),
    ];

    /// 解决方案提取的正则模式列表。
    /// 匹配中英文的解决方案描述。
    static ref SOLUTION_PATTERNS: Vec<Regex> = vec![
        Regex::new(r"([^。.\n]*修复方法[是为：:\s][^。.\n]*)").unwrap(),
        Regex::new(r"([^。.\n]*解决方[案法][^。.\n]*)").unwrap(),
        Regex::new(r"(?i)([^。.\n]*(?:Solution|Fix)[：:\s][^。.\n]*)").unwrap(),
        Regex::new(r"([^。.\n]*建议[^。.\n]*)").unwrap(),
    ];

    /// 模块名称提取的正则模式。
    /// 匹配 "模块：xxx"、"module: xxx" 等格式。
    static ref MODULE_PATTERNS: Vec<Regex> = vec![
        Regex::new(r"(?i)(?:模块|module|service|组件)[：:\s]*([^\n。.]+)").unwrap(),
    ];

    /// 分割模块名称的正则（逗号、顿号、斜杠、空格）。
    static ref SPLIT_RE: Regex = Regex::new(r"[,、/\s]+").unwrap();
}

/// 从文本中提取匹配到的第一个句子。
///
/// 遍历所有模式，返回第一个匹配到的文本片段（去除末尾句号）。
/// 如果没有匹配到任何模式，返回空字符串。
fn extract_sentence(text: &str, patterns: &[Regex]) -> String {
    for pattern in patterns {
        if let Some(m) = pattern.find(text) {
            let s = m.as_str().trim();
            return s.trim_end_matches(&['。', '.'][..]).to_string();
        }
    }
    String::new()
}

/// 从文本中提取涉及的模块名称列表。
///
/// 查找类似 "模块：auth, user-service" 的模式，
/// 按逗号/空格等分隔符拆分为独立的模块名。
fn extract_modules(text: &str) -> Vec<String> {
    for pattern in MODULE_PATTERNS.iter() {
        if let Some(caps) = pattern.captures(text) {
            if let Some(raw) = caps.get(1) {
                return SPLIT_RE
                    .split(raw.as_str().trim())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
            }
        }
    }
    Vec::new()
}

/// 生成确定性的案例 ID。
///
/// 基于问题和回答的内容计算 SHA256 哈希值，取前6个字节（12个十六进制字符）。
/// 相同的问答对始终生成相同的 ID，用于去重。
fn deterministic_id(question: &str, answer: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{question}\n{answer}").as_bytes());
    let hash = hasher.finalize();
    hex::encode(&hash[..6]) // 12 个十六进制字符
}

/// 判断回答是否看起来像已解决的故障排查案例。
///
/// 通过检测回答中是否包含修复/解决相关的中英文关键词来判断。
/// 空回答直接返回 false。
pub fn looks_like_resolved_issue(_question: &str, answer: &str) -> bool {
    if answer.is_empty() {
        return false;
    }
    RESOLVED_PATTERN.is_match(answer)
}

/// 尝试从问答对中提取案例记录。
///
/// 工作流程：
/// 1. 先判断回答是否像已解决的问题（不像则返回 None）
/// 2. 提取根因和解决方案
/// 3. 提取涉及的模块（如果提取不到，则使用工具列表作为替代）
/// 4. 构建 CaseRecord，使用确定性 ID
///
/// 各字段有长度限制（question: 500字符, root_cause: 500字符, solution: 500字符）。
pub fn extract_case(
    question: &str,
    answer: &str,
    tools_used: &[String],
    workspace_id: &str,
    user_id: &str,
    duration_seconds: f64,
) -> Option<CaseRecord> {
    // 如果回答不像已解决的问题，直接返回 None
    if !looks_like_resolved_issue(question, answer) {
        return None;
    }

    // 提取根因和解决方案
    let root_cause = extract_sentence(answer, &CAUSE_PATTERNS);
    let solution = extract_sentence(answer, &SOLUTION_PATTERNS);

    // 提取模块列表，提取不到时用工具列表作为回退
    let mut modules = extract_modules(answer);
    if modules.is_empty() {
        modules = tools_used.to_vec();
    }

    // 构建案例记录
    let mut record = CaseRecord::new(
        &question[..question.len().min(500)], // 问题截断到500字符
        workspace_id,
    );
    record.case_id = deterministic_id(question, answer); // 使用确定性 ID
    record.root_cause = root_cause.chars().take(500).collect(); // 根因截断到500字符
    record.solution = solution.chars().take(500).collect(); // 方案截断到500字符
    record.modules = modules;
    record.tools_used = tools_used.to_vec();
    record.user_id = user_id.to_string();
    record.duration_seconds = duration_seconds;

    Some(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：包含解决关键词的回答应被识别
    #[test]
    fn test_looks_like_resolved() {
        assert!(looks_like_resolved_issue("q", "问题已修复，原因是配置错误"));
        assert!(looks_like_resolved_issue("q", "Root cause: missing dep"));
        assert!(!looks_like_resolved_issue("q", "I don't know"));
        assert!(!looks_like_resolved_issue("q", ""));
    }

    /// 测试：基本的案例提取功能
    #[test]
    fn test_extract_case_basic() {
        let answer = "Root cause: missing import. Solution: added import statement";
        let case = extract_case("build fails", answer, &[], "ws1", "u1", 5.0);
        assert!(case.is_some());
        let c = case.unwrap();
        assert!(c.root_cause.contains("missing import"));
    }

    /// 测试：确定性 ID 的一致性和唯一性
    #[test]
    fn test_deterministic_id() {
        let id1 = deterministic_id("q1", "a1");
        let id2 = deterministic_id("q1", "a1");
        let id3 = deterministic_id("q1", "a2");
        assert_eq!(id1, id2); // 相同输入产生相同 ID
        assert_ne!(id1, id3); // 不同输入产生不同 ID
        assert_eq!(id1.len(), 12); // ID 长度为12个十六进制字符
    }

    /// 测试：模块名称提取
    #[test]
    fn test_extract_modules() {
        let text = "模块：auth, user-service";
        let mods = extract_modules(text);
        assert_eq!(mods, vec!["auth", "user-service"]);
    }

    /// 测试：提取不到模块名时，回退到使用工具列表
    #[test]
    fn test_fallback_to_tools() {
        let answer = "Root cause: config error. Fix: updated config";
        let tools = vec!["read_file".into(), "edit_file".into()];
        let case = extract_case("q", answer, &tools, "ws1", "", 0.0).unwrap();
        assert_eq!(case.modules, tools);
    }
}
