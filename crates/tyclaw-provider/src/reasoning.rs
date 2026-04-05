//! Reasoning 结构化解析器。
//!
//! 不同模型的 reasoning/thinking 内容格式各异：
//! - DeepSeek：纯文本思考过程
//! - GLM：可能混入 `<tool_call>` XML 标签
//! - Claude (via proxy)：纯文本 thinking
//!
//! 本模块将原始 reasoning 文本解析为结构化的 `ReasoningBlock` 列表，
//! 方便前端展示和后端分析（如抢救错误放置的 tool_call）。

use serde::{Deserialize, Serialize};

/// Reasoning 中的一个结构化段落。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ReasoningBlock {
    /// 纯文本思考过程
    #[serde(rename = "thinking")]
    Thinking { text: String },

    /// 模型在 reasoning 中生成的代码块
    #[serde(rename = "code")]
    Code {
        language: Option<String>,
        code: String,
    },

    /// 被错误放置在 reasoning 中的 tool_call（XML 格式）
    #[serde(rename = "tool_call")]
    ToolCall {
        tool_name: String,
        arguments: Vec<(String, String)>,
        raw: String,
    },

    /// 无法解析的原始片段
    #[serde(rename = "raw")]
    Raw { text: String },
}

/// 解析后的完整 reasoning 结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedReasoning {
    /// 结构化段落列表
    pub blocks: Vec<ReasoningBlock>,
    /// 清洗后的纯文本（去掉 tool_call XML 和代码块标记，用于前端展示）
    pub display_text: String,
    /// 原始文本长度
    pub raw_length: usize,
    /// 是否包含被错误放置的 tool_call
    pub has_misplaced_tool_calls: bool,
}

/// 解析原始 reasoning 文本为结构化块。
pub fn parse_reasoning(raw: &str) -> ParsedReasoning {
    let raw_length = raw.len();
    let mut blocks: Vec<ReasoningBlock> = Vec::new();
    let mut display_parts: Vec<String> = Vec::new();
    let mut has_misplaced_tool_calls = false;

    let mut cursor = 0;

    while cursor < raw.len() {
        let remaining = &raw[cursor..];

        // 1. 检查 <tool_call> 标签
        if let Some(tc_start) = remaining.find("<tool_call>") {
            // 先把 tool_call 之前的文本作为 thinking 块
            if tc_start > 0 {
                let before = &remaining[..tc_start];
                push_text_blocks(&mut blocks, &mut display_parts, before);
            }

            let tc_content_start = cursor + tc_start + "<tool_call>".len();
            if let Some(tc_end) = raw[tc_content_start..].find("</tool_call>") {
                let tc_content = &raw[tc_content_start..tc_content_start + tc_end];
                let (tool_name, arguments) = parse_tool_call_xml(tc_content);

                has_misplaced_tool_calls = true;
                let raw_xml = format!("<tool_call>{}</tool_call>", tc_content);
                blocks.push(ReasoningBlock::ToolCall {
                    tool_name: tool_name.clone(),
                    arguments: arguments.clone(),
                    raw: truncate(raw_xml.as_str(), 500),
                });
                // 在 display_text 中显示为简短摘要
                let args_preview: Vec<String> = arguments
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, truncate(v, 50)))
                    .collect();
                display_parts.push(format!(
                    "[Tool Call: {}({})]",
                    tool_name,
                    args_preview.join(", ")
                ));

                cursor = tc_content_start + tc_end + "</tool_call>".len();
                continue;
            } else {
                // 没有闭合标签，把剩余都当文本
                push_text_blocks(&mut blocks, &mut display_parts, remaining);
                break;
            }
        }

        // 2. 检查 markdown 代码块 ```
        if let Some(code_start) = remaining.find("```") {
            // 先把代码块之前的文本作为 thinking 块
            if code_start > 0 {
                let before = &remaining[..code_start];
                push_text_blocks(&mut blocks, &mut display_parts, before);
            }

            let after_backticks = cursor + code_start + 3;
            // 提取语言标记（如 ```python）
            let lang_end = raw[after_backticks..].find('\n').unwrap_or(0);
            let language = raw[after_backticks..after_backticks + lang_end]
                .trim()
                .to_string();
            let language = if language.is_empty() {
                None
            } else {
                Some(language)
            };

            let code_content_start = after_backticks + lang_end + 1;
            if let Some(code_end) = raw[code_content_start..].find("```") {
                let code = raw[code_content_start..code_content_start + code_end]
                    .trim_end()
                    .to_string();
                blocks.push(ReasoningBlock::Code {
                    language: language.clone(),
                    code: code.clone(),
                });
                let lang_display = language.as_deref().unwrap_or("code");
                display_parts.push(format!("[{}: {} chars]", lang_display, code.len()));

                cursor = code_content_start + code_end + 3;
                continue;
            } else {
                // 没有闭合的代码块，剩余当文本
                push_text_blocks(&mut blocks, &mut display_parts, remaining);
                break;
            }
        }

        // 3. 没有特殊标记了，剩余全是文本
        push_text_blocks(&mut blocks, &mut display_parts, remaining);
        break;
    }

    // 合并连续的 Thinking 块
    blocks = merge_consecutive_thinking(blocks);

    let display_text = display_parts.join("").trim().to_string();

    ParsedReasoning {
        blocks,
        display_text,
        raw_length,
        has_misplaced_tool_calls,
    }
}

