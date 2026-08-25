//! Shell 命令执行工具 —— 带安全防护机制。
//!
//! 在沙箱化的 shell 环境中执行命令，并提供：
//! - 危险命令拦截（如 rm -rf、格式化磁盘、fork 炸弹等）
//! - 执行超时控制（默认60秒）
//! - 输出长度限制（头尾双段截断，上限可配置，默认 20000 字符）

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command;

use tyclaw_tool_abi::Sandbox;

use crate::base::{brief_truncate, truncate_head_tail, RiskLevel, Tool};
use crate::truncation::current_truncation_limits;

/// 命令执行的默认超时时间（秒）。
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const LONG_TIMER_JOB_IDS: &[&str] = &["7f088316", "addad09e", "c9cd54b0"];

fn effective_timeout_secs(job: Option<&str>, cmd: &str, requested: Option<u64>, default: u64) -> u64 {
    let lower = cmd.to_ascii_lowercase();
    match job {
        Some("52bb6d84" | "faa28743") => 120,
        Some("7f088316" | "addad09e") if lower.contains("run_payment_report.py") => 2700,
        Some("c9cd54b0") if lower.contains("run_ar_report.py") => 7200,
        Some("c9cd54b0") if lower.contains("revenue_qa.py") => 600,
        _ => requested.unwrap_or(default),
    }
}

fn validate_timer_command(job: Option<&str>, command: &str) -> Result<(), &'static str> {
    if !job.is_some_and(|id| LONG_TIMER_JOB_IDS.contains(&id)) { return Ok(()); }
    let lower = command.to_ascii_lowercase();
    let known = ["run_payment_report.py", "run_ar_report.py", "revenue_qa.py"]
        .iter().any(|s| lower.contains(s));
    let forbidden_word = |word: &str| Regex::new(&format!(r"(?:^|[\s;&|]){}(?:$|[\s;&|])", word))
        .is_ok_and(|r| r.is_match(&lower));
    let bytes = command.as_bytes();
    let background = bytes.iter().enumerate().any(|(i, byte)| {
        *byte == b'&' && i.checked_sub(1).and_then(|n| bytes.get(n)) != Some(&b'>')
    });
    let shell_chain = command.contains(';') || command.contains('|')
        || command.contains("&&") || background;
    let direct = if known {
        let executable = command.split_whitespace()
            .find(|part| !part.contains('='))
            .unwrap_or_default().trim_matches(['\'', '"']);
        let executable = std::path::Path::new(executable).file_name()
            .and_then(|name| name.to_str()).unwrap_or_default();
        executable == "python" || executable.starts_with("python3")
    } else { true };
    if forbidden_word("setsid") || forbidden_word("nohup")
        || forbidden_word("sleep") || forbidden_word("ps") || forbidden_word("tail")
        || (known && (shell_chain || !direct)) {
        return Err("该定时任务必须使用单次前台 exec：禁止 setsid、nohup、后台 &、命令串联以及 sleep/ps/tail 轮询");
    }
    Ok(())
}

/// 危险命令的正则匹配模式列表。
///
/// 匹配到以下任一模式的命令将被拦截：
/// - `rm -rf` / `rm -fr`: 递归强制删除
/// - `format`: 格式化磁盘
/// - `mkfs` / `diskpart`: 磁盘分区和文件系统操作
/// - `dd if=`: 磁盘镜像写入（可能覆盖磁盘）
/// - `shutdown` / `reboot` / `poweroff`: 系统关机/重启
/// - `:() { :|:& };:`: fork 炸弹（会耗尽系统资源）
static DENY_PATTERNS: &[&str] = &[
    r"\brm\s+-[rf]*r[rf]*\b", // 只拦截 rm -r / rm -rf / rm -fr，不拦截 rm -f（单文件删除）
    r"(?:^|[;&|]\s*)format\b",
    r"\b(mkfs|diskpart)\b",
    r"\bdd\s+if=",
    r"\b(shutdown|reboot|poweroff)\b",
    r":\(\)\s*\{.*\};\s*:",       // fork 炸弹
    r"\brm\s+-[rf]*r[rf]*\s+/\b", // rm -rf / 根目录
    r">\s*/dev/[sh]d",             // 覆写磁盘设备
    r"\bchmod\s+-R\s+777\s+/\b",  // 递归 chmod 根目录
];

