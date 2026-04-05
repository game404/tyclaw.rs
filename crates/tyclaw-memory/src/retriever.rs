//! 案例检索器：基于关键词匹配和时间衰减评分的相似案例搜索。
//!
//! 评分公式：score = keyword_score × time_decay
//! - keyword_score: 查询词与案例词的交集数 / 查询词总数
//! - time_decay: exp(-0.693 × age_days / 30)，半衰期为30天

use std::path::Path;

use chrono::{Local, NaiveDateTime};
use regex::Regex;

use crate::case_store::{CaseRecord, CaseStore};

/// 时间衰减的半衰期（天）。
/// 30天前的案例权重约为当前案例的一半，60天约为1/4。
const HALF_LIFE_DAYS: f64 = 30.0;

lazy_static::lazy_static! {
    /// ASCII 单词匹配正则 —— 用于提取英文单词和数字标识符
    static ref ASCII_WORD_RE: Regex = Regex::new(r"[a-zA-Z0-9_]+").unwrap();

    /// CJK（中日韩）字符连续序列匹配正则。
    /// 由于中文不以空格分词，这里将连续的中文字符作为一个整体 token。
    static ref CJK_RUN_RE: Regex = Regex::new(r"[\x{4e00}-\x{9fff}\x{3400}-\x{4dbf}]+").unwrap();
}

/// 文本分词器 —— 将文本拆分为可匹配的 token 集合。
///
/// 处理规则：
/// - ASCII 单词：提取并转为小写（如 "Hello" → "hello"）
/// - CJK 字符：连续的中文字符作为一个 token（如 "你好世界" 作为整体）
///
/// 返回去重的 token 集合。
fn tokenize(text: &str) -> std::collections::HashSet<String> {
    let mut tokens = std::collections::HashSet::new();
    // 提取 ASCII 单词
    for m in ASCII_WORD_RE.find_iter(text) {
        tokens.insert(m.as_str().to_lowercase());
    }
    // 提取 CJK 字符序列
    for m in CJK_RUN_RE.find_iter(text) {
        tokens.insert(m.as_str().to_string());
    }
    tokens
}

/// 计算时间衰减系数。
///
/// 基于指数衰减公式：decay = exp(-0.693 × age_days / half_life)
/// - 当前时间的案例：decay ≈ 1.0
/// - 30天前的案例：decay ≈ 0.5
/// - 60天前的案例：decay ≈ 0.25
///
/// 支持 RFC 3339 格式和 ISO 8601 无时区格式的时间戳。
/// 解析失败时返回默认值 0.5。
fn time_decay(timestamp: &str) -> f64 {
    let ts = match chrono::DateTime::parse_from_rfc3339(timestamp) {
        Ok(dt) => dt.with_timezone(&Local).naive_local(),
        Err(_) => {
            // 尝试无时区的 ISO 8601 格式
            match NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S%.f") {
                Ok(dt) => dt,
                Err(_) => return 0.5, // 解析失败，使用默认衰减值
            }
        }
    };
    let now = Local::now().naive_local();
    let age_days = (now - ts).num_seconds().max(0) as f64 / 86400.0; // 转换为天数
    (-0.693 * age_days / HALF_LIFE_DAYS).exp() // 指数衰减
}

/// 将案例的关键信息拼接为可搜索的文本。
///
/// 拼接 question + root_cause + solution + modules，
/// 作为关键词匹配的目标文本。
fn case_text(case: &CaseRecord) -> String {
    let mut parts = vec![
        case.question.as_str(),
        case.root_cause.as_str(),
        case.solution.as_str(),
    ];
    for m in &case.modules {
        parts.push(m.as_str());
    }
    parts.join(" ")
}

/// 带评分的案例 —— 检索结果中的单条记录。
pub struct ScoredCase {
    pub case: CaseRecord, // 案例记录
    pub score: f64,       // 综合评分（关键词匹配 × 时间衰减）
}

/// 案例检索器 —— 基于关键词 + 时间衰减的相似案例搜索。
///
/// 使用引用的 CaseStore，不拥有其所有权。
pub struct CaseRetriever<'a> {
    store: &'a CaseStore,
}

impl<'a> CaseRetriever<'a> {
    pub fn new(store: &'a CaseStore) -> Self {
        Self { store }
    }

