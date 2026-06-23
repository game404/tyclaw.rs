//! 工具结果与参数的历史压缩。
//!
//! 从 `agent_loop.rs` 拆分出的压缩相关函数，负责对历史消息中的
//! 工具调用结果和参数进行衰减压缩，降低 prompt token 消耗。

use serde_json::{json, Value};
use std::collections::HashMap;

use crate::loop_helpers::{
    is_error_envelope, EXEC_INTENT_MEDIUM_MAX_CHARS, EXEC_INTENT_OLD_MAX_CHARS,
    EXPLORE_ABSOLUTE_CAP, TOOL_CALL_ARGS_FRESH_COUNT, TOOL_CALL_ARGS_MEDIUM_CHARS,
    TOOL_CALL_ARGS_OLD_COUNT, TOOL_RESULT_FRESH_COUNT, TOOL_RESULT_MEDIUM_CHARS,
    TOOL_RESULT_OLD_COUNT,
};

/// 对历史消息中的工具结果进行衰减压缩，越老的工具输出越精简。
///
/// **重要**：仅在产出阶段启用衰减。探索阶段保留完整上下文，
/// 避免 LLM 因"失忆"而重复探索相同数据。
///
/// 策略（从消息末尾往前数 tool 类型消息）：
/// - 最近 TOOL_RESULT_FRESH_COUNT 条：保留完整内容
/// - FRESH_COUNT ~ OLD_COUNT 条：截断到 MEDIUM_CHARS 字符
/// - 超过 OLD_COUNT 条：只保留摘要行
///
pub(crate) fn compress_tool_results(
    messages: &[HashMap<String, Value>],
    skip_compression: bool,
    light_keep_recent_rounds: usize,
    protected_prefix_len: usize,
) -> Vec<HashMap<String, Value>> {
    // 跳过压缩时直接返回引用的 clone（不做任何处理）
    if skip_compression {
        return Vec::from(messages);
    }
    // 按 assistant(tool_calls) 估算轮次边界，最近 N 轮尽量不压缩。
    let assistant_round_indices_forward: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.get("role").and_then(|v| v.as_str()) == Some("assistant")
                && m.get("tool_calls")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();
    let recent_round_cutoff_idx =
        if assistant_round_indices_forward.len() > light_keep_recent_rounds {
            assistant_round_indices_forward
                [assistant_round_indices_forward.len() - light_keep_recent_rounds]
        } else {
            0
        };

    // 先收集所有 tool 消息的索引（从后往前）
    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, m)| m.get("role").and_then(|v| v.as_str()) == Some("tool"))
        .map(|(i, _)| i)
        .collect();

    let mut result = messages.to_vec();

    // === Part 1: 压缩旧的 tool result 内容 ===
    // 先标记需要保护的 tool result（需求文档类内容，永不压缩）
    // 规则：前 EXPLORE_ABSOLUTE_CAP 条 tool 中，read_file 的结果视为需求/规格文档
    let protected_indices: std::collections::HashSet<usize> = tool_indices
        .iter()
        .rev() // tool_indices 是从后往前的，反转回正序
        .take(EXPLORE_ABSOLUTE_CAP)
        .filter(|&&idx| {
            let msg = &messages[idx];
            let tool_name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("");
            tool_name == "read_file"
        })
        .cloned()
        .collect();

    for (age, &idx) in tool_indices.iter().enumerate() {
        if idx < protected_prefix_len {
            continue;
        }
        if idx >= recent_round_cutoff_idx {
            continue;
        }
        if age < TOOL_RESULT_FRESH_COUNT {
            // 最近的：保留完整
            continue;
        }
        // 需求文档类 tool result 永不压缩
        if protected_indices.contains(&idx) {
            continue;
        }

        let msg = &messages[idx];
        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let tool_name = msg
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if age < TOOL_RESULT_OLD_COUNT {
            // 中等新鲜度：截断到 MEDIUM_CHARS
            if content.len() > TOOL_RESULT_MEDIUM_CHARS {
                let boundary = content.floor_char_boundary(TOOL_RESULT_MEDIUM_CHARS);
                let compressed = format!(
                    "{}\n\n... [Compressed: showing {TOOL_RESULT_MEDIUM_CHARS}/{} chars]",
                    &content[..boundary],
                    content.len()
                );
                result[idx].insert("content".into(), json!(compressed));
            }
        } else {
            // 很旧的：只保留摘要
            let status = if content.starts_with("[DENIED]") {
                "denied"
            } else if is_error_envelope(content) {
                "error"
            } else {
                "ok"
            };
            let summary = format!(
                "[tool: {tool_name}, {len} chars, {status}]",
                len = content.len(),
            );
            result[idx].insert("content".into(), json!(summary));
        }
    }

    // === Part 2: 压缩旧的 assistant tool_call arguments ===
    // 三层衰减策略（与 tool result 对称）：
    //   - 最近 FRESH_COUNT 条：保留完整 arguments
    //   - FRESH ~ OLD_COUNT 条：截断到 MEDIUM_CHARS 字符（保留足够上下文）
    //   - 超过 OLD_COUNT 条：替换为结构化摘要
    let assistant_tc_indices: Vec<usize> = result
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, m)| {
            m.get("role").and_then(|v| v.as_str()) == Some("assistant")
                && m.get("tool_calls")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();

    for (age, &idx) in assistant_tc_indices.iter().enumerate() {
        if idx < protected_prefix_len {
            continue;
        }
        if idx >= recent_round_cutoff_idx {
            continue;
        }
        if age < TOOL_CALL_ARGS_FRESH_COUNT {
            continue;
        }

        if let Some(tool_calls) = result[idx].get("tool_calls").cloned() {
            if let Some(tcs) = tool_calls.as_array() {
                let compressed_tcs: Vec<Value> = tcs
                    .iter()
                    .map(|tc| {
                        let mut tc = tc.clone();
                        if let Some(func) = tc.get_mut("function") {
                            let tool_name = func
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            if let Some(args_val) = func.get("arguments") {
                                let args_str = args_val.as_str().unwrap_or("");
                                let new_args = if age < TOOL_CALL_ARGS_OLD_COUNT {
                                    // 中等新鲜度：截断到 MEDIUM_CHARS
                                    compress_args_medium(tool_name, args_str)
                                } else {
                                    // 很旧：只保留摘要
                                    compress_args_summary(tool_name, args_str)
                                };
                                if let Some(compressed) = new_args {
                                    if let Some(obj) = func.as_object_mut() {
                                        obj.insert("arguments".into(), json!(compressed));
                                    }
                                }
                            }
                        }
                        tc
                    })
                    .collect();
                result[idx].insert("tool_calls".into(), Value::Array(compressed_tcs));
            }
        }
    }

    result
}

