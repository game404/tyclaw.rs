"""
变更采集工具
一次性收集 server / client / config 三个仓库在两个版本间的所有变更
配表仓库的 commit 会与 excel_diff 做列级交叉校验，自动标注热更等价 commit

用法:
  # 自动检测最近两个版本
  venv/bin/python3 tools/collect_changes.py

  # 指定版本
  venv/bin/python3 tools/collect_changes.py --base 2026.02.09 --head 2026.02.25
"""

import argparse
import os
import re
import subprocess
import sys
import tempfile
from datetime import datetime
from pathlib import Path

import openpyxl

# 公告生成时忽略的配表（不影响玩家体验的运营调度数据）
CHANGELOG_SKIP_FILES = {
    "Configs/cross_server_match.xlsx",
    "Configs/patch.xlsx",
}

from utils import load_config, get_repos as _get_repos_cfg, get_repo_path, run_git as _run_git
from excel_diff import (
    diff_excel_file,
    diff_sheet,
    format_diff_markdown,
    git_changed_xlsx_files,
    git_deleted_xlsx_files,
    git_extract_file,
    read_sheet_data,
)


run_git = _run_git


# ---------------------------------------------------------------------------
# 分支检测
# ---------------------------------------------------------------------------

def find_release_branches(repo_path, count=2):
    """从远程分支中找到最近 count 个 release 分支（按日期去重）"""
    result = run_git(repo_path, "branch", "-r")
    if result.returncode != 0:
        return []

    pattern = re.compile(r"origin/(release/v(\d{4}\.\d{2}\.\d{2})\S*)")
    today = datetime.now().date()
    candidates = []

    for line in result.stdout.splitlines():
        m = pattern.search(line.strip())
        if not m:
            continue
        branch_name = m.group(1)
        date_str = m.group(2)
        try:
            branch_date = datetime.strptime(date_str, "%Y.%m.%d").date()
        except ValueError:
            continue
        if branch_date <= today:
            candidates.append((branch_date, date_str, branch_name))

    candidates.sort(key=lambda x: x[0], reverse=True)

    seen_dates = set()
    out = []
    for _, date_str, name in candidates:
        if date_str not in seen_dates:
            seen_dates.add(date_str)
            out.append((date_str, name))
        if len(out) >= count:
            break
    return out


def find_branch_for_date(repo_path, date_str):
    """在仓库中找到匹配指定日期的 release 分支"""
    result = run_git(repo_path, "branch", "-r")
    if result.returncode != 0:
        return None

    pattern = re.compile(rf"origin/(release/v{re.escape(date_str)}\S*)")
    for line in result.stdout.splitlines():
        m = pattern.search(line.strip())
        if m:
            return m.group(1)
    return None


# ---------------------------------------------------------------------------
# xlsx 解析缓存（用于 commit 级列交叉校验）
# ---------------------------------------------------------------------------

_xlsx_cache: dict = {}


def _get_xlsx_sheets(repo_path, ref, filepath):
    """提取并解析 xlsx，返回 {sheet_name: (headers, rows)}，带缓存"""
    cache_key = (str(repo_path), ref, filepath)
    if cache_key in _xlsx_cache:
        return _xlsx_cache[cache_key]

    fd, tmp_path = tempfile.mkstemp(suffix=".xlsx")
    os.close(fd)

    try:
        if not git_extract_file(str(repo_path), ref, filepath, tmp_path):
            _xlsx_cache[cache_key] = None
            return None
        wb = openpyxl.load_workbook(tmp_path, read_only=True)
        data = {}
        for sn in wb.sheetnames:
            headers, rows = read_sheet_data(wb, sn)
            data[sn] = (headers, rows)
        wb.close()
    except Exception:
        _xlsx_cache[cache_key] = None
        return None
    finally:
        Path(tmp_path).unlink(missing_ok=True)

    _xlsx_cache[cache_key] = data
    return data


def _get_commit_changed_columns(repo_path, commit_hash, filepath):
    """计算某个 commit 对某个 xlsx 文件改了哪些 (sheet, column)"""
    parent_ref = f"{commit_hash}^"
    base_data = _get_xlsx_sheets(repo_path, parent_ref, filepath)
    head_data = _get_xlsx_sheets(repo_path, commit_hash, filepath)

    if base_data is None and head_data is None:
        return set()
    if base_data is None:
        return {("__file__", "__added__")}
    if head_data is None:
        return {("__file__", "__deleted__")}

    changed: set[tuple[str, str]] = set()
    all_sheets = set(base_data.keys()) | set(head_data.keys())

    for sn in all_sheets:
        if sn not in base_data:
            changed.add((sn, "__sheet_added__"))
            continue
        if sn not in head_data:
            changed.add((sn, "__sheet_deleted__"))
            continue

        bh, br = base_data[sn]
        hh, hr = head_data[sn]
        d = diff_sheet(bh, br, hh, hr)

        for row in d.get("modified_rows", []):
            for col in row["changes"]:
                changed.add((sn, col))
        if d.get("added_rows"):
            changed.add((sn, "__rows_added__"))
        if d.get("removed_rows"):
            changed.add((sn, "__rows_removed__"))
        for col in d.get("added_columns", []):
            changed.add((sn, f"__col_added__{col}"))
        for col in d.get("removed_columns", []):
            changed.add((sn, f"__col_removed__{col}"))

    return changed


