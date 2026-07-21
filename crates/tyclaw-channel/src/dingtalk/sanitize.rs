//! 钉钉/多多出口的 Markdown 管道表格清洗。
//!
//! 钉钉客户端渲染不了 GFM 管道表格(`| 列 |` + `|---|`),表格行会被压成
//! 一行显示成管道符原文;同条消息里的 bullet list 可正常渲染。本模块在
//! **下发出口**把回复正文里的管道表格幂等转换为 bullet list,作为渠道级
//! 确定性兜底(不依赖 LLM 自检、也无需按 skill 开关)。
//!
//! 转换规则与 Python 版
//! `workspace/skills/finance/weekly-report-faq/scripts/sanitize_markdown.py`
//! 完全对齐,两份实现需同步(测试用例亦从 `test_sanitize_markdown.py` 移植)。

/// 判断某行(strip 后)是否为围栏代码块起止行(3+ 反引号或波浪号)。
fn is_code_fence(line: &str) -> bool {
    let s = line.trim();
    s.starts_with("```") || s.starts_with("~~~")
}

/// 判断某行是否为管道行:以 `|` 开头,去尾部空白后以 `|` 结尾且至少两个 `|`。
/// 对齐 Python `^\|.*\|\s*$`(不允许前导空白)。
fn is_pipe_row(line: &str) -> bool {
    if !line.starts_with('|') {
        return false;
    }
    let trimmed = line.trim_end();
    trimmed.len() >= 2 && trimmed.ends_with('|')
}

/// 判断某单元格(已 trim)是否为分隔单元格:可选前导 `:`,2+ 个 `-`,可选尾部 `:`。
fn is_dash_cell(cell: &str) -> bool {
    let mut chars = cell.chars().peekable();
    if chars.peek() == Some(&':') {
        chars.next();
    }
    let mut dashes = 0usize;
    while chars.peek() == Some(&'-') {
        chars.next();
        dashes += 1;
    }
    if dashes < 2 {
        return false;
    }
    if chars.peek() == Some(&':') {
        chars.next();
    }
    chars.next().is_none()
}

/// 判断某行是否为表格分隔行(`|:---|:---:|---:|` 之类)。
/// 对齐 Python `^\|(?:\s*:?-{2,}:?\s*\|)+\s*$`。
fn is_separator_row(line: &str) -> bool {
    let s = line.trim_end();
    if s.len() < 2 || !s.starts_with('|') || !s.ends_with('|') {
        return false;
    }
    // 去掉首尾的 `|`(均为 ASCII,字节切片安全),对内部按 `|` 分列。
    let inner = &s[1..s.len() - 1];
    let cells: Vec<&str> = inner.split('|').collect();
    if cells.is_empty() {
        return false;
    }
    cells.iter().all(|c| is_dash_cell(c.trim()))
}

/// 把 `| a | b | c |` 拆成去空白的单元格列表。
fn split_pipe_row(line: &str) -> Vec<String> {
    let mut s = line.trim();
    if let Some(rest) = s.strip_prefix('|') {
        s = rest;
    }
    if let Some(rest) = s.strip_suffix('|') {
        s = rest;
    }
    s.split('|').map(|c| c.trim().to_string()).collect()
}

/// 从一行的各单元格拼出行内字段描述(`列名 值，列名 值`)。
fn build_parts(headers: &[String], row: &[String], skip_first: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    let start = if skip_first { 1 } else { 0 };
    for i in start..headers.len() {
        let val = row.get(i).map(|s| s.trim()).unwrap_or("");
        if val.is_empty() {
            continue;
        }
        let hdr = headers.get(i).map(|s| s.trim()).unwrap_or("");
        if hdr.is_empty() {
            parts.push(val.to_string());
        } else {
            parts.push(format!("{hdr} {val}"));
        }
    }
    parts.join("，")
}