/// 中等新鲜度的 arguments 压缩：截断但保留足够上下文。
/// 对 exec 命令特殊处理：提取代码中的注释和 print 语句作为意图摘要。
pub(crate) fn compress_args_medium(tool_name: &str, args_str: &str) -> Option<String> {
    if args_str.len() <= TOOL_CALL_ARGS_MEDIUM_CHARS {
        return None; // 不需要压缩
    }

    if tool_name == "exec" {
        // 对 exec 命令：提取内联代码的意图摘要，截断到 medium 限制
        if let Some(summary) = extract_exec_intent(args_str, EXEC_INTENT_MEDIUM_MAX_CHARS) {
            return Some(summary);
        }
    }

    // 通用截断：尝试解析为 JSON 后用 serde_json 重新序列化安全摘要
    // 避免截断到 JSON 中间导致不合法
    if let Ok(mut parsed) = serde_json::from_str::<Value>(args_str) {
        // 截断所有超长的字符串字段值
        truncate_json_values(&mut parsed, TOOL_CALL_ARGS_MEDIUM_CHARS);
        return Some(parsed.to_string());
    }
    // JSON 解析失败时，生成安全的摘要
    let tool_summary = format!("[args: {} chars]", args_str.len());
    Some(json!({"_summary": tool_summary}).to_string())
}