def _build_branch_diff_index(excel_diff_files):
    """从 branch 级 excel_diff 结果构建 {filepath: set((sheet, column))} 索引"""
    index: dict[str, set[tuple[str, str]]] = {}

    for file_info in excel_diff_files:
        filepath = file_info["file"]
        cols: set[tuple[str, str]] = set()

        status = file_info.get("status", "")
        if status == "added":
            cols.add(("__file__", "__added__"))
        elif status == "deleted":
            cols.add(("__file__", "__deleted__"))

        for sheet in file_info.get("sheets", []):
            sn = sheet["sheet"]
            st = sheet.get("status", "")

            if st == "added":
                cols.add((sn, "__sheet_added__"))
                continue
            if st == "deleted":
                cols.add((sn, "__sheet_deleted__"))
                continue

            for row in sheet.get("modified_rows", []):
                for col in row["changes"]:
                    cols.add((sn, col))
            if sheet.get("added_rows"):
                cols.add((sn, "__rows_added__"))
            if sheet.get("removed_rows"):
                cols.add((sn, "__rows_removed__"))
            for col in sheet.get("added_columns", []):
                cols.add((sn, f"__col_added__{col}"))
            for col in sheet.get("removed_columns", []):
                cols.add((sn, f"__col_removed__{col}"))

        if cols:
            index[filepath] = cols

    return index


# ---------------------------------------------------------------------------
# 通用 git 信息采集
# ---------------------------------------------------------------------------

def _get_diff_stat(repo_path, base_ref, head_ref):
    result = run_git(repo_path, "diff", "--shortstat", base_ref, head_ref)
    return result.stdout.strip() if result.returncode == 0 else ""


def _get_commits(repo_path, base_ref, head_ref):
    result = run_git(
        repo_path, "log", "--format=%H|%h|%s", f"{base_ref}..{head_ref}"
    )
    if result.returncode != 0:
        return []
    commits = []
    for line in result.stdout.strip().splitlines():
        if not line.strip():
            continue
        parts = line.split("|", 2)
        if len(parts) == 3:
            commits.append(
                {"hash": parts[0], "short_hash": parts[1], "message": parts[2]}
            )
    return commits


def _get_commit_files(repo_path, commit_hash):
    result = run_git(repo_path, "show", "--name-only", "--format=", commit_hash)
    if result.returncode != 0:
        return []
    return [f.strip() for f in result.stdout.strip().splitlines() if f.strip()]


def _is_merge_commit(repo_path, commit_hash):
    result = run_git(repo_path, "rev-parse", "--verify", f"{commit_hash}^2")
    return result.returncode == 0


# ---------------------------------------------------------------------------
# 仓库变更采集
# ---------------------------------------------------------------------------

def collect_code_repo(name, repo_path, base_branch, head_branch):
    """采集代码仓库（server / client）的变更"""
    base_ref = f"origin/{base_branch}"
    head_ref = f"origin/{head_branch}"

    print(f"[{name}] Collecting changes...", file=sys.stderr)

    return {
        "type": "code",
        "name": name,
        "base": base_branch,
        "head": head_branch,
        "diff_stat": _get_diff_stat(repo_path, base_ref, head_ref),
        "commits": _get_commits(repo_path, base_ref, head_ref),
    }