/// Shell 命令执行工具。
///
/// 通过 `sh -c` 执行用户指定的命令，支持管道、重定向等 shell 特性。
pub struct ExecTool {
    timeout_secs: u64,           // 超时时间
    working_dir: Option<String>, // 工作目录
    deny_patterns: Vec<Regex>,   // 编译后的危险命令模式
}

impl ExecTool {
    /// 创建新的 ExecTool 实例。
    ///
    /// - `working_dir`: 命令执行的工作目录（None 则使用当前目录）
    /// - `timeout_secs`: 超时时间（None 则使用默认60秒）
    pub fn new(working_dir: Option<String>, timeout_secs: Option<u64>) -> Self {
        // 预编译所有危险命令正则表达式
        let deny_patterns = DENY_PATTERNS
            .iter()
            .filter_map(|p| Regex::new(p).ok())
            .collect();
        Self {
            timeout_secs: timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
            working_dir,
            deny_patterns,
        }
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return stdout/stderr. Use for: compilation (cargo build/test), \
         running scripts, git commands, package management, or other shell-native tasks. \
         Prefer dedicated tools whenever one exists, and keep commands narrowly scoped to the task. \
         Do NOT use for: reading files (use read_file), writing files (use write_file), \
         editing files (use edit_file/apply_patch), searching content (use grep_search), \
         or listing dirs / finding files (use list_dir or glob)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "working_directory": { "type": "string", "description": "Working directory for command execution. Relative paths resolved against workspace root. Default: workspace root." },
                "timeout": { "type": "integer", "description": "Command timeout in seconds. Default: 120. Increase for long-running builds." }
            },
            "required": ["command"]
        })
    }

    /// 命令执行的风险等级为 Write
    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Write
    }

    fn should_sandbox(&self) -> bool {
        true
    }

    /// 按命令类型压缩 exec 输出（借鉴 RTK 策略）。
    fn compress_output(&self, output: &str, params: &HashMap<String, Value>) -> String {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        compress_exec_output(output, command)
    }

    fn brief(&self, args: &HashMap<String, Value>) -> Option<String> {
        let cmd = args.get("command").and_then(|v| v.as_str())?;
        Some(format!("exec: {}", brief_truncate(cmd, 120)))
    }

    async fn execute_in_sandbox(
        &self,
        sandbox: &dyn Sandbox,
        params: HashMap<String, Value>,
    ) -> String {
        let command = match params.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: Missing 'command' parameter".into(),
        };
        let timer_job_id = crate::timer::current_timer_job_id();
        if let Err(e) = validate_timer_command(timer_job_id.as_deref(), command) { return format!("Error: {e}"); }
        let lower = command.trim().to_lowercase();
        for pattern in &self.deny_patterns {
            if pattern.is_match(&lower) {
                tracing::warn!(pattern = %pattern, "Command blocked by deny pattern");
                return "Error: Command blocked by safety guard".into();
            }
        }
        let timeout_secs = effective_timeout_secs(timer_job_id.as_deref(), command,
            params.get("timeout").and_then(|v| v.as_u64()), self.timeout_secs);
        match sandbox
            .exec_with_context(command, std::time::Duration::from_secs(timeout_secs),
                tyclaw_tool_abi::SandboxExecContext { timer_job_id, cancellation: tyclaw_tool_abi::current_exec_cancel_token() })
            .await
        {
            Ok(r) => {
                let text = r.to_tool_output();
                let limits = current_truncation_limits();
                truncate_head_tail(&text, limits.exec_truncate_chars, limits.tail_ratio)
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    async fn execute(&self, params: HashMap<String, Value>) -> String {
        let command = match params.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: Missing 'command' parameter".into(),
        };
        let timer_job_id = crate::timer::current_timer_job_id();
        if let Err(e) = validate_timer_command(timer_job_id.as_deref(), command) { return format!("Error: {e}"); }

        // 安全检查：匹配危险命令模式
        let lower = command.trim().to_lowercase();
        for pattern in &self.deny_patterns {
            if pattern.is_match(&lower) {
                tracing::warn!(pattern = %pattern, cmd_len = lower.len(), cmd_prefix = %&lower[..lower.len().min(200)], "Command blocked by deny pattern");
                return "Error: Command blocked by safety guard".into();
            }
        }

        let timeout_secs = effective_timeout_secs(timer_job_id.as_deref(), command,
            params.get("timeout").and_then(|v| v.as_u64()), self.timeout_secs);

        // Host 执行
        let cwd = if let Some(wd) = params.get("working_directory").and_then(|v| v.as_str()) {
            if std::path::Path::new(wd).is_absolute() {
                wd.to_string()
            } else {
                let base = self.working_dir.as_deref().unwrap_or(".");
                std::path::Path::new(base)
                    .join(wd)
                    .to_string_lossy()
                    .to_string()
            }
        } else {
            self.working_dir.as_deref().unwrap_or(".").to_string()
        };

        let mut process = Command::new("sh");
        process.arg("-c").arg(command).current_dir(&cwd)
            .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(unix)] process.process_group(0);
        let child = match process.spawn() {
            Ok(child) => child,
            Err(e) => return format!("Error executing command: {e}"),
        };
        let pgid = child.id();
        let output = child.wait_with_output();
        tokio::pin!(output);
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(deadline);
        enum HostFinish { Output(std::io::Result<std::process::Output>), Timeout, Cancelled }
        let result = if let Some(token) = tyclaw_tool_abi::current_exec_cancel_token() {
            tokio::select! { r = &mut output => HostFinish::Output(r), _ = &mut deadline => HostFinish::Timeout, _ = token.cancelled() => HostFinish::Cancelled }
        } else {
            tokio::select! { r = &mut output => HostFinish::Output(r), _ = &mut deadline => HostFinish::Timeout }
        };

        match result {
            HostFinish::Timeout | HostFinish::Cancelled => {
                #[cfg(unix)] if let Some(pgid) = pgid {
                    let group = format!("-{pgid}");
                    let _ = Command::new("kill").args(["-TERM", "--", &group]).output().await;
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let _ = Command::new("kill").args(["-KILL", "--", &group]).output().await;
                }
                if matches!(result, HostFinish::Cancelled) { "Error: Command cancelled".into() }
                else { format!("Error: Command timed out after {timeout_secs} seconds") }
            }
            // 执行失败（如找不到 sh）
            HostFinish::Output(Err(e)) => format!("Error executing command: {e}"),
            // 执行成功（可能有非0退出码）
            HostFinish::Output(Ok(output)) => {
                let mut parts = Vec::new();
                // 收集标准输出
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.is_empty() {
                    parts.push(stdout.to_string());
                }
                // 收集标准错误
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    parts.push(format!("STDERR:\n{stderr}"));
                }
                // 非0退出码时附加退出码信息
                if !output.status.success() {
                    let code = output.status.code().unwrap_or(-1);
                    parts.push(format!("\nExit code: {code}"));
                }
                let text = if parts.is_empty() {
                    "(no output)".to_string()
                } else {
                    parts.join("\n")
                };
                // 输出截断（头尾双段保留，按字符数避免 UTF-8 边界 panic，上限可配置 R5）
                let limits = current_truncation_limits();
                truncate_head_tail(&text, limits.exec_truncate_chars, limits.tail_ratio)
            }
        }
    }
}

