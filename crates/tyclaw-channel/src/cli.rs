//! CLI 交互循环 —— 基于 rustyline 的交互式 REPL。
//!
//! 终端布局：
//!   上方：固定输入区域（rustyline 提示符 + 用户输入）
//!   分隔线
//!   下方：滚动输出区域（agent 输出在此区域内自动滚动）
//!
//! ```text
//! ┌──────────────────────────────┐
//! │ You> 帮我写一个H5小游戏█     │  ← 固定输入区（第 1 行）
//! ├──────────────────────────────┤  ← 分隔线（第 2 行）
//! │ [轮次 1] 阶段=explore        │  ← 滚动输出区（第 3 行 ~ 末尾）
//! │ ▌ 分析用户请求...            │     新内容从底部出现
//! │   │ [sub] 读取文件...        │     旧内容往上滚
//! │ TyClaw.rs> 已完成            │
//! └──────────────────────────────┘
//! ```
//!
//! 通过 ANSI 滚动区域（DECSTBM）实现：
//! - `\x1b[{top};{bottom}r` 设置滚动区域，区域外的行不参与滚动
//! - rustyline 在第 1 行操作，不受输出影响
//! - 所有 agent 输出通过 `cli_print()` 写入滚动区域

use std::path::PathBuf;

use tyclaw_orchestration::{BusHandle, InboundMessage};

/// 固定顶部区域的行布局：
///   第 1~6 行：Logo
///   第 7 行：提示信息
///   第 8 行：输入行（rustyline）
///   第 9 行：分隔线
///   第 10 行起：滚动输出区
const LOGO_LINES: u16 = 3;
const TIPS_LINE: u16 = LOGO_LINES + 1;      // 4
const INPUT_LINE: u16 = LOGO_LINES + 2;     // 5
const SEP_LINE: u16 = LOGO_LINES + 3;       // 6
const FIXED_TOP_LINES: u16 = LOGO_LINES + 3; // 6

const LOGO: [&str; 3] = [
    "░▀█▀░█░█░█▀▀░█░░░█▀█░█░█░░░░█▀▄░█▀▀",
    "░░█░░░█░░█░░░█░░░█▀█░█▄█░░░░█▀▄░▀▀█",
    "░░▀░░░▀░░▀▀▀░▀▀▀░▀░▀░▀░▀░▀░░▀░▀░▀▀▀",
];

/// 交互式 CLI 通道。
pub struct CliChannel {
    user_id: String,
    workspace_id: String,
    startup_lines: Vec<String>,
}

impl CliChannel {
    pub fn new(user_id: impl Into<String>, workspace_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            workspace_id: workspace_id.into(),
            startup_lines: Vec::new(),
        }
    }

    /// 设置启动时在滚动区显示的信息（如配置摘要）。
    pub fn with_startup_lines(mut self, lines: Vec<String>) -> Self {
        self.startup_lines = lines;
        self
    }

    /// 运行交互式 CLI 循环（REPL）。
    pub async fn run(
        &self,
        bus_handle: BusHandle,
        timer_service: &tyclaw_tools::timer::TimerService,
    ) {
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let startup_lines = self.startup_lines.clone();

        tokio::task::spawn_blocking(move || {
            use rustyline::error::ReadlineError;
            use rustyline::DefaultEditor;

            // 设置终端布局：Logo + 输入在顶部固定，输出在下方滚动
            setup_scroll_region();

            // 在滚动区打印启动信息（配置摘要等）
            for line in &startup_lines {
                cli_print(&format!("\x1b[2m{line}\x1b[0m"));
            }

            let mut rl = DefaultEditor::new().expect("Failed to init readline");
            let history_path = cli_history_path();
            rl.load_history(&history_path).ok();

            loop {
                // 每次 readline 前：强制光标回到第 1 行，清除残留，重画分隔线
                // （rustyline 按回车会打 \n 把光标推下去，这里纠正回来）
                reset_input_area();

                match rl.readline("> ") {
                    Ok(line) => {
                        let trimmed = line.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        rl.add_history_entry(&trimmed).ok();
                        if trimmed.eq_ignore_ascii_case("exit")
                            || trimmed.eq_ignore_ascii_case("quit")
                        {
                            restore_scroll_region();
                            println!("Goodbye!");
                            break;
                        }
                        // 把用户输入 echo 到输出区（像聊天记录）
                        // 用户输入 echo 到输出区，亮白色
                        cli_print(&format!("\x1b[1;37mYou> {trimmed}\x1b[0m"));
                        if input_tx.blocking_send(trimmed).is_err() {
                            break;
                        }
                    }
                    Err(ReadlineError::Interrupted) => {
                        continue;
                    }
                    Err(e) => {
                        restore_scroll_region();
                        println!("Goodbye! (reason: {e})");
                        let _ = input_tx.blocking_send("__exit__".into());
                        break;
                    }
                }
            }
            rl.save_history(&history_path).ok();
        });

        while let Some(input) = input_rx.recv().await {
            if input == "__exit__" {
                break;
            }

            let msg = InboundMessage {
                content: input,
                user_id: self.user_id.clone(),
                user_name: "cli_user".into(),
                workspace_id: self.workspace_id.clone(),
                channel: "cli".into(),
                chat_id: "direct".into(),
                conversation_id: None,
                images: vec![],
                files: vec![],
                reply_tx: None,
                is_timer: false,
                emotion_context: None,
            };

            if let Err(e) = bus_handle.send(msg).await {
                cli_print(&format!("\x1b[2;31mError: failed to send message to bus: {e}\x1b[0m"));
            }
        }

        timer_service.stop();
    }
}