/// 把解析出的表格转换为 bullet list markdown。
///
/// - 3+ 列、首列唯一 -> `- **{首列}**：{列2名} {值}，...`
/// - 3+ 列、首列有重复(分组) -> `**{分组名}**` 标题 + 子 bullet
/// - 2 列(key-value) -> `- **{key}**：{value}`
fn table_to_bullets(header_cells: &[String], data_rows: &[Vec<String>]) -> String {
    if data_rows.is_empty() {
        return String::new();
    }

    let ncols = header_cells.len();

    let first_vals: Vec<String> = data_rows
        .iter()
        .map(|r| r.first().map(|s| s.trim().to_string()).unwrap_or_default())
        .collect();
    let distinct_nonempty: std::collections::HashSet<&str> = first_vals
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    let has_groups = first_vals.len() != distinct_nonempty.len();

    let mut lines: Vec<String> = Vec::new();

    if ncols <= 2 {
        for row in data_rows {
            let mut row = row.clone();
            while row.len() < ncols {
                row.push(String::new());
            }
            let key = row.first().map(|s| s.trim()).unwrap_or("");
            let val = if ncols == 2 {
                row.get(1).map(|s| s.trim()).unwrap_or("")
            } else {
                ""
            };
            if !key.is_empty() && !val.is_empty() {
                lines.push(format!("- **{key}**：{val}"));
            } else if !key.is_empty() {
                lines.push(format!("- {key}"));
            } else if !val.is_empty() {
                lines.push(format!("- {val}"));
            }
        }
        return lines.join("\n");
    }

    if has_groups {
        let mut prev_first: Option<String> = None;
        for row in data_rows {
            let mut row = row.clone();
            while row.len() < ncols {
                row.push(String::new());
            }
            let first_val = row[0].trim().to_string();
            if !first_val.is_empty() && Some(&first_val) != prev_first.as_ref() {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push(format!("**{first_val}**"));
                prev_first = Some(first_val);
            }
            let parts = build_parts(header_cells, &row, true);
            if !parts.is_empty() {
                lines.push(format!("- {parts}"));
            }
        }
    } else {
        for row in data_rows {
            let mut row = row.clone();
            while row.len() < ncols {
                row.push(String::new());
            }
            let first_val = row[0].trim().to_string();
            let rest = build_parts(header_cells, &row, true);
            if !first_val.is_empty() && !rest.is_empty() {
                lines.push(format!("- **{first_val}**：{rest}"));
            } else if !first_val.is_empty() {
                lines.push(format!("- **{first_val}**"));
            } else if !rest.is_empty() {
                lines.push(format!("- {rest}"));
            }
        }
    }

    lines.join("\n")
}

/// 找到文本中所有管道表格,返回 `(起始行, 结束行, 替换文本)`。跳过代码块内的表格。
fn find_tables(text: &str) -> Vec<(usize, usize, String)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();

    let mut in_code_block = false;
    let mut tables: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0usize;

    while i < n {
        if is_code_fence(lines[i]) {
            in_code_block = !in_code_block;
            i += 1;
            continue;
        }
        if in_code_block {
            i += 1;
            continue;
        }

        if i + 1 < n && is_pipe_row(lines[i]) && is_separator_row(lines[i + 1]) {
            let header_cells = split_pipe_row(lines[i]);
            let table_start = i;
            i += 2; // 跳过表头 + 分隔行

            let mut data_rows: Vec<Vec<String>> = Vec::new();
            while i < n && is_pipe_row(lines[i]) {
                if is_code_fence(lines[i]) {
                    break;
                }
                data_rows.push(split_pipe_row(lines[i]));
                i += 1;
            }

            let replacement = table_to_bullets(&header_cells, &data_rows);
            tables.push((table_start, i, replacement));
        } else {
            i += 1;
        }
    }

    tables
}