    /// 搜索相似案例。
    ///
    /// 算法流程：
    /// 1. 对查询文本进行分词
    /// 2. 遍历工作区内所有案例，计算每个案例的综合评分
    /// 3. 评分 = (查询词与案例词的交集数 / 查询词总数) × 时间衰减系数
    /// 4. 过滤掉没有任何关键词匹配的案例
    /// 5. 按评分降序排列，返回 top_k 个结果
    pub fn search(&self, query: &str, workspace_cases_dir: &Path, top_k: usize) -> Vec<ScoredCase> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let cases = self.store.list_merged(workspace_cases_dir);
        let mut scored: Vec<ScoredCase> = Vec::new();

        for case in cases {
            let case_tokens = tokenize(&case_text(&case));
            // 计算查询词与案例词的交集
            let overlap = query_tokens.intersection(&case_tokens).count();
            if overlap == 0 {
                continue; // 没有任何匹配的关键词，跳过
            }

            // 关键词匹配得分 = 匹配数 / 查询词总数
            let keyword_score = overlap as f64 / query_tokens.len().max(1) as f64;
            // 时间衰减系数
            let decay = time_decay(&case.timestamp);
            // 综合评分
            let score = keyword_score * decay;

            scored.push(ScoredCase { case, score });
        }

        // 按评分降序排列
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(top_k); // 只保留 top_k 个
        scored
    }

    /// 将相似案例格式化为 LLM 提示词的一部分。
    ///
    /// 输出顺序：固定案例（pinned）→ 关键词匹配的相似案例。
    /// 固定案例始终包含，不受关键词匹配和时间衰减影响。
    /// 如果两者都为空，返回空字符串。
    /// 返回 (pinned_cases, similar_cases) 两个独立字符串。
    /// pinned 是稳定内容（可放入 cache boundary 之前），similar 是动态内容。
    pub fn format_for_prompt_split(
        &self,
        query: &str,
        workspace_cases_dir: &Path,
        top_k: usize,
    ) -> (String, String) {
        let pinned = self.store.list_pinned(workspace_cases_dir);
        let results = self.search(query, workspace_cases_dir, top_k);

        let mut case_num = 0;

        let pinned_str = if !pinned.is_empty() {
            let mut lines = vec!["## Pinned Cases (must follow)".to_string()];
            for case in &pinned {
                case_num += 1;
                lines.push(Self::format_case(case_num, case, true));
            }
            lines.join("\n")
        } else {
            String::new()
        };

        let similar_str = if !results.is_empty() {
            let mut lines = vec![
                "## Similar Historical Cases (reference only, do not copy conclusions)".to_string(),
            ];
            for item in &results {
                case_num += 1;
                lines.push(Self::format_case(case_num, &item.case, false));
            }
            lines.join("\n")
        } else {
            String::new()
        };

        (pinned_str, similar_str)
    }

    /// 返回合并的字符串（pinned + similar）。
    pub fn format_for_prompt(&self, query: &str, workspace_cases_dir: &Path, top_k: usize) -> String {
        let (pinned, similar) = self.format_for_prompt_split(query, workspace_cases_dir, top_k);
        if pinned.is_empty() && similar.is_empty() {
            return String::new();
        }
        let mut result = pinned;
        if !similar.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&similar);
        }
        result
    }

    /// 格式化单条案例为 Markdown。
    fn format_case(num: usize, case: &CaseRecord, include_modules: bool) -> String {
        let mut parts = vec![format!("\n### Case {}: {}", num, case.question)];
        if !case.root_cause.is_empty() {
            parts.push(format!("- Root cause: {}", case.root_cause));
        }
        if !case.solution.is_empty() {
            parts.push(format!("- Solution: {}", case.solution));
        }
        if !case.evidence.is_empty() {
            parts.push("- Evidence:".to_string());
            for e in &case.evidence {
                parts.push(format!("  - {}", e));
            }
        }
        if include_modules && !case.modules.is_empty() {
            parts.push(format!("- Modules: {}", case.modules.join(", ")));
        }
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// 测试：分词器能正确处理中英文混合文本
    #[test]
    fn test_tokenize() {
        let tokens = tokenize("Hello World 你好世界");
        assert!(tokens.contains("hello")); // 英文转小写
        assert!(tokens.contains("world"));
        assert!(tokens.contains("你好世界")); // 中文作为整体
    }

    /// 测试：最近的时间戳应得到接近1.0的衰减值
    #[test]
    fn test_time_decay_recent() {
        let now = Local::now().to_rfc3339();
        let d = time_decay(&now);
        assert!(d > 0.9); // 刚创建的案例，衰减值接近 1.0
    }

    /// 测试：搜索相似案例
    #[test]
    fn test_search() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("global_cases");
        let ws_cases = tmp.path().join("ws_cases");
        std::fs::create_dir_all(&ws_cases).unwrap();
        let store = CaseStore::new(&global_dir);

        let mut case = CaseRecord::new("build fails missing import", "ws1");
        case.root_cause = "Missing import statement".into();
        store.save(&case, &ws_cases);

        let retriever = CaseRetriever::new(&store);
        let results = retriever.search("build import error", &ws_cases, 3);
        assert_eq!(results.len(), 1);
        assert!(results[0].score > 0.0);
    }

    /// 测试：格式化为提示词
    #[test]
    fn test_format_for_prompt() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("global_cases");
        let ws_cases = tmp.path().join("ws_cases");
        std::fs::create_dir_all(&ws_cases).unwrap();
        let store = CaseStore::new(&global_dir);

        let mut case = CaseRecord::new("API timeout", "ws1");
        case.root_cause = "Connection pool exhausted".into();
        case.solution = "Increase pool size".into();
        store.save(&case, &ws_cases);

        let retriever = CaseRetriever::new(&store);
        let prompt = retriever.format_for_prompt("API slow timeout", &ws_cases, 3);
        assert!(prompt.contains("Similar Historical Cases"));
        assert!(prompt.contains("Connection pool"));
    }

    /// 测试：pinned cases 始终出现在输出中，且排在关键词匹配结果之前
    #[test]
    fn test_pinned_cases_always_injected() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("global_cases");
        std::fs::create_dir_all(&global_dir).unwrap();
        let ws_cases = tmp.path().join("ws_cases");
        std::fs::create_dir_all(&ws_cases).unwrap();
        let store = CaseStore::new(&global_dir);

        // 创建全局 pinned case
        let pinned = CaseRecord::new("验证阶段轮次过多", "ws1");
        let pinned_json = serde_json::to_string_pretty(&vec![pinned]).unwrap();
        std::fs::write(global_dir.join("pinned_cases.json"), &pinned_json).unwrap();

        // 创建普通 case 到 workspace
        let mut normal = CaseRecord::new("build error", "ws1");
        normal.root_cause = "missing dep".into();
        store.save(&normal, &ws_cases);

        let retriever = CaseRetriever::new(&store);
        let prompt = retriever.format_for_prompt("build error", &ws_cases, 3);
        assert!(prompt.contains("Pinned Cases"));
        assert!(prompt.contains("验证阶段轮次过多"));
        assert!(prompt.contains("Similar Historical Cases"));
        let pinned_pos = prompt.find("验证阶段轮次过多").unwrap();
        let normal_pos = prompt.find("missing dep").unwrap();
        assert!(pinned_pos < normal_pos);
    }

    /// 测试：workspace 级 pinned cases
    #[test]
    fn test_workspace_pinned_cases() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("global_cases");
        std::fs::create_dir_all(&global_dir).unwrap();
        let ws1_cases = tmp.path().join("ws1_cases");
        std::fs::create_dir_all(&ws1_cases).unwrap();
        let ws2_cases = tmp.path().join("ws2_cases");
        std::fs::create_dir_all(&ws2_cases).unwrap();
        let store = CaseStore::new(&global_dir);

        // 创建 workspace 级 pinned case
        let pinned = CaseRecord::new("workspace pinned", "ws1");
        let pinned_json = serde_json::to_string_pretty(&vec![pinned]).unwrap();
        std::fs::write(ws1_cases.join("pinned_cases.json"), &pinned_json).unwrap();

        let retriever = CaseRetriever::new(&store);
        let prompt = retriever.format_for_prompt("anything", &ws1_cases, 3);
        assert!(prompt.contains("workspace pinned"));
        // ws2 不应看到 ws1 的 pinned
        let prompt2 = retriever.format_for_prompt("anything", &ws2_cases, 3);
        assert!(!prompt2.contains("workspace pinned"));
    }
}