/// 从 XML 格式的 tool_call 内容中解析工具名和参数。
fn parse_tool_call_xml(content: &str) -> (String, Vec<(String, String)>) {
    let mut arguments = Vec::new();

    // 工具名：内容开头到第一个 <arg_key> 之间
    let tool_name = if let Some(first_arg) = content.find("<arg_key>") {
        content[..first_arg].trim().to_string()
    } else {
        content.trim().to_string()
    };

    // 提取参数对
    let mut search = 0;
    while let Some(key_start) = content[search..].find("<arg_key>") {
        let key_abs = search + key_start + "<arg_key>".len();
        let Some(key_end) = content[key_abs..].find("</arg_key>") else {
            break;
        };
        let key = content[key_abs..key_abs + key_end].trim().to_string();

        let val_search = key_abs + key_end + "</arg_key>".len();
        if let Some(val_start) = content[val_search..].find("<arg_value>") {
            let val_abs = val_search + val_start + "<arg_value>".len();
            if let Some(val_end) = content[val_abs..].find("</arg_value>") {
                let val = content[val_abs..val_abs + val_end].to_string();
                arguments.push((key, val));
                search = val_abs + val_end + "</arg_value>".len();
                continue;
            }
        }
        break;
    }

    (tool_name, arguments)
}

/// 把文本推入 blocks（Thinking）和 display_parts。
fn push_text_blocks(blocks: &mut Vec<ReasoningBlock>, display_parts: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        blocks.push(ReasoningBlock::Thinking {
            text: trimmed.to_string(),
        });
        display_parts.push(trimmed.to_string());
    }
}

/// 合并连续的 Thinking 块。
fn merge_consecutive_thinking(blocks: Vec<ReasoningBlock>) -> Vec<ReasoningBlock> {
    let mut result: Vec<ReasoningBlock> = Vec::new();

    for block in blocks {
        match (&block, result.last_mut()) {
            (
                ReasoningBlock::Thinking { text: new_text },
                Some(ReasoningBlock::Thinking { text: existing }),
            ) => {
                existing.push('\n');
                existing.push_str(new_text);
            }
            _ => {
                result.push(block);
            }
        }
    }

    result
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let boundary = s.floor_char_boundary(max);
        format!("{}...", &s[..boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pure_thinking() {
        let raw = "我需要先分析数据结构，然后再写代码。\n\n让我看看 1.xlsx 的列名。";
        let parsed = parse_reasoning(raw);
        assert_eq!(parsed.blocks.len(), 1);
        assert!(!parsed.has_misplaced_tool_calls);
        assert!(matches!(&parsed.blocks[0], ReasoningBlock::Thinking { .. }));
        assert_eq!(parsed.display_text, raw.trim());
    }

    #[test]
    fn test_tool_call_in_reasoning() {
        let raw = "我需要读取文件。<tool_call>read_file<arg_key>path</arg_key><arg_value>test.txt</arg_value></tool_call>然后分析内容。";
        let parsed = parse_reasoning(raw);
        assert!(parsed.has_misplaced_tool_calls);
        assert_eq!(parsed.blocks.len(), 3); // thinking + tool_call + thinking
        assert!(
            matches!(&parsed.blocks[1], ReasoningBlock::ToolCall { tool_name, .. } if tool_name == "read_file")
        );
        assert!(parsed.display_text.contains("[Tool Call: read_file"));
    }

    #[test]
    fn test_code_block() {
        let raw = "让我写一段代码：\n```python\nprint('hello')\n```\n代码写完了。";
        let parsed = parse_reasoning(raw);
        assert_eq!(parsed.blocks.len(), 3); // thinking + code + thinking
        assert!(
            matches!(&parsed.blocks[1], ReasoningBlock::Code { language, code, .. }
            if language.as_deref() == Some("python") && code == "print('hello')")
        );
    }

    #[test]
    fn test_multiple_tool_calls() {
        let raw = "分析任务。<tool_call>read_file<arg_key>path</arg_key><arg_value>a.txt</arg_value></tool_call>读完了。<tool_call>list_dir<arg_key>path</arg_key><arg_value>.</arg_value></tool_call>列完了。";
        let parsed = parse_reasoning(raw);
        assert!(parsed.has_misplaced_tool_calls);
        let tc_count = parsed
            .blocks
            .iter()
            .filter(|b| matches!(b, ReasoningBlock::ToolCall { .. }))
            .count();
        assert_eq!(tc_count, 2);
    }

    #[test]
    fn test_mixed_content() {
        let raw = "思考中...\n```python\nx = 1\n```\n<tool_call>exec<arg_key>command</arg_key><arg_value>ls</arg_value></tool_call>结束。";
        let parsed = parse_reasoning(raw);
        assert!(parsed.has_misplaced_tool_calls);
        // thinking + code + tool_call + thinking
        assert!(parsed.blocks.len() >= 3);
    }

    #[test]
    fn test_empty() {
        let parsed = parse_reasoning("");
        assert!(parsed.blocks.is_empty());
        assert!(!parsed.has_misplaced_tool_calls);
        assert!(parsed.display_text.is_empty());
    }
}