/// 按命令类型压缩 exec 输出（借鉴 RTK 策略）。
///
/// 识别常见命令模式，去掉冗余信息，只保留关键内容：
/// - 测试输出：去掉 pass 行，只保留失败 + 摘要
/// - 编译输出：去掉进度行（Compiling/Downloading），保留 error/warning
/// - pip install：去掉下载进度，保留最终结果
/// - git diff：截断过长的 hunk
fn compress_exec_output(output: &str, command: &str) -> String {
    let cmd_lower = command.to_lowercase();

    // 测试输出：cargo test / pytest / go test / npm test
    if cmd_lower.contains("cargo test")
        || cmd_lower.contains("pytest")
        || cmd_lower.contains("go test")
        || cmd_lower.contains("npm test")
        || cmd_lower.contains("jest")
    {
        return compress_test_output(output);
    }

    // 编译/构建输出：cargo build / npm run build / go build
    if cmd_lower.contains("cargo build")
        || cmd_lower.contains("cargo check")
        || cmd_lower.contains("npm run build")
        || cmd_lower.contains("go build")
        || cmd_lower.contains("make")
    {
        return compress_build_output(output);
    }

    // pip install
    if cmd_lower.contains("pip install") || cmd_lower.contains("pip3 install") {
        return compress_pip_output(output);
    }

    // git diff
    if cmd_lower.starts_with("git diff") {
        return compress_git_diff(output);
    }

    // 通用：输出超过 200 行时，保留首尾 + 摘要
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() > 200 {
        let head: Vec<&str> = lines[..80].to_vec();
        let tail: Vec<&str> = lines[lines.len() - 40..].to_vec();
        return format!(
            "{}\n\n... ({} lines omitted) ...\n\n{}",
            head.join("\n"),
            lines.len() - 120,
            tail.join("\n")
        );
    }

    output.to_string()
}