def collect_config_repo(config_path, base_branch, head_branch, limit):
    """采集配表仓库：excel_diff + commit 列级交叉校验"""
    base_ref = f"origin/{base_branch}"
    head_ref = f"origin/{head_branch}"
    repo = str(config_path)

    print("[config] Collecting changes...", file=sys.stderr)

    diff_stat = _get_diff_stat(repo, base_ref, head_ref)
    commits = _get_commits(repo, base_ref, head_ref)

    # ---- excel diff (branch-level) ----
    print("[config] Running excel diff...", file=sys.stderr)
    changed_files = git_changed_xlsx_files(config_path, base_ref, head_ref)
    deleted_files = git_deleted_xlsx_files(config_path, base_ref, head_ref)

    files_result = []
    skipped = [fp for fp in changed_files if fp in CHANGELOG_SKIP_FILES]
    for fp in changed_files:
        if fp.startswith("~$") or fp in CHANGELOG_SKIP_FILES:
            continue
        print(f"  Diffing: {fp}", file=sys.stderr)
        files_result.append(
            diff_excel_file(config_path, base_ref, head_ref, fp, limit=limit)
        )
    for fp in deleted_files:
        files_result.append({"file": fp, "status": "deleted"})
    if skipped:
        print(f"[config] Skipped {len(skipped)} files: {', '.join(skipped)}",
              file=sys.stderr)

    # ---- commit 列级交叉校验 ----
    branch_index = _build_branch_diff_index(files_result)

    print("[config] Cross-checking commits against excel_diff...", file=sys.stderr)
    hotfix_count = 0
    total = len(commits)

    for i, commit in enumerate(commits):
        if _is_merge_commit(repo, commit["hash"]):
            commit["diff_status"] = "merge"
            continue

        files = _get_commit_files(repo, commit["hash"])
        commit["files"] = files
        xlsx_files = [f for f in files if f.endswith(".xlsx")]

        if not xlsx_files:
            commit["diff_status"] = "non_xlsx"
            continue

        # 快速检查：所有 xlsx 文件是否都不在 branch diff 中
        files_in_diff = [f for f in xlsx_files if f in branch_index]
        if not files_in_diff:
            commit["diff_status"] = "hotfix_equivalent"
            hotfix_count += 1
            continue

        # 列级检查：commit 改的列是否与 branch diff 有交集
        has_overlap = False
        for fp in files_in_diff:
            pct = f"[{i + 1}/{total}]"
            print(
                f"  {pct} Checking {commit['short_hash']} vs {Path(fp).name}...",
                file=sys.stderr,
            )
            commit_cols = _get_commit_changed_columns(repo, commit["hash"], fp)
            if commit_cols & branch_index[fp]:
                has_overlap = True
                break

        if has_overlap:
            commit["diff_status"] = "verified"
        else:
            commit["diff_status"] = "hotfix_equivalent"
            hotfix_count += 1

    print(
        f"[config] Done. {hotfix_count}/{total} hotfix-equivalent commits identified.",
        file=sys.stderr,
    )

    return {
        "type": "config",
        "name": "config",
        "base": base_branch,
        "head": head_branch,
        "diff_stat": diff_stat,
        "commits": commits,
        "excel_diff": {"base_ref": base_ref, "head_ref": head_ref, "files": files_result},
    }


# ---------------------------------------------------------------------------
# 输出格式化
# ---------------------------------------------------------------------------

def format_report(results, base_date, head_date):
    """将采集结果格式化为 markdown 报告"""
    lines = [
        "# 版本变更采集报告",
        "",
        f"> 版本范围: v{base_date} → v{head_date}",
        f"> 采集时间: {datetime.now().strftime('%Y-%m-%d %H:%M')}",
        "",
    ]

    for repo in results:
        if "error" in repo:
            lines += [f"## {repo['name']}", "", f"Error: {repo['error']}", ""]
            continue

        lines += ["---", ""]

        if repo["type"] == "code":
            _format_code_repo(lines, repo)
        elif repo["type"] == "config":
            _format_config_repo(lines, repo)

    return "\n".join(lines)


def format_repo_report(repo_result, base_date, head_date):
    """将单个仓库的采集结果格式化为独立的 markdown 报告"""
    ts = datetime.now().strftime("%Y-%m-%d %H:%M")
    lines = [
        f"> 版本范围: v{base_date} → v{head_date}",
        f"> 采集时间: {ts}",
        "",
    ]

    if "error" in repo_result:
        lines += [f"Error: {repo_result['error']}", ""]
    elif repo_result["type"] == "code":
        _format_code_repo(lines, repo_result)
    elif repo_result["type"] == "config":
        _format_config_repo(lines, repo_result)

    return "\n".join(lines)


def _format_code_repo(lines, repo):
    lines.append(f"## {repo['name']}")
    lines += ["", f"**分支**: `{repo['base']}` → `{repo['head']}`", ""]

    if repo["diff_stat"]:
        lines += [f"**变更统计**: {repo['diff_stat']}", ""]

    commits = repo["commits"]
    if commits:
        lines += [f"### Commit 记录 ({len(commits)} 条)", ""]
        for c in commits:
            lines.append(f"- {c['short_hash']} {c['message']}")
        lines.append("")
    else:
        lines += ["无 commit。", ""]