/// 很旧的 arguments 压缩：只保留结构化摘要。
pub(crate) fn compress_args_summary(tool_name: &str, args_str: &str) -> Option<String> {
    if args_str.len() <= 200 {
        return None; // 很短的不压缩
    }

    match tool_name {
        "exec" => {
            // 提取 exec 的意图摘要，old 层用更短的限制
            if let Some(summary) = extract_exec_intent(args_str, EXEC_INTENT_OLD_MAX_CHARS) {
                return Some(summary);
            }
            // 使用 serde_json 生成合法 JSON
            Some(json!({"command": format!("[exec: {} chars]", args_str.len())}).to_string())
        }
        "write_file" => {
            // 提取文件路径
            let path = extract_json_field(args_str, "path").unwrap_or("?".into());
            Some(
                json!({"path": path, "content": format!("[written: {} chars]", args_str.len())})
                    .to_string(),
            )
        }
        _ => Some(
            json!({"_summary": format!("[{tool_name}: {} chars]", args_str.len())}).to_string(),
        ),
    }
}

/// 从 exec 的 arguments 中提取代码意图摘要。
///
/// 保留：注释行(#)、print() 语句、import 语句、关键赋值。
/// 这些足以让 LLM 理解"之前做了什么"，而无需看到完整代码。
/// `max_intent_chars` 控制 intent 部分的最大字符数，确保不同衰减层有不同精简度。
pub(crate) fn extract_exec_intent(args_str: &str, max_intent_chars: usize) -> Option<String> {
    // 提取 command 字段内容
    let cmd = extract_json_field(args_str, "command")?;

    // 提取有意义的行（注释、print、import）
    let mut intent_lines: Vec<&str> = Vec::new();
    let mut prefix = "";

    for line in cmd.lines() {
        let trimmed = line.trim();
        // 保留 cd 前缀
        if trimmed.starts_with("cd ") && trimmed.contains("&&") {
            prefix = trimmed;
            continue;
        }
        // 保留注释、print、import
        if trimmed.starts_with('#')
            || trimmed.starts_with("print(")
            || trimmed.starts_with("print (")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
        {
            intent_lines.push(trimmed);
        }
    }

    if intent_lines.is_empty() {
        // 没有注释/print，退回到前 N 字符截断
        return None;
    }

    // 拼接 intent 并截断到 max_intent_chars
    let mut intent = intent_lines.join(" | ");
    if intent.chars().count() > max_intent_chars {
        let boundary = intent
            .char_indices()
            .nth(max_intent_chars)
            .map(|(i, _)| i)
            .unwrap_or(intent.len());
        intent.truncate(boundary);
        intent.push_str("...");
    }

    let summary_text = if !prefix.is_empty() {
        format!(
            "[{prefix} | python script ({} chars) intent: {intent}]",
            cmd.len()
        )
    } else {
        format!("[python script ({} chars) intent: {intent}]", cmd.len())
    };

    // 使用 serde_json 确保生成合法 JSON（自动转义引号、反斜杠等）
    Some(json!({"command": summary_text}).to_string())
}

/// 递归截断 JSON 值中超长的字符串字段。
pub(crate) fn truncate_json_values(value: &mut Value, max_chars: usize) {
    match value {
        Value::String(s) => {
            if s.len() > max_chars {
                let boundary = s.floor_char_boundary(max_chars);
                let original_len = s.len();
                s.truncate(boundary);
                s.push_str(&format!("... [truncated: {max_chars}/{original_len}]"));
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                truncate_json_values(v, max_chars);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                truncate_json_values(v, max_chars);
            }
        }
        _ => {}
    }
}

/// 从 JSON 字符串中提取指定字段的值（简单解析，不依赖完整 JSON parse）。
pub(crate) fn extract_json_field(json_str: &str, field: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", field);
    let start = json_str.find(&pattern)?;
    let value_start = start + pattern.len();
    // 找到未转义的结束引号：需要计算引号前连续反斜杠的数量，
    // 偶数个反斜杠表示引号未被转义（反斜杠自身被转义），奇数个表示引号被转义。
    let mut i = value_start;
    let bytes = json_str.as_bytes();
    loop {
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'"' {
            // 计算引号前连续反斜杠的数量
            let mut num_backslashes = 0;
            let mut j = i;
            while j > value_start && bytes[j - 1] == b'\\' {
                num_backslashes += 1;
                j -= 1;
            }
            // 偶数个反斜杠 → 引号未被转义，是真正的结束引号
            if num_backslashes % 2 == 0 {
                break;
            }
        }
        i += 1;
    }
    let raw = &json_str[value_start..i];
    // 基本反转义
    let unescaped = raw
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\");
    Some(unescaped)
}

