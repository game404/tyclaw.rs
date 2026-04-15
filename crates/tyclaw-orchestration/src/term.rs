//! 终端输出工具 —— 在 ANSI 滚动区域内打印，不破坏固定区域布局。
//!
//! 与 tyclaw_channel::cli::cli_print 功能相同，但不依赖 channel crate。

use std::io::Write;


/// 在终端滚动区域内打印一行。
///
/// 移到终端底部 → 打印（触发滚动区内上滚）→ 光标回到输入行。
/// 输入行行号（与 cli.rs INPUT_LINE 一致）。
const INPUT_LINE: u16 = 5;
/// prompt "> " 宽度。
const PROMPT_WIDTH: u16 = 2;

pub fn scroll_print(msg: &str) {
    let mut out = std::io::stdout().lock();
    let height = terminal_height();
    let _ = write!(out, "\x1b[{height};1H");
    let _ = writeln!(out, "{msg}");
    let col = PROMPT_WIDTH + 1;
    let _ = write!(out, "\x1b[{INPUT_LINE};{col}H");
    let _ = out.flush();
}

fn terminal_height() -> u16 {
    #[cfg(unix)]
    {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 {
                return ws.ws_row;
            }
        }
    }
    std::env::var("LINES").ok().and_then(|v| v.parse().ok()).unwrap_or(24)
}
