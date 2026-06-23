//! 工具 trait 和风险等级定义。
//!
//! 定义了所有工具必须实现的统一接口，
//! 以及参数类型转换和验证的辅助函数。

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

use tyclaw_tool_abi::Sandbox;

/// 工具操作的风险等级。
///
/// 用于权限控制，决定不同角色能否执行某个工具：
/// - `Read`: 只读操作（如读取文件）—— 所有角色可用
/// - `Write`: 写入操作（如修改文件、执行命令）—— 需要 Member 及以上
/// - `Dangerous`: 危险操作（如删除文件系统）—— 需要 Admin 确认
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    Read,      // 只读
    Write,     // 写入
    Dangerous, // 危险
}

/// RiskLevel 的显示实现，用于日志和权限检查时的字符串比较。
impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RiskLevel::Read => write!(f, "read"),
            RiskLevel::Write => write!(f, "write"),
            RiskLevel::Dangerous => write!(f, "dangerous"),
        }
    }
}

/// 工具抽象接口 —— 所有工具必须实现此 trait。
///
/// `Send + Sync` 约束确保工具可以在异步多线程环境中安全使用。
/// 通过 `async_trait` 支持异步执行方法。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具的唯一名称（如 "read_file"、"exec"），用于工具查找和 LLM 调用。
    fn name(&self) -> &str;

    /// 工具的描述信息，展示给 LLM 帮助其理解工具的用途。
    fn description(&self) -> &str;

    /// 工具参数的 JSON Schema 定义。
    /// 符合 OpenAI function calling 的参数格式规范。
    fn parameters(&self) -> Value;

    /// 工具的风险等级，默认为 Read（只读）。
    /// 子类可覆盖此方法来声明更高的风险等级。
    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Read
    }

    /// 执行工具并返回结果字符串（本地执行路径）。
    ///
    /// 参数以 HashMap<String, Value> 传入，由 LLM 生成。
    /// 返回值为工具执行的文本输出（成功内容或错误信息）。
    async fn execute(&self, params: HashMap<String, Value>) -> String;

    /// 是否应该路由到沙箱执行。
    /// 默认 false，有副作用的工具（exec、write_file 等）应覆盖为 true。
    fn should_sandbox(&self) -> bool {
        false
    }

    /// 在沙箱中执行工具。
    /// 默认回退到本地执行。需要沙箱支持的工具应覆盖此方法。
    async fn execute_in_sandbox(
        &self,
        _sandbox: &dyn Sandbox,
        params: HashMap<String, Value>,
    ) -> String {
        self.execute(params).await
    }

    /// 压缩工具执行结果，减少 LLM token 消耗。
    ///
    /// 各工具可覆盖此方法实现定制压缩策略（参考 RTK 模式）：
    /// - exec：按命令类型压缩（测试输出只保留失败、编译去掉进度行等）
    /// - read_file：去掉注释和空行
    /// - grep：限制每文件匹配数、截断长行
    /// - list_dir：去掉权限/时间戳
    ///
    /// `params` 为原始调用参数，供判断命令类型等上下文信息。
    /// 默认实现：不压缩，原样返回。
    fn compress_output(&self, output: &str, _params: &HashMap<String, Value>) -> String {
        output.to_string()
    }

    /// 给 UI（钉钉卡片思考/工具行）用的简短描述。
    ///
    /// 传入工具的原始调用参数（`tool_call.arguments`），返回人类可读的一行。
    /// 默认返回 `None`，具体工具可按需实现：`exec` 截断命令、`read_file` 只留 basename 等。
    fn brief(&self, _args: &HashMap<String, Value>) -> Option<String> {
        None
    }
}

/// brief 辅助：按字符（非字节）边界截断字符串，UTF-8 安全。
pub fn brief_truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