// ============================================================================
// Prompt Token Compression：工具 schema 精简表示（R11.1 / Property 30）
// ============================================================================
//
// 提供 `compact` 模式的工具定义：在保留工具名与参数结构（properties / type /
// enum / required）的前提下，去除冗长的 description 文本与示例（examples），
// 从而降低发送给 LLM 的 token 量。
//
// **精简的确定性保证（供 task 15.2 Property 30 依赖）**：
// `compact_tool_schema` / `compact_tool_definitions` 只会"删减"内容，绝不新增：
//   - 顶层与参数内的 `description` 被截断为原文的子串（截断/裁剪到示例标记之前
//     并限制最大字符数），其字符数恒 ≤ 原文；
//   - 参数对象内的 `examples` / `example` 键被整体移除；
//   - 工具名、参数 schema 结构（properties 键、type、enum、required 等）原样保留。
// 因此精简后序列化 JSON 的字符数恒 ≤ 原始，且当存在任一可精简内容（超长
// description 或 examples）时**严格小于**原始。由于估算 token 量随文本内容单调，
// 故：
//   - 对**任意**工具定义集合：`estimate_tool_defs_tokens(compact) ≤ 原始`；
//   - 当集合中**至少一个**工具含可精简的冗长内容时：严格 `<`。
// Property 30 的 proptest 生成器需保证生成含冗长 description/examples 的工具，
// 以触发严格 `<`。

/// compact 模式下顶层工具 description 的最大保留字符数。
const COMPACT_DESC_MAX_CHARS: usize = 100;

/// compact 模式下单个参数 description 的最大保留字符数。
const COMPACT_PARAM_DESC_MAX_CHARS: usize = 60;

/// 估算一组工具定义（OpenAI function-calling 格式）的 token 量。
///
/// 复用 `tyclaw-types` 的 tiktoken 估算器（与 prompt token 统计口径一致）。
pub fn estimate_tool_defs_tokens(defs: &[Value]) -> usize {
    tyclaw_types::tokens::estimate_prompt_tokens(&[], Some(defs))
}

/// 将一组工具定义转换为 compact（精简）模式。
///
/// 保留工具名与参数结构，去除冗长 description 与示例。详见模块文档中的
/// "精简的确定性保证"。
pub fn compact_tool_definitions(defs: &[Value]) -> Vec<Value> {
    defs.iter().map(compact_tool_schema).collect()
}

/// 将单个工具定义转换为 compact（精简）模式。
///
/// 输入应为 OpenAI function-calling 格式：
/// `{ "type": "function", "function": { "name", "description", "parameters" } }`。
/// 非该结构的输入将原样返回（不报错）。
pub fn compact_tool_schema(tool_def: &Value) -> Value {
    let mut def = tool_def.clone();
    if let Some(func) = def.get_mut("function").and_then(|f| f.as_object_mut()) {
        // 精简顶层 description
        if let Some(Value::String(d)) = func.get("description") {
            let compacted = compact_description(d, COMPACT_DESC_MAX_CHARS);
            func.insert("description".into(), Value::String(compacted));
        }
        // 精简参数 schema 中的 description / examples
        if let Some(params) = func.get_mut("parameters") {
            strip_param_verbosity(params);
        }
    }
    def
}

/// 递归去除参数 schema 中的冗长内容：
/// - 移除 `examples` / `example` 键；
/// - 将 `description` 字符串截断到 `COMPACT_PARAM_DESC_MAX_CHARS`（为空则移除该键）；
/// - 其余结构（properties / type / enum / required 等）原样保留。
fn strip_param_verbosity(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("examples");
            map.remove("example");
            if let Some(Value::String(d)) = map.get("description") {
                let compacted = compact_description(d, COMPACT_PARAM_DESC_MAX_CHARS);
                if compacted.is_empty() {
                    map.remove("description");
                } else {
                    map.insert("description".into(), Value::String(compacted));
                }
            }
            for v in map.values_mut() {
                strip_param_verbosity(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_param_verbosity(v);
            }
        }
        _ => {}
    }
}