// ---------------------------------------------------------------------------
// 终端滚动区域管理
// ---------------------------------------------------------------------------

/// 获取终端尺寸 (width, height)。
fn terminal_size() -> (u16, u16) {
    #[cfg(unix)]
    {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
                && ws.ws_col > 0
                && ws.ws_row > 0
            {
                return (ws.ws_col, ws.ws_row);
            }
        }
    }
    let cols = std::env::var("COLUMNS").ok().and_then(|v| v.parse().ok()).unwrap_or(80);
    let rows = std::env::var("LINES").ok().and_then(|v| v.parse().ok()).unwrap_or(24);
    (cols, rows)
}

/// 输出区的起始行号（紧接分隔线之后）。
fn output_start_row() -> u16 {
    FIXED_TOP_LINES + 1 // logo + tips + input + separator 之后
}

/// 设置终端布局：
///   第 1~6 行：Logo（固定）
///   第 7 行：提示信息（固定）
///   第 8 行：输入行 rustyline（固定）
///   第 9 行：分隔线（固定）
///   第 10 行 ~ 末尾：滚动输出区域
fn setup_scroll_region() {
    let (width, height) = terminal_size();
    let scroll_start = output_start_row();

    // 清屏
    eprint!("\x1b[2J");
    // 设置滚动区域
    eprint!("\x1b[{scroll_start};{height}r");
    // 画 Logo（青色亮字）
    draw_fixed_top(width);
    // 光标移到输入行
    eprint!("\x1b[{INPUT_LINE};1H");
    flush_stderr();
}

/// 画固定顶部区域（Logo + Tips + 分隔线）。
fn draw_fixed_top(width: u16) {
    // Logo（第 1~6 行）
    for (i, line) in LOGO.iter().enumerate() {
        let row = i as u16 + 1;
        eprint!("\x1b[{row};1H\x1b[K");
        // ░ 阴影用灰色，字母用青色亮字
        for ch in line.chars() {
            if ch == '░' {
                eprint!("\x1b[2m{ch}\x1b[0m");
            } else {
                eprint!("\x1b[1;36m{ch}\x1b[0m");
            }
        }
    }
    // Tips（第 7 行）
    eprint!("\x1b[{TIPS_LINE};1H\x1b[K\x1b[2m  type 'exit' to quit · '/new' new session · ↑↓ history · Ctrl+R search\x1b[0m");
    // 分隔线（第 9 行）
    eprint!("\x1b[{SEP_LINE};1H\x1b[K\x1b[2m{}\x1b[0m", "─".repeat(width as usize));
}

/// 恢复终端为正常模式。
fn restore_scroll_region() {
    let (_, height) = terminal_size();
    eprint!("\x1b[r");          // 重置滚动区域
    eprint!("\x1b[{height};1H"); // 光标到最底行
    flush_stderr();
}

/// 重置输入区 + 刷新固定顶部 + 滚动区域。
///
/// 在每次 readline 前调用，处理：
/// 1. rustyline 按回车后光标位置错乱
/// 2. 终端 resize 后布局失效
fn reset_input_area() {
    let (width, height) = terminal_size();
    let scroll_start = output_start_row();
    // 刷新滚动区域（应对 resize）
    eprint!("\x1b[{scroll_start};{height}r");
    // 重画固定顶部（Logo + Tips + 分隔线，处理 resize 后宽度变化）
    draw_fixed_top(width);
    // 清除输入行残留
    eprint!("\x1b[{INPUT_LINE};1H\x1b[K");
    flush_stderr();
}

/// 在输出区域打印一行。
///
/// 每次调用时重新设置滚动区域（应对终端 resize），
/// 然后：保存光标 → 移到滚动区域底部 → 打印 → 恢复光标。
/// prompt "> " 的宽度，光标应在此列之后。
const PROMPT_WIDTH: u16 = 2;

pub fn cli_print(msg: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let (_, height) = terminal_size();
    let scroll_start = output_start_row();
    // 刷新滚动区域设置（终端 resize 后旧值失效）
    let _ = write!(out, "\x1b[{scroll_start};{height}r");
    // 移到滚动区域最后一行
    let _ = write!(out, "\x1b[{height};1H");
    // 打印（换行触发滚动区域内上滚）
    let _ = writeln!(out, "{msg}");
    // 光标回到输入行 prompt 之后
    let col = PROMPT_WIDTH + 1;
    let _ = write!(out, "\x1b[{INPUT_LINE};{col}H");
    let _ = out.flush();
}

fn flush_stderr() {
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

fn cli_history_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".tyclaw_cli_history")
}