/// brief 辅助：路径只留最后一段（basename）。
pub fn brief_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// 头尾双段截断：保留头部段 + 尾部段，总字符数 <= `max_chars`，
/// 尾部段 >= `max_chars * tail_ratio`（默认建议 0.25）。
/// 中间插入截断标记，标明被省略的中间字符数。
/// 输入字符数 <= `max_chars` 时原样返回，不附加任何截断标记。
///
/// 说明：
/// - 全程按字符（`char`）而非字节计数，多字节 UTF-8 安全。
/// - `tail_ratio` 会被 clamp 到 `[0.0, 1.0]`，防止尾段长度越界。
/// - 头部段长度为 `max_chars - tail_len`，因此保留头尾字符总数恰为 `max_chars`（<= 上限）。
pub fn truncate_head_tail(text: &str, max_chars: usize, tail_ratio: f64) -> String {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();

    // 未超上限：恒等返回，不加标记（R5.6）。
    if total <= max_chars {
        return text.to_string();
    }

    // 尾段长度 = ceil(max_chars * ratio)，clamp 到 [0, max_chars]，保证 head_len 不下溢。
    let ratio = if tail_ratio.is_finite() {
        tail_ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let tail_len = ((max_chars as f64) * ratio).ceil() as usize;
    let tail_len = tail_len.min(max_chars);
    let head_len = max_chars - tail_len;

    // 被省略的中间字符数 = 原字符数 - 保留头尾段字符数（R5.4）。
    let omitted = total - head_len - tail_len;

    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[total - tail_len..].iter().collect();
    let marker = format!("\n... (truncated, {omitted} chars omitted) ...\n");

    format!("{head}{marker}{tail}")
}

/// 根据 JSON Schema 中声明的属性类型，自动转换参数类型。
///
/// LLM 有时会把数字或布尔值以字符串形式传递（如 "42"、"true"），
/// 此函数负责将这些值转换为 Schema 期望的正确类型：
/// - "integer": 字符串 → i64，浮点数 → i64
/// - "number": 字符串 → f64
/// - "boolean": 字符串 "true"/"1"/"yes" → true，其他 → false
/// - "string": 数字/布尔值 → 字符串表示
pub fn cast_params(params: &mut HashMap<String, Value>, schema: &Value) {
    // 从 Schema 中获取 properties 定义
    let props = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return,
    };

    for (key, prop_schema) in props {
        // 获取期望的类型
        let expected_type = match prop_schema.get("type").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
        };
        // 获取当前值
        let value = match params.get(key) {
            Some(v) => v.clone(),
            None => continue,
        };

        // 根据期望类型进行转换
        let casted = match expected_type {
            "integer" => match &value {
                Value::String(s) => s.parse::<i64>().ok().map(Value::from), // "42" → 42
                Value::Number(n) => n.as_f64().map(|f| Value::from(f as i64)), // 42.0 → 42
                _ => None,
            },
            "number" => match &value {
                Value::String(s) => s.parse::<f64>().ok().map(Value::from), // "3.14" → 3.14
                _ => None,
            },
            "boolean" => match &value {
                Value::String(s) => {
                    let lower = s.to_lowercase();
                    Some(Value::Bool(
                        lower == "true" || lower == "1" || lower == "yes", // "true" → true
                    ))
                }
                _ => None,
            },
            "string" => match &value {
                Value::String(_) => None, // 已经是正确类型，无需转换
                Value::Number(n) => Some(Value::String(n.to_string())), // 42 → "42"
                Value::Bool(b) => Some(Value::String(b.to_string())), // true → "true"
                _ => None,
            },
            _ => None,
        };

        // 如果成功转换，更新参数值
        if let Some(v) = casted {
            params.insert(key.clone(), v);
        }
    }
}