/// 将一段 description 精简为原文的子串：
/// 1) 截断到首个示例/段落标记（"example"/"e.g."/"示例"/空行）之前；
/// 2) 再裁剪到 `max_chars` 个字符以内。
///
/// 返回值恒为原文的前缀子串（去除尾部空白），故字符数 ≤ 原文。
fn compact_description(desc: &str, max_chars: usize) -> String {
    // 寻找最早的"示例/段落"标记字节位置（均落在 char 边界上）
    let mut cut = desc.len();
    for marker in ["example", "e.g.", "for example"] {
        if let Some(idx) = find_ascii_ci(desc, marker) {
            cut = cut.min(idx);
        }
    }
    for marker in ["示例", "\n\n"] {
        if let Some(idx) = desc.find(marker) {
            cut = cut.min(idx);
        }
    }

    let head = desc[..cut].trim_end();

    // 裁剪到 max_chars 个字符
    if head.chars().count() > max_chars {
        let boundary = head
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(head.len());
        head[..boundary].trim_end().to_string()
    } else {
        head.to_string()
    }
}

/// ASCII 大小写不敏感的子串查找，返回 needle 在 haystack 中首次出现的字节下标。
///
/// 仅用于 ASCII needle；命中位置必然落在 char 边界（ASCII 字节不会匹配多字节
/// UTF-8 序列的延续字节），可安全用于字符串切片。
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    'outer: for i in 0..=(h.len() - n.len()) {
        for j in 0..n.len() {
            if !h[i + j].eq_ignore_ascii_case(&n[j]) {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

#[cfg(test)]
mod compact_schema_tests {
    use super::*;
    use serde_json::json;

    fn verbose_tool() -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file from disk. This tool is extremely \
                    useful and you should use it whenever you need to inspect a file. \
                    Examples: read_file(path=\"a.txt\"); read_file(path=\"b.txt\", start=1).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "The absolute path to the file to read. This must be \
                                a valid path that exists on the filesystem, otherwise an error \
                                is returned and you should retry with a corrected path.",
                            "examples": ["/tmp/a.txt", "/var/log/b.log"]
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "Optional starting line number, 1-indexed."
                        }
                    },
                    "required": ["path"]
                }
            }
        })
    }

    #[test]
    fn compact_reduces_tokens_for_verbose_tool() {
        let original = vec![verbose_tool()];
        let compact = compact_tool_definitions(&original);
        let before = estimate_tool_defs_tokens(&original);
        let after = estimate_tool_defs_tokens(&compact);
        assert!(
            after < before,
            "compact tokens ({after}) should be strictly less than original ({before})"
        );
    }

    #[test]
    fn compact_preserves_name_and_param_structure() {
        let compact = compact_tool_schema(&verbose_tool());
        let func = &compact["function"];
        assert_eq!(func["name"], json!("read_file"));
        // 参数结构保留
        let params = &func["parameters"];
        assert_eq!(params["type"], json!("object"));
        assert!(params["properties"]["path"].get("type").is_some());
        assert_eq!(params["properties"]["path"]["type"], json!("string"));
        assert_eq!(params["required"], json!(["path"]));
        // examples 被移除
        assert!(params["properties"]["path"].get("examples").is_none());
        // 顶层 description 被精简（不再包含 Examples 段）
        let desc = func["description"].as_str().unwrap_or("");
        assert!(!desc.to_lowercase().contains("example"));
    }

    #[test]
    fn compact_never_grows_for_minimal_tool() {
        // 已经很精简的工具：token 量不增加（相等或更小）
        let minimal = vec![json!({
            "type": "function",
            "function": {
                "name": "noop",
                "description": "Do nothing.",
                "parameters": { "type": "object", "properties": {} }
            }
        })];
        let compact = compact_tool_definitions(&minimal);
        let before = estimate_tool_defs_tokens(&minimal);
        let after = estimate_tool_defs_tokens(&compact);
        assert!(after <= before, "compact ({after}) must not exceed original ({before})");
    }

    #[test]
    fn compact_handles_non_function_value_gracefully() {
        let weird = json!({"foo": "bar"});
        let out = compact_tool_schema(&weird);
        assert_eq!(out, weird);
    }

    use proptest::prelude::*;

    /// 生成一个含冗长 description 与 examples 的工具定义。
    ///
    /// 为保证 Property 30 的严格 `<` 成立，每个工具都包含：
    /// - 顶层超长 description（> 200 字符，并带 "Examples:" 段）；
    /// - 若干参数，每个参数都带超长 description 以及 `examples` 数组。
    fn verbose_tool_strategy() -> impl Strategy<Value = serde_json::Value> {
        (
            "[a-z][a-z_]{0,15}",                 // 工具名
            "[a-zA-Z ]{40,80}",                  // description 基础短语（会被重复放大）
            prop::collection::vec("[a-z][a-z_]{0,10}", 1..=4), // 参数名
        )
            .prop_map(|(name, phrase, param_names)| {
                // 放大成超长 description（远超 100 字符顶层预算），并附带 Examples 段。
                let long_desc = format!(
                    "{phrase}. {phrase}. {phrase}. Examples: {name}(x=1); {name}(x=2); {name}(x=3)."
                );
                let mut properties = serde_json::Map::new();
                for (i, pname) in param_names.iter().enumerate() {
                    // 去重参数名，避免 Map 覆盖导致参数数量不稳定。
                    let key = format!("{pname}_{i}");
                    let param_desc = format!(
                        "{phrase} {phrase} (param {key}). \
                         This is a deliberately verbose parameter description exceeding the budget."
                    );
                    properties.insert(
                        key,
                        json!({
                            "type": "string",
                            "description": param_desc,
                            "examples": ["alpha-example-value", "beta-example-value", "gamma"]
                        }),
                    );
                }
                json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": long_desc,
                        "parameters": {
                            "type": "object",
                            "properties": serde_json::Value::Object(properties),
                            "required": []
                        }
                    }
                })
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 30: 精简工具 schema 的 token 量低于原始
        // Validates: Requirements 11.1
        #[test]
        fn prop_compact_schema_strictly_fewer_tokens(
            defs in prop::collection::vec(verbose_tool_strategy(), 1..=5),
        ) {
            let compact = compact_tool_definitions(&defs);
            let before = estimate_tool_defs_tokens(&defs);
            let after = estimate_tool_defs_tokens(&compact);
            prop_assert!(
                after < before,
                "compact tokens ({after}) must be strictly less than original ({before})"
            );
        }
    }
}