/// 测试输出压缩：只保留失败项 + 摘要行
fn compress_test_output(output: &str) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut in_failure = false;
    let mut failure_lines = 0;
    let mut summary_lines: Vec<String> = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim_start();

        // 摘要行（总是保留）
        if trimmed.starts_with("test result:")
            || trimmed.starts_with("FAILED")
            || trimmed.starts_with("failures:")
            || trimmed.starts_with("error[")
            || trimmed.starts_with("error:")
            || (trimmed.contains("passed")
                && (trimmed.contains("failed") || trimmed.contains("ignored")))
            || trimmed.starts_with("Tests:")
            || trimmed.starts_with("Test Suites:")
        {
            summary_lines.push(line.to_string());
            continue;
        }

        // 失败块
        if trimmed.starts_with("---- ")
            || trimmed.starts_with("FAIL ")
            || trimmed.starts_with("FAILED ")
            || trimmed.starts_with("E ")
            || trimmed.starts_with("AssertionError")
            || trimmed.starts_with("assert ")
            || trimmed.starts_with("thread '")
        {
            in_failure = true;
            failure_lines = 0;
        }

        if in_failure {
            failure_lines += 1;
            if failure_lines <= 20 {
                result.push(line.to_string());
            }
            if trimmed.is_empty() && failure_lines > 3 {
                in_failure = false;
            }
        }
    }

    if result.is_empty() && summary_lines.is_empty() {
        return output.to_string();
    }

    result.extend(summary_lines);
    result.join("\n")
}

/// 编译输出压缩：去掉进度行，保留 error/warning
fn compress_build_output(output: &str) -> String {
    let mut result = Vec::new();
    let mut error_count = 0u32;

    for line in output.lines() {
        let trimmed = line.trim_start();

        // 跳过进度行
        if trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Downloading ")
            || trimmed.starts_with("Downloaded ")
            || trimmed.starts_with("Locking ")
            || trimmed.starts_with("Resolving ")
            || trimmed.starts_with("Updating ")
            || trimmed.starts_with("Packaging ")
            || trimmed.starts_with("Fresh ")
        {
            continue;
        }

        // error/warning 总是保留（限制数量）
        if trimmed.starts_with("error") || trimmed.starts_with("warning") {
            error_count += 1;
            if error_count <= 30 {
                result.push(line.to_string());
            }
            continue;
        }

        // Finished/summary 总是保留
        if trimmed.starts_with("Finished")
            || trimmed.starts_with("Built")
            || trimmed.contains("generated")
            || trimmed.starts_with("Successfully")
        {
            result.push(line.to_string());
            continue;
        }

        // error 上下文行（缩进的行）
        if error_count > 0 && error_count <= 30 && (trimmed.starts_with("-->") || trimmed.starts_with("|") || trimmed.starts_with("=")) {
            result.push(line.to_string());
        }
    }

    if result.is_empty() {
        return output.to_string();
    }

    if error_count > 30 {
        result.push(format!("... +{} more errors/warnings", error_count - 30));
    }

    result.join("\n")
}