def _format_config_repo(lines, repo):
    lines += ["## config（配表）", "", f"**分支**: `{repo['base']}` → `{repo['head']}`", ""]

    if repo.get("diff_stat"):
        lines += [f"**变更统计**: {repo['diff_stat']}", ""]

    # excel_diff 主体（过滤 # 备注列）
    lines += [
        "### 📊 配表变更明细 (excel_diff)",
        "",
        "> ⚠️ 以下是配表相关公告条目的**唯一事实来源**。",
        "> 只有出现在此处的字段级变更才能写入公告。",
        "",
        format_diff_markdown(repo["excel_diff"], skip_comment_cols=True),
        "",
    ]

    # commit 分类
    verified = [c for c in repo["commits"] if c.get("diff_status") == "verified"]
    hotfix = [c for c in repo["commits"] if c.get("diff_status") == "hotfix_equivalent"]
    non_xlsx = [c for c in repo["commits"] if c.get("diff_status") == "non_xlsx"]
    merge = [c for c in repo["commits"] if c.get("diff_status") == "merge"]
    total = len(repo["commits"])

    lines += [f"### 📝 Commit 记录 (共 {total} 条)", ""]
    lines.append("> 以下 commit 已与 excel_diff 做列级交叉校验。")
    lines.append("")

    if verified:
        lines.append(
            f"**有效 commit ({len(verified)} 条)** — 改动的列在 excel_diff 中有对应差异:"
        )
        lines.append("")
        for c in verified:
            xlsx = [f for f in c.get("files", []) if f.endswith(".xlsx")]
            tag = ", ".join(Path(f).name for f in xlsx[:3])
            lines.append(f"- ✅ {c['short_hash']} {c['message']} [{tag}]")
        lines.append("")

    if hotfix:
        lines.append(
            f"**⚠️ 热更等价 commit ({len(hotfix)} 条)** — 改动在 excel_diff 中无差异，已自动排除"
        )
        lines.append("")

    if non_xlsx:
        lines.append(f"**非配表 commit ({len(non_xlsx)} 条):**")
        lines.append("")
        for c in non_xlsx:
            lines.append(f"- {c['short_hash']} {c['message']}")
        lines.append("")

    if merge:
        lines.append(f"**Merge commit ({len(merge)} 条，已跳过)**")
        lines.append("")


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Collect version changes across server/client/config repos"
    )
    parser.add_argument("--base", help="Base version date, e.g. 2026.02.09")
    parser.add_argument("--head", help="Head version date, e.g. 2026.02.25")
    parser.add_argument("--config", help="Path to config.yaml")
    parser.add_argument(
        "--limit", type=int, default=50, help="Max diff rows per sheet (default: 50)"
    )
    parser.add_argument(
        "--output", "-o", help="Write report to file instead of stdout"
    )
    parser.add_argument(
        "--split", action="store_true",
        help="Output separate files per repo: {output}_server.md / _client.md / _config.md"
    )
    args = parser.parse_args()

    config = load_config(args.config)

    repos = {
        name: r["path"]
        for name, r in _get_repos_cfg(config, worktree_only=True).items()
    }

    cfg_repo_path = get_repo_path(config, "config")
    if not cfg_repo_path:
        print("Error: code.repos.config not configured", file=sys.stderr)
        sys.exit(1)

    # fetch 所有仓库
    print("Fetching remote branches...", file=sys.stderr)
    for name, path in repos.items():
        run_git(path, "fetch", "origin", "--prune")

    # 确定版本范围
    if args.base and args.head:
        base_date, head_date = args.base, args.head
    else:
        branches = find_release_branches(cfg_repo_path)
        if len(branches) < 2:
            print("Error: need at least 2 release branches", file=sys.stderr)
            sys.exit(1)
        head_date = branches[0][0]
        base_date = branches[1][0]

    print(f"Version range: v{base_date} → v{head_date}", file=sys.stderr)

    # 采集各仓库变更
    results = []
    for name, path in repos.items():

        base_branch = find_branch_for_date(path, base_date)
        head_branch = find_branch_for_date(path, head_date)

        if not base_branch or not head_branch:
            results.append({
                "name": name,
                "error": f"Branch not found for {base_date} and/or {head_date}",
            })
            continue

        if name == "config":
            results.append(
                collect_config_repo(path, base_branch, head_branch, args.limit)
            )
        else:
            results.append(collect_code_repo(name, path, base_branch, head_branch))

    if args.split:
        if not args.output:
            print("Error: --split requires --output", file=sys.stderr)
            sys.exit(1)
        prefix = re.sub(r"\.(md|txt)$", "", args.output)
        print("Split output files:", file=sys.stderr)
        for repo in results:
            name = repo.get("name", "unknown")
            content = format_repo_report(repo, base_date, head_date)
            out_path = Path(f"{prefix}_{name}.md")
            out_path.parent.mkdir(parents=True, exist_ok=True)
            out_path.write_text(content, encoding="utf-8")
            lines_count = content.count("\n") + 1
            print(f"  {out_path} ({lines_count} lines)", file=sys.stderr)
    elif args.output:
        report = format_report(results, base_date, head_date)
        out_path = Path(args.output)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(report, encoding="utf-8")
        print(f"Report written to {out_path}", file=sys.stderr)
    else:
        report = format_report(results, base_date, head_date)
        print(report)


if __name__ == "__main__":
    main()