// ============================================================================
// Prompt Token 占比观测指标（R11.3 / task 15.7）
// ============================================================================
//
// 每次请求记录 prompt 各组成部分的 token 量：history（历史消息）、cases
// （相似/置顶案例正文）、skills（技能注入正文）、tool 定义（function-calling
// schema）。通过 `compute_token_breakdown` 计算后，`emit_token_breakdown` 以
// 结构化 `tracing::info!` 事件写入可观测性管线，供 progress/审计指标采集。
//
// token 口径与 prompt 统计完全一致——全部复用 `tyclaw_types::tokens` 的
// tiktoken 估算器（与 `estimate_tool_defs_tokens` 同源），不另造估算逻辑。

/// 单次请求 prompt 各部分的 token 占用明细。
///
/// 字段单位均为 token 数，由 tiktoken cl100k_base 估算得到。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PromptTokenBreakdown {
    /// 历史消息（含工具调用/结果）贡献的 token。
    pub history: usize,
    /// 相似/置顶案例正文贡献的 token。
    pub cases: usize,
    /// 技能注入正文贡献的 token。
    pub skills: usize,
    /// 工具定义（function-calling schema）贡献的 token。
    pub tool_defs: usize,
}

/// 各部分 token 占总量的比例（0.0..=1.0）。总量为 0 时各项均为 0.0。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromptTokenFractions {
    pub history: f64,
    pub cases: f64,
    pub skills: f64,
    pub tool_defs: f64,
}

impl PromptTokenBreakdown {
    /// 各部分 token 之和。
    pub fn total(&self) -> usize {
        self.history + self.cases + self.skills + self.tool_defs
    }

    /// 各部分占总量的比例；总量为 0 时返回全 0，避免除零。
    pub fn fractions(&self) -> PromptTokenFractions {
        let total = self.total();
        if total == 0 {
            return PromptTokenFractions {
                history: 0.0,
                cases: 0.0,
                skills: 0.0,
                tool_defs: 0.0,
            };
        }
        let total = total as f64;
        PromptTokenFractions {
            history: self.history as f64 / total,
            cases: self.cases as f64 / total,
            skills: self.skills as f64 / total,
            tool_defs: self.tool_defs as f64 / total,
        }
    }
}