/// pip install 压缩：去掉下载进度，保留安装结果
fn compress_pip_output(output: &str) -> String {
    output
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !t.starts_with("Downloading ")
                && !t.starts_with("Collecting ")
                && !t.starts_with("Using cached ")
                && !t.starts_with("  Downloading ")
                && !t.contains("━")
                && !t.contains("██")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// git diff 压缩：每个 hunk 最多 30 行
fn compress_git_diff(output: &str) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut hunk_lines = 0;
    let mut hunk_truncated = false;

    for line in output.lines() {
        if line.starts_with("diff ") || line.starts_with("---") || line.starts_with("+++") || line.starts_with("@@") {
            if hunk_truncated {
                result.push("  ... (hunk truncated)".to_string());
                hunk_truncated = false;
            }
            hunk_lines = 0;
            result.push(line.to_string());
            continue;
        }

        hunk_lines += 1;
        if hunk_lines <= 30 {
            result.push(line.to_string());
        } else if !hunk_truncated {
            hunk_truncated = true;
        }
    }

    if hunk_truncated {
        result.push("  ... (hunk truncated)".to_string());
    }

    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_timeout_policy_and_foreground_guard() {
        assert_eq!(effective_timeout_secs(Some("7f088316"), "python run_payment_report.py", Some(1), 120), 2700);
        assert_eq!(effective_timeout_secs(Some("c9cd54b0"), "python run_ar_report.py", None, 120), 7200);
        assert_eq!(effective_timeout_secs(Some("c9cd54b0"), "python revenue_qa.py refresh", Some(9999), 120), 600);
        assert_eq!(effective_timeout_secs(None, "python run_ar_report.py", Some(300), 120), 300);
        for command in ["setsid python run_ar_report.py", "nohup python run_ar_report.py",
            "python run_ar_report.py &", "python run_ar_report.py 2>&1 &",
            "sh -c 'python run_ar_report.py'", "sleep 1; ps; tail x.log"] {
            assert!(validate_timer_command(Some("c9cd54b0"), command).is_err());
        }
        assert!(validate_timer_command(Some("7f088316"), "python run_payment_report.py >x.log 2>&1").is_ok());
    }

    /// 测试：正常执行 echo 命令
    #[tokio::test]
    async fn test_exec_echo() {
        let tool = ExecTool::new(None, None);
        let mut params = HashMap::new();
        params.insert("command".into(), json!("echo hello"));
        let result = tool.execute(params).await;
        assert_eq!(result.trim(), "hello");
    }

    /// 测试：rm -rf 被安全机制拦截
    #[tokio::test]
    async fn test_exec_blocked_rm_rf() {
        let tool = ExecTool::new(None, None);
        let mut params = HashMap::new();
        params.insert("command".into(), json!("rm -rf /"));
        let result = tool.execute(params).await;
        assert!(result.contains("blocked by safety guard"));
    }

    /// 测试：fork 炸弹被安全机制拦截
    #[tokio::test]
    async fn test_exec_blocked_fork_bomb() {
        let tool = ExecTool::new(None, None);
        let mut params = HashMap::new();
        params.insert("command".into(), json!(":() { :|:& };:"));
        let result = tool.execute(params).await;
        assert!(result.contains("blocked by safety guard"));
    }

    /// 测试：非0退出码能正确报告
    #[tokio::test]
    async fn test_exec_exit_code() {
        let tool = ExecTool::new(None, None);
        let mut params = HashMap::new();
        params.insert("command".into(), json!("exit 1"));
        let result = tool.execute(params).await;
        assert!(result.contains("Exit code: 1"));
    }

    /// 测试：exec 输出超限时头尾双段保留（含截断标记，且尾部为原始结尾）
    #[tokio::test]
    async fn test_exec_output_head_tail_truncation() {
        // 还原默认上限，避免其他用例污染（单例进程内共享）
        crate::truncation::init_truncation_limits(crate::truncation::TruncationLimits::default());
        let limits = current_truncation_limits();
        // 构造远超上限的输出：用 seq 打印从 1 到一个很大的数，确保字符数 > 上限
        let n = limits.exec_truncate_chars * 2;
        let tool = ExecTool::new(None, None);
        let mut params = HashMap::new();
        // 打印一长串可识别的尾部标记，确保结尾稳定
        params.insert(
            "command".into(),
            json!(format!(
                "head -c {} /dev/zero | tr '\\0' 'a'; printf 'TAIL_MARKER_END'",
                n
            )),
        );
        let result = tool.execute(params).await;
        // 1) 含截断标记（头尾之间插入）
        assert!(
            result.contains("truncated") && result.contains("chars omitted"),
            "expected truncation marker, got tail: {}",
            &result[result.len().saturating_sub(80)..]
        );
        // 2) 头部保留原始开头
        assert!(result.starts_with('a'));
        // 3) 尾部保留原始结尾（证明不是纯头部截断）
        assert!(
            result.ends_with("TAIL_MARKER_END"),
            "expected original tail preserved"
        );
        // 4) 保留字符总数不超过上限（去掉标记后）
        let total_chars = result.chars().count();
        assert!(total_chars <= limits.exec_truncate_chars + 64);
    }
}
