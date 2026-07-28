#!/usr/bin/env python3
"""清理长期记忆中的"工具参数纠错"投毒行。

背景：模型偶发把 `path` 写成 `file_path` 后自我纠错，纠错被合并进 MEMORY.md，
此后每轮注入系统提示，否定式表述反而激活 file_path token，诱导模型继续犯错，
形成自我强化的投毒反馈环。写入层已加确定性过滤（memory_store.rs::sanitize_memory_text），
本脚本用**完全一致**的启发式清理线上已投毒的存量记忆文件。

用法：
    # 预览（默认 dry-run，不写盘）
    python3 scripts/purge_memory_poison.py /path/to/workspaces

    # 实际写盘（写前自动备份 *.bak）
    python3 scripts/purge_memory_poison.py /path/to/workspaces --apply

    # 同时清理 HISTORY.md（默认只处理 MEMORY.md）
    python3 scripts/purge_memory_poison.py /path/to/workspaces --apply --include-history

安全：
    - 默认 --dry-run，仅报告将删除的行。
    - --apply 写盘前对每个被修改文件生成 <file>.bak 备份。
    - agent 运行中直接改 MEMORY.md 有竞争风险，建议低峰或停服执行。
"""

import argparse
import sys
from pathlib import Path

# 与 memory_store.rs::is_tool_param_noise 保持一致的启发式。
_EXPLICIT_ERRORS = (
    "missing required parameter",
    "missing 'path' parameter",
    'missing "path" parameter',
)
_PARAM_MARKERS = ("参数", "参数名", "字段名", "parameter", "argument")
_TOOL_TOKENS = (
    "file_path",
    "read_file",
    "write_file",
    "edit_file",
    "list_dir",
    "apply_patch",
    "grep_search",
    "delete_file",
    "send_file",
    "move_file",
    "copy_file",
)


def is_tool_param_noise(line: str) -> bool:
    """判断单行是否为"工具参数机制/参数名纠错"噪声。"""
    l = line.lower()
    if any(err in l for err in _EXPLICIT_ERRORS):
        return True
    param_marker = any(k in l for k in _PARAM_MARKERS)
    tool_token = any(k in l for k in _TOOL_TOKENS)
    return param_marker and tool_token


def sanitize_text(text: str):
    """返回 (清洗后文本, 被删除的行列表)。按行过滤，保留其余全部内容。"""
    ends_with_nl = text.endswith("\n")
    lines = text.split("\n")
    # split 会在结尾换行时产生一个空串末元素，去掉以复原 lines() 语义
    if ends_with_nl and lines and lines[-1] == "":
        lines.pop()
    kept, removed = [], []
    for line in lines:
        if is_tool_param_noise(line):
            removed.append(line)
        else:
            kept.append(line)
    out = "\n".join(kept)
    if ends_with_nl and out:
        out += "\n"
    return out, removed


def iter_target_files(root: Path, include_history: bool):
    names = ["MEMORY.md"] + (["HISTORY.md"] if include_history else [])
    for name in names:
        yield from sorted(root.rglob(name))


def main() -> int:
    ap = argparse.ArgumentParser(description="清理长期记忆中的工具参数纠错投毒行。")
    ap.add_argument("root", type=Path, help="记忆根目录（如 workspaces/，递归查找 MEMORY.md）")
    ap.add_argument("--apply", action="store_true", help="实际写盘（默认 dry-run 只预览）")
    ap.add_argument("--include-history", action="store_true", help="同时清理 HISTORY.md")
    ap.add_argument("--no-backup", action="store_true", help="写盘时不生成 .bak 备份（不推荐）")
    args = ap.parse_args()

    if not args.root.exists():
        print(f"错误：路径不存在 {args.root}", file=sys.stderr)
        return 2

    scanned = 0
    hit_files = 0
    total_removed = 0

    for path in iter_target_files(args.root, args.include_history):
        scanned += 1
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as e:
            print(f"跳过（读取失败）{path}: {e}", file=sys.stderr)
            continue
        cleaned, removed = sanitize_text(text)
        if not removed:
            continue
        hit_files += 1
        total_removed += len(removed)
        print(f"\n{path}  (命中 {len(removed)} 行)")
        for line in removed:
            print(f"  - {line}")
        if args.apply:
            if not args.no_backup:
                bak = path.with_suffix(path.suffix + ".bak")
                bak.write_text(text, encoding="utf-8")
                print(f"  已备份 -> {bak}")
            path.write_text(cleaned, encoding="utf-8")
            print("  已写盘")

    mode = "APPLY" if args.apply else "DRY-RUN"
    print(
        f"\n[{mode}] 扫描 {scanned} 个文件，命中 {hit_files} 个，"
        f"{'删除' if args.apply else '将删除'} {total_removed} 行。"
    )
    if not args.apply and total_removed:
        print("提示：加 --apply 才会实际写盘（写前会备份 .bak）。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