/// 把文本中所有 GFM 管道表格转换为 bullet list。无表格时原样返回(幂等)。
pub fn sanitize_pipe_tables(text: &str) -> String {
    let tables = find_tables(text);
    if tables.is_empty() {
        return text.to_string();
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let mut result_parts: Vec<String> = Vec::new();
    let mut prev_end = 0usize;

    for (start, end, replacement) in tables {
        result_parts.push(lines[prev_end..start].join("\n"));
        if result_parts.last().map(|s| !s.is_empty()).unwrap_or(false) {
            result_parts.push(String::new());
        }
        result_parts.push(replacement);
        prev_end = end;
    }

    if prev_end < lines.len() {
        let remaining = lines[prev_end..].join("\n");
        if !remaining.is_empty() {
            result_parts.push(remaining);
        }
    }

    result_parts.join("\n")
}

/// 「追问建议 / 猜你想问」标题行的识别标记（去除 emoji/加粗后按子串匹配）。
const RECOMMEND_HEADINGS: [&str; 4] = [
    "您可能还想了解",
    "你可能还想问",
    "您可能还想问",
    "你可能还想了解",
];

/// 解析单个列表项，返回去掉序号/项目符号与加粗标记后的问题文本。
///
/// 支持有序（`1.` / `1、` / `1)` / `1）` / `1．`）与无序（`-` / `*` / `•`）两类。
/// 非列表行返回 `None`。
fn parse_list_item(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    let first = t.chars().next()?;

    let after = if first == '-' || first == '*' || first == '•' {
        t[first.len_utf8()..].trim_start()
    } else if first.is_ascii_digit() {
        // 吸收连续数字
        let mut idx = 0usize;
        for c in t.chars() {
            if c.is_ascii_digit() {
                idx += c.len_utf8();
            } else {
                break;
            }
        }
        let rest = &t[idx..];
        let sep = rest.chars().next()?;
        if matches!(sep, '.' | ')' | '、' | '）' | '．' | '：' | ':') {
            rest[sep.len_utf8()..].trim_start()
        } else {
            return None;
        }
    } else {
        return None;
    };

    let cleaned = after.replace("**", "");
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

/// 从回复正文末尾提取 skill 生成的「追问建议 / 猜你想问」块。
///
/// 返回 `(剥离该块后的正文, 追问问题列表)`。用于渠道 egress：正文丰富度由
/// skill 全权控制（模型把领域感知的追问建议写进正文末尾），本函数只负责把该
/// 文本块解析出来，交给上层渲染成钉钉卡片的「猜你想问」按钮，并从展示正文里
/// 去掉它以免重复。
///
/// 未匹配到追问块时原样返回 `(text, vec![])`，因此对没有该块的 skill / 渠道无副作用。
pub fn extract_recommends(text: &str) -> (String, Vec<String>) {
    let lines: Vec<&str> = text.split('\n').collect();

    // 取最后一个匹配的标题行（避免正文中偶然提及导致误判）。
    let heading_idx = lines.iter().enumerate().rev().find_map(|(i, line)| {
        let stripped = line.replace(|c| matches!(c, '*' | '#' | '💡' | '🤔'), "");
        if RECOMMEND_HEADINGS.iter().any(|m| stripped.contains(m)) {
            Some(i)
        } else {
            None
        }
    });
    let hi = match heading_idx {
        Some(i) => i,
        None => return (text.to_string(), Vec::new()),
    };

    // 从标题行之后收集列表项，遇到非列表行（如数据来源标注）或文本结束即停止。
    // 允许列表项之间夹空行。
    let mut questions: Vec<String> = Vec::new();
    let mut last_item = hi;
    let mut j = hi + 1;
    while j < lines.len() {
        if lines[j].trim().is_empty() {
            // 探测下一非空行是否仍是列表项：是则跨过空行继续，否则停止。
            let mut k = j + 1;
            while k < lines.len() && lines[k].trim().is_empty() {
                k += 1;
            }
            if k < lines.len() && parse_list_item(lines[k]).is_some() {
                j = k;
                continue;
            }
            break;
        }
        match parse_list_item(lines[j]) {
            Some(q) => {
                if !questions.contains(&q) {
                    questions.push(q);
                }
                last_item = j;
                j += 1;
            }
            None => break,
        }
    }

    if questions.is_empty() {
        return (text.to_string(), Vec::new());
    }
    questions.truncate(5);

    // 向前吞掉紧邻标题的空行与单独的分隔线，避免剥离后留下悬空的 `---`。
    let mut strip_start = hi;
    while strip_start > 0 {
        let prev = lines[strip_start - 1].trim();
        if prev.is_empty() || prev == "---" || prev == "***" || prev == "___" {
            strip_start -= 1;
        } else {
            break;
        }
    }

    let mut kept: Vec<&str> = Vec::new();
    kept.extend_from_slice(&lines[..strip_start]);
    if last_item + 1 < lines.len() {
        kept.extend_from_slice(&lines[last_item + 1..]);
    }
    let stripped = kept.join("\n").trim_end().to_string();

    (stripped, questions)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 3+ 列、首列唯一 -> 平铺 bullet ---

    #[test]
    fn test_simple_3col() {
        let md = "| 区域 | 上线主体 | 渠道 |\n|---|---|---|\n| 海外 | Ark | Google Play |\n| 俄罗斯 | Cosmos | 俄罗斯包-Google |\n| 境内 | 广州润耀 | 微小 |";
        let r = sanitize_pipe_tables(md);
        assert!(!r.contains("|---|"));
        assert!(r.contains("- **海外**："));
        assert!(r.contains("- **俄罗斯**："));
        assert!(r.contains("- **境内**："));
        assert!(r.contains("上线主体 Ark"));
    }

    #[test]
    fn test_4col_with_status() {
        let md = "| 区域 | 上线主体 | 渠道 | 状态 |\n|---|---|---|---|\n| 海外 | Ark | Google Play | 已上线 |\n| 境内 | 广州润耀 | 微小 | 暂未上线 |";
        let r = sanitize_pipe_tables(md);
        assert!(r.contains("- **海外**：上线主体 Ark，渠道 Google Play，状态 已上线"));
    }

    #[test]
    fn test_alignment_variants() {
        let md = "| A | B | C |\n|:---|:---:|---:|\n| x | y | z |";
        let r = sanitize_pipe_tables(md);
        assert!(!r.contains("|---|"));
        assert!(!r.contains("|:---|"));
        assert!(r.contains("- **x**："));
    }

    // --- 2 列 key-value ---

    #[test]
    fn test_competitor_info() {
        let md = "| 字段 | 内容 |\n|---|---|\n| 研发主体 | FunPlus |\n| 全球上线 | 2023-02 |";
        let r = sanitize_pipe_tables(md);
        assert!(r.contains("- **研发主体**：FunPlus"));
        assert!(r.contains("- **全球上线**：2023-02"));
    }

    // --- 3+ 列、首列重复 -> 分组 ---

    #[test]
    fn test_grouped_rows() {
        let md = "| 主体分类 | 上线主体 | 渠道 |\n|---|---|---|\n| 境内主体 | 欢游互动 | 安卓 |\n| 境内主体 | 北京雪境 | 抖音 |\n| 境外主体 | Ark | Google |";
        let r = sanitize_pipe_tables(md);
        assert!(r.contains("**境内主体**"));
        assert!(r.contains("**境外主体**"));
        assert!(r.contains("- 上线主体 欢游互动"));
        assert!(r.contains("- 上线主体 Ark"));
    }

    // --- 幂等 ---

    #[test]
    fn test_bullet_list_unchanged() {
        let md = "- **海外**：上线主体 Ark，渠道 Google Play\n- **俄罗斯**：上线主体 Cosmos";
        assert_eq!(sanitize_pipe_tables(md), md);
    }

    #[test]
    fn test_plain_text_unchanged() {
        let md = "这是一段普通文本，不含表格。\n\n第二段。";
        assert_eq!(sanitize_pipe_tables(md), md);
    }

    #[test]
    fn test_empty_string() {
        assert_eq!(sanitize_pipe_tables(""), "");
    }

    // --- 代码块跳过 ---

    #[test]
    fn test_backtick_fence() {
        let md = "```markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n```";
        assert_eq!(sanitize_pipe_tables(md), md);
    }

    #[test]
    fn test_tilde_fence() {
        let md = "~~~\n| A | B |\n|---|---|\n| 1 | 2 |\n~~~";
        assert_eq!(sanitize_pipe_tables(md), md);
    }

    #[test]
    fn test_mixed_code_and_real_table() {
        let md = "```\n| X | Y |\n|---|---|\n| fake | table |\n```\n\n| A | B |\n|---|---|\n| real | table |";
        let r = sanitize_pipe_tables(md);
        assert!(r.contains("| fake | table |"));
        assert!(!r.contains("| real | table |"));
        assert!(r.contains("- **real**：table"));
    }

    // --- 多表格 ---

    #[test]
    fn test_two_tables_both_converted() {
        let md = "第一个表：\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n中间文字\n\n| X | Y | Z |\n|---|---|---|\n| a | b | c |";
        let r = sanitize_pipe_tables(md);
        assert_eq!(r.matches("|---|").count(), 0);
        assert!(r.contains("- **1**：2"));
        assert!(r.contains("- **a**："));
        assert!(r.contains("中间文字"));
    }

    // --- 边界 ---

    #[test]
    fn test_header_only_no_data_rows() {
        let md = "| A | B |\n|---|---|";
        let r = sanitize_pipe_tables(md);
        assert!(!r.contains("|---|"));
    }

    #[test]
    fn test_empty_cells() {
        let md = "| K | V |\n|---|---|\n| name | Alice |\n|  |  |\n| age | 30 |";
        let r = sanitize_pipe_tables(md);
        assert!(r.contains("- **name**：Alice"));
        assert!(r.contains("- **age**：30"));
    }

    #[test]
    fn test_surrounding_text_preserved() {
        let md = "标题文字\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n尾部文字";
        let r = sanitize_pipe_tables(md);
        assert!(r.contains("标题文字"));
        assert!(r.contains("尾部文字"));
    }

    // --- 截图里的渠道主体拆分表(方案回归用例) ---

    #[test]
    fn test_channel_split_table() {
        let md = "| 渠道 | 上线主体 | 备注 |\n|------|------|------|\n| 海外 Google Play | Ark | 6月新包上线，主体在 BVI 香港 |\n| iOS | ARK | 7-8月上线 |\n| 俄罗斯 | Evista | 7月上线 |";
        let r = sanitize_pipe_tables(md);
        assert!(!r.contains("|---|") && !r.contains("|------|"));
        assert!(r.contains("- **海外 Google Play**：上线主体 Ark，备注 6月新包上线，主体在 BVI 香港"));
        assert!(r.contains("- **iOS**：上线主体 ARK，备注 7-8月上线"));
        assert!(r.contains("- **俄罗斯**：上线主体 Evista，备注 7月上线"));
    }

    // --- extract_recommends：追问建议块提取 ---

    #[test]
    fn test_extract_recommends_numbered() {
        let md = "正文第一行\n\n- 明细1\n- 明细2\n\n---\n\n💡 **您可能还想了解：**\n1. TM新包的买量数据如何？\n2. 次神项目组的渠道主体拆分\n3. 俄罗斯区6月上线后表现\n\n📎 数据来源：渠道详情 sheet";
        let (body, qs) = extract_recommends(md);
        assert_eq!(qs.len(), 3);
        assert_eq!(qs[0], "TM新包的买量数据如何？");
        assert_eq!(qs[1], "次神项目组的渠道主体拆分");
        // 正文保留、追问块被剥离、数据来源标注保留
        assert!(body.contains("正文第一行"));
        assert!(body.contains("📎 数据来源：渠道详情 sheet"));
        assert!(!body.contains("您可能还想了解"));
        assert!(!body.contains("TM新包的买量数据如何"));
        // 悬空的 --- 分隔线也被吞掉
        assert!(!body.contains("---"));
    }

    #[test]
    fn test_extract_recommends_no_block() {
        let md = "普通回答正文\n\n- 要点1\n- 要点2\n\n📎 数据来源：某 sheet";
        let (body, qs) = extract_recommends(md);
        assert!(qs.is_empty());
        assert_eq!(body, md);
    }

    #[test]
    fn test_extract_recommends_at_end() {
        let md = "结论正文。\n\n🤔 你可能还想问：\n- 问题一\n- 问题二";
        let (body, qs) = extract_recommends(md);
        assert_eq!(qs, vec!["问题一".to_string(), "问题二".to_string()]);
        assert_eq!(body, "结论正文。");
    }

    #[test]
    fn test_extract_recommends_strips_bold_and_dedup() {
        let md = "正文\n\n💡 您可能还想了解：\n1. **重复问题**\n2. 重复问题\n3. 另一个问题";
        let (_body, qs) = extract_recommends(md);
        assert_eq!(qs, vec!["重复问题".to_string(), "另一个问题".to_string()]);
    }

    #[test]
    fn test_extract_recommends_caps_at_five() {
        let md = "正文\n\n💡 您可能还想了解：\n1. a\n2. b\n3. c\n4. d\n5. e\n6. f\n7. g";
        let (_body, qs) = extract_recommends(md);
        assert_eq!(qs.len(), 5);
    }

    #[test]
    fn test_extract_recommends_empty_string() {
        let (body, qs) = extract_recommends("");
        assert_eq!(body, "");
        assert!(qs.is_empty());
    }
}
