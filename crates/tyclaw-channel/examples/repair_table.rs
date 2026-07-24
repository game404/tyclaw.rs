//! 手动验证钉钉「表格修复」出口逻辑。
//!
//! 读取 stdin（或首个命令行参数）里的 markdown，调用**真实的**
//! `repair_pipe_tables`（与钉钉机器人下发出口同一函数），把修复后的 markdown
//! 打印到 stdout，便于肉眼检查或接力发到钉钉查看渲染效果。
//!
//! 用法：
//!   echo '| a | b | |---| | 1 | 2 |' | cargo run -p tyclaw-channel --example repair_table
//!   cargo run -p tyclaw-channel --example repair_table -- "$(cat some.md)"
//!
//! 接力发到钉钉（PC 端查看表格渲染）：
//!   cat bad_table.md \
//!     | cargo run -q -p tyclaw-channel --example repair_table \
//!     | python3 workspace/tools/test_dingtalk_markdown.py --user-id <staffId> --stdin

use std::io::Read;

fn main() {
    let input = match std::env::args().nth(1) {
        Some(s) => s,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .expect("failed to read stdin");
            buf
        }
    };

    let repaired = tyclaw_channel::dingtalk::repair_pipe_tables(&input);
    print!("{repaired}");
}