/// 估算一段纯文本（案例 / 技能正文）的 token 量。
///
/// 复用 prompt 口径的 tiktoken 估算器：将文本包成单条 `content` 消息后计数。
/// 空文本计 0（不占 token）。
fn estimate_text_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut msg: HashMap<String, Value> = HashMap::new();
    msg.insert("content".to_string(), Value::String(text.to_string()));
    tyclaw_types::tokens::estimate_prompt_tokens(std::slice::from_ref(&msg), None)
}

/// 计算单次请求 prompt 各部分的 token 占用明细（R11.3）。
///
/// 输入按 prompt 装配路径的天然表示选取：
/// - `history`：历史消息列表（OpenAI message map），复用
///   [`tyclaw_types::tokens::estimate_prompt_tokens`]；
/// - `cases_text` / `skills_text`：已渲染的案例 / 技能正文文本；
/// - `tool_defs`：工具定义（function-calling schema），复用
///   [`estimate_tool_defs_tokens`]。
pub fn compute_token_breakdown(
    history: &[HashMap<String, Value>],
    cases_text: &str,
    skills_text: &str,
    tool_defs: &[Value],
) -> PromptTokenBreakdown {
    // 空工具列表序列化为 "[]" 会被估算器计为 1 token；占比指标语义上应记 0，
    // 与空 cases/skills 文本保持一致。
    let tool_defs_tokens = if tool_defs.is_empty() {
        0
    } else {
        estimate_tool_defs_tokens(tool_defs)
    };
    PromptTokenBreakdown {
        history: tyclaw_types::tokens::estimate_prompt_tokens(history, None),
        cases: estimate_text_tokens(cases_text),
        skills: estimate_text_tokens(skills_text),
        tool_defs: tool_defs_tokens,
    }
}

/// 将 token 占比明细写入可观测性管线（结构化 `tracing` 事件）。
///
/// 供 progress/审计指标采集层订阅 `tyclaw::observability::token_breakdown`
/// target 抓取各字段。
pub fn emit_token_breakdown(breakdown: &PromptTokenBreakdown) {
    let total = breakdown.total();
    let frac = breakdown.fractions();
    tracing::info!(
        target: "tyclaw::observability::token_breakdown",
        history_tokens = breakdown.history,
        cases_tokens = breakdown.cases,
        skills_tokens = breakdown.skills,
        tool_defs_tokens = breakdown.tool_defs,
        total_tokens = total,
        history_frac = frac.history,
        cases_frac = frac.cases,
        skills_frac = frac.skills,
        tool_defs_frac = frac.tool_defs,
        "prompt token breakdown"
    );
}

#[cfg(test)]
mod token_breakdown_tests {
    use super::*;
    use serde_json::json;

    fn user_msg(text: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".into(), json!("user"));
        m.insert("content".into(), json!(text));
        m
    }