/// 验证必填参数是否都已提供。
///
/// 检查 Schema 中 "required" 数组列出的所有参数名是否在 params 中存在。
/// 如果有缺失的必填参数，返回错误信息；否则返回 None。
pub fn validate_params(params: &HashMap<String, Value>, schema: &Value) -> Option<String> {
    let required = match schema.get("required").and_then(|r| r.as_array()) {
        Some(arr) => arr,
        None => return None, // 没有 required 字段，所有参数都是可选的
    };
    for key in required {
        if let Some(k) = key.as_str() {
            if !params.contains_key(k) {
                return Some(format!("Missing required parameter: {k}"));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    /// 测试：字符串参数转换为整数
    #[test]
    fn test_cast_string_to_int() {
        let schema = json!({
            "properties": { "count": { "type": "integer" } }
        });
        let mut params = HashMap::new();
        params.insert("count".into(), json!("42"));
        cast_params(&mut params, &schema);
        assert_eq!(params["count"], json!(42));
    }

    /// 测试：字符串参数转换为布尔值
    #[test]
    fn test_cast_string_to_bool() {
        let schema = json!({
            "properties": { "flag": { "type": "boolean" } }
        });
        let mut params = HashMap::new();
        params.insert("flag".into(), json!("true"));
        cast_params(&mut params, &schema);
        assert_eq!(params["flag"], json!(true));
    }

    /// 测试：缺少必填参数时返回错误信息
    #[test]
    fn test_validate_missing_required() {
        let schema = json!({
            "required": ["path", "content"]
        });
        let mut params = HashMap::new();
        params.insert("path".into(), json!("/tmp/test"));
        let err = validate_params(&params, &schema);
        assert_eq!(err, Some("Missing required parameter: content".into()));
    }

    /// 测试：所有必填参数都存在时返回 None
    #[test]
    fn test_validate_all_present() {
        let schema = json!({
            "required": ["path"]
        });
        let mut params = HashMap::new();
        params.insert("path".into(), json!("/tmp/test"));
        assert_eq!(validate_params(&params, &schema), None);
    }

    /// 测试：未超上限时 truncate_head_tail 为恒等操作（R5.6）
    #[test]
    fn test_truncate_head_tail_identity_under_limit() {
        let s = "hello world";
        assert_eq!(truncate_head_tail(s, 100, 0.25), s);
        // 恰好等于上限也应原样返回
        assert_eq!(truncate_head_tail(s, s.chars().count(), 0.25), s);
        assert!(!truncate_head_tail(s, 100, 0.25).contains("truncated"));
    }

    /// 测试：超上限时保留头尾两段、总量受限且尾段达比例（R5.3）
    #[test]
    fn test_truncate_head_tail_keeps_head_and_tail() {
        let s: String = "abcdefghij".repeat(10); // 100 chars
        let out = truncate_head_tail(&s, 40, 0.25);
        // 头部以原文开头、尾部以原文结尾
        assert!(out.starts_with("abcde"));
        assert!(out.ends_with("ghij"));
        assert!(out.contains("truncated"));
        // 头(30) + 尾(10) = 40，尾段 >= 40 * 0.25 = 10
        let kept: usize = 30 + 10;
        let omitted = 100 - kept;
        assert!(out.contains(&format!("{omitted} chars omitted")));
    }

    /// 测试：截断标记标明正确的省略字符数（R5.4）
    #[test]
    fn test_truncate_head_tail_omitted_count() {
        let s: String = "x".repeat(50);
        let max = 20;
        let out = truncate_head_tail(&s, max, 0.25);
        // tail_len = ceil(20 * 0.25) = 5, head_len = 15, omitted = 50 - 20 = 30
        assert!(out.contains("30 chars omitted"));
    }

    /// 测试：多字节 UTF-8 不会触发边界 panic 且按字符计数（R5.3）
    #[test]
    fn test_truncate_head_tail_utf8_safe() {
        let s: String = "建".repeat(40); // 40 个多字节字符
        let out = truncate_head_tail(&s, 16, 0.25);
        assert!(out.contains("truncated"));
        // tail_len = ceil(16*0.25)=4, head_len=12, omitted = 40-16 = 24
        assert!(out.contains("24 chars omitted"));
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 19: 头尾双段截断满足总量与尾段比例约束
        #[test]
        fn prop_truncate_head_tail_total_and_tail_ratio(
            // 字符集合含多字节 UTF-8（ASCII、中文、emoji），构造任意文本。
            text in proptest::collection::vec(
                prop_oneof![
                    proptest::char::range('a', 'z'),
                    Just('建'),
                    Just('字'),
                    Just('🚀'),
                    Just('\n'),
                ],
                0..16000usize,
            ).prop_map(|cs| cs.into_iter().collect::<String>()),
            max_chars in 8000usize..=12000usize,
        ) {
            let tail_ratio = 0.25_f64;
            let out = truncate_head_tail(&text, max_chars, tail_ratio);
            let total = text.chars().count();

            // 仅在超过上限、实际触发截断时检查约束。
            if total > max_chars {
                // 复现函数内部分段逻辑：尾段 = ceil(max*ratio)，头段 = max - 尾段。
                let tail_len = ((max_chars as f64) * tail_ratio).ceil() as usize;
                let tail_len = tail_len.min(max_chars);
                let head_len = max_chars - tail_len;
                let omitted = total - head_len - tail_len;

                // 从输出中剥离截断标记，余下即为保留的头尾两段。
                let marker = format!("\n... (truncated, {omitted} chars omitted) ...\n");
                let stripped = out.replacen(&marker, "", 1);
                let kept = stripped.chars().count();

                // 约束 1：保留总量（不含标记）<= 上限。
                prop_assert_eq!(kept, head_len + tail_len);
                prop_assert!(kept <= max_chars);

                // 约束 2：尾段字符数 >= max_chars * 0.25。
                let min_tail = (max_chars as f64 * tail_ratio).floor() as usize;
                prop_assert!(tail_len >= min_tail);
                prop_assert!((tail_len as f64) >= max_chars as f64 * tail_ratio - 1.0);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 20: 截断标记标明正确的省略字符数
        #[test]
        fn prop_truncate_head_tail_omitted_count(
            // 字符集合含多字节 UTF-8（ASCII、中文、emoji），构造任意文本。
            text in proptest::collection::vec(
                prop_oneof![
                    proptest::char::range('a', 'z'),
                    Just('建'),
                    Just('字'),
                    Just('🚀'),
                    Just('\n'),
                ],
                0..16000usize,
            ).prop_map(|cs| cs.into_iter().collect::<String>()),
            max_chars in 8000usize..=12000usize,
        ) {
            let tail_ratio = 0.25_f64;
            let out = truncate_head_tail(&text, max_chars, tail_ratio);
            let total = text.chars().count();

            // 仅在超过上限、实际触发截断时检查省略字符数标记。
            if total > max_chars {
                // 复现函数内部分段逻辑：尾段 = ceil(max*ratio)，头段 = max - 尾段。
                let tail_len = ((max_chars as f64) * tail_ratio).ceil() as usize;
                let tail_len = tail_len.min(max_chars);
                let head_len = max_chars - tail_len;

                // 省略中间字符数 = 原字符数 - 保留头尾段字符数（R5.4）。
                let omitted = total - (head_len + tail_len);

                // 输出须在头尾段间插入截断标记，并标明精确的省略字符数。
                let expected_marker = format!("\n... (truncated, {omitted} chars omitted) ...\n");
                prop_assert!(
                    out.contains(&expected_marker),
                    "output missing expected truncation marker: {expected_marker:?}"
                );
                // 标记标明的省略数等于 (原字符数 - 保留头尾段字符数)。
                let omitted_substr = format!("{omitted} chars omitted");
                prop_assert!(out.contains(&omitted_substr));
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 21: 未超上限时截断为恒等操作
        #[test]
        fn prop_truncate_head_tail_identity_under_limit(
            // 字符集合含多字节 UTF-8（ASCII、中文、emoji），构造任意文本。
            text in proptest::collection::vec(
                prop_oneof![
                    proptest::char::range('a', 'z'),
                    Just('建'),
                    Just('字'),
                    Just('🚀'),
                    Just('\n'),
                ],
                0..2000usize,
            ).prop_map(|cs| cs.into_iter().collect::<String>()),
            // 额外余量 0..=500，确保 max_chars >= 字符数（未超上限）。
            slack in 0usize..=500usize,
        ) {
            let char_count = text.chars().count();
            let max_chars = char_count + slack;

            let out = truncate_head_tail(&text, max_chars, 0.25);

            // 未超上限：输出与输入逐字符相等（恒等操作）。
            prop_assert_eq!(&out, &text);
            // 未超上限：不附加任何截断标记。
            prop_assert!(!out.contains("truncated"));
        }
    }
}