    fn sample_tool() -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file from disk.",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        })
    }

    #[test]
    fn breakdown_total_sums_all_parts() {
        let history = vec![user_msg("the quick brown fox jumps over the lazy dog")];
        let cases = "previously the agent solved a similar billing reconciliation case";
        let skills = "skill: generate a bar chart with matplotlib from a csv file";
        let tools = vec![sample_tool()];

        let b = compute_token_breakdown(&history, cases, skills, &tools);

        assert_eq!(b.total(), b.history + b.cases + b.skills + b.tool_defs);
    }

    #[test]
    fn each_part_is_counted_when_present() {
        let history = vec![user_msg("hello world this is the conversation history")];
        let cases = "case study text that is reasonably long for token counting";
        let skills = "skill instructions describing how to do the task in detail";
        let tools = vec![sample_tool()];

        let b = compute_token_breakdown(&history, cases, skills, &tools);

        assert!(b.history > 0, "history should be counted");
        assert!(b.cases > 0, "cases should be counted");
        assert!(b.skills > 0, "skills should be counted");
        assert!(b.tool_defs > 0, "tool defs should be counted");
    }

    #[test]
    fn empty_parts_contribute_zero() {
        let b = compute_token_breakdown(&[], "", "", &[]);
        assert_eq!(b.history, 0);
        assert_eq!(b.cases, 0);
        assert_eq!(b.skills, 0);
        assert_eq!(b.tool_defs, 0);
        assert_eq!(b.total(), 0);
    }

    #[test]
    fn fractions_sum_to_one_when_nonempty() {
        let history = vec![user_msg("history content for fraction test, a few tokens here")];
        let cases = "case content for fraction test with several tokens included";
        let skills = "skill content for the fraction test also several tokens";
        let tools = vec![sample_tool()];

        let b = compute_token_breakdown(&history, cases, skills, &tools);
        let f = b.fractions();
        let sum = f.history + f.cases + f.skills + f.tool_defs;
        assert!((sum - 1.0).abs() < 1e-9, "fractions should sum to ~1.0, got {sum}");
    }

    #[test]
    fn fractions_are_zero_for_empty_breakdown() {
        let b = PromptTokenBreakdown::default();
        let f = b.fractions();
        assert_eq!(f.history, 0.0);
        assert_eq!(f.cases, 0.0);
        assert_eq!(f.skills, 0.0);
        assert_eq!(f.tool_defs, 0.0);
    }

    #[test]
    fn emit_does_not_panic() {
        let b = compute_token_breakdown(&[user_msg("hi")], "c", "s", &[sample_tool()]);
        emit_token_breakdown(&b);
    }

    fn assistant_msg(text: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".into(), json!("assistant"));
        m.insert("content".into(), json!(text));
        m
    }

    fn exec_tool() -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "exec",
                "description": "Execute a shell or python command.",
                "parameters": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"]
                }
            }
        })
    }

    // ========================================================================
    // 集成测试（task 15.8）：验证一次代表性请求中各部分 token 占比被记录（R11.3）
    // ========================================================================

    /// 端到端：用一个贴近真实 prompt 装配的请求（多条历史消息 + 非空 cases /
    /// skills + 多个 function-calling 工具定义）走 `compute_token_breakdown`，
    /// 断言四部分均被独立计数、total 等于四部分之和、fractions 归一且各项落在
    /// [0,1]，并确认 `emit_token_breakdown` 能将占比写入观测管线而不 panic。
    #[test]
    fn integration_token_breakdown_records_all_part_shares() {
        // 1) 构建一个代表性请求
        let history = vec![
            user_msg("请帮我对 0521 的账单做一次对账，并统计异常条目数量"),
            assistant_msg("好的，我先读取账单文件并按渠道汇总，然后定位差异项"),
        ];
        let cases_text =
            "历史案例：上次账单对账通过逐行 diff + 渠道分组定位到 3 笔异常，最终人工复核确认";
        let skills_text =
            "技能：使用 pandas 读取 csv、按列分组聚合、并用 matplotlib 输出对账差异柱状图";
        let tool_defs = vec![sample_tool(), exec_tool()];

        let breakdown =
            compute_token_breakdown(&history, cases_text, skills_text, &tool_defs);

        // 2) 各部分都被独立计数（> 0）
        assert!(breakdown.history > 0, "history 部分应被计数");
        assert!(breakdown.cases > 0, "cases 部分应被计数");
        assert!(breakdown.skills > 0, "skills 部分应被计数");
        assert!(breakdown.tool_defs > 0, "tool 定义部分应被计数");

        // 3) total 等于四部分之和
        assert_eq!(
            breakdown.total(),
            breakdown.history + breakdown.cases + breakdown.skills + breakdown.tool_defs,
            "total 应等于各部分 token 之和"
        );

        // 4) 占比归一，且每一项都落在 [0, 1]
        let f = breakdown.fractions();
        for (name, frac) in [
            ("history", f.history),
            ("cases", f.cases),
            ("skills", f.skills),
            ("tool_defs", f.tool_defs),
        ] {
            assert!(
                (0.0..=1.0).contains(&frac),
                "{name} 占比应落在 [0,1]，实际 {frac}"
            );
        }
        let sum = f.history + f.cases + f.skills + f.tool_defs;
        assert!(
            (sum - 1.0).abs() < 1e-9,
            "各部分占比之和应约等于 1.0，实际 {sum}"
        );

        // 5) 占比可被写入观测管线（结构化 tracing 事件），不应 panic
        emit_token_breakdown(&breakdown);
    }
}
