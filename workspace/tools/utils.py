"""
公共工具模块
提供配置读取、输出格式化等通用功能
"""

import json
import os
import sys
from datetime import datetime, timedelta
from pathlib import Path

import yaml

_FILE_ROOT = Path(__file__).resolve().parent.parent
_CWD_ROOT = Path.cwd()

# 优先用 cwd（Rust 进程的工作目录 = team rootdir），
# 这样 symlink 部署时 tools/ 指向共享目录也能正确找到 team 自己的 config
_PROJECT_ROOT = _CWD_ROOT if (_CWD_ROOT / "config" / "config.yaml").exists() else _FILE_ROOT

if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))


def get_project_root():
    """获取项目根目录（优先 cwd，fallback 到 __file__ 相对路径）"""
    return _PROJECT_ROOT


def load_config(config_path=None):
    """
    读取 config.yaml 配置文件

    路径解析优先级：cwd/config/config.yaml > __file__/../config/config.yaml
    当 BUGHUNTER_ENV 环境变量为非空且非 "release" 时，
    自动将 code.* 路径替换为 worktree 路径。

    Args:
        config_path: 配置文件路径，默认为 config/config.yaml
    Returns:
        dict: 配置字典
    """
    if config_path is None:
        config_path = get_project_root() / "config" / "config.yaml"
    else:
        config_path = Path(config_path)

    if not config_path.exists():
        print(f"Error: config file not found: {config_path}", file=sys.stderr)
        print("Please copy config/config.example.yaml to config/config.yaml and fill in your settings.", file=sys.stderr)
        sys.exit(1)

    with open(config_path, "r", encoding="utf-8") as f:
        config = yaml.safe_load(f)

    env = os.environ.get("BUGHUNTER_ENV", "").strip()
    wt_base = config.get("code", {}).get("worktree_base", "")
    if env and env != "release" and wt_base:
        repos = config.get("code", {}).get("repos", {})
        for name, repo_cfg in repos.items():
            if isinstance(repo_cfg, str):
                repos[name] = {"path": repo_cfg}
                repo_cfg = repos[name]
            if not repo_cfg.get("worktree", False):
                continue
            wt_path = os.path.join(wt_base, env, name)
            if os.path.isdir(wt_path):
                repos[name] = {**repo_cfg, "path": wt_path}

    return config


def get_repos(config, *, worktree_only=False, git_only=False):
    """从 code.repos 中获取仓库配置

    Args:
        config: load_config() 返回的配置字典
        worktree_only: 仅返回 worktree=true 的仓库
        git_only: 仅返回 git 仓库（排除 git=false 的）
    Returns:
        dict[name, {"path": str, "worktree": bool, "git": bool}]
    """
    raw = config.get("code", {}).get("repos", {})
    result = {}
    for name, repo_cfg in raw.items():
        if isinstance(repo_cfg, str):
            repo_cfg = {"path": repo_cfg}
        repo = {
            "path": repo_cfg.get("path", ""),
            "worktree": repo_cfg.get("worktree", False),
            "git": repo_cfg.get("git", True),
        }
        if not repo["path"]:
            continue
        if worktree_only and not repo["worktree"]:
            continue
        if git_only and not repo["git"]:
            continue
        result[name] = repo
    return result


def get_repo_path(config, name):
    """获取指定仓库的路径，未配置返回空字符串"""
    repos = get_repos(config)
    repo = repos.get(name)
    return repo["path"] if repo else ""


def format_json(data):
    """格式化输出 JSON"""
    return json.dumps(data, ensure_ascii=False, indent=2, default=str)


def format_markdown_table(headers, rows):
    """
    将数据格式化为 Markdown 表格

    Args:
        headers: 列名列表
        rows: 行数据列表（每行是一个列表或字典）
    Returns:
        str: Markdown 表格字符串
    """
    if not headers or not rows:
        return "No data."

    # 将字典行转为列表行
    if rows and isinstance(rows[0], dict):
        rows = [[str(row.get(h, "")) for h in headers] for row in rows]
    else:
        rows = [[str(v) if v is not None else "" for v in row] for row in rows]

    headers_str = [str(h).replace("|", "\\|") if h is not None else "" for h in headers]

    # 构建表格
    lines = []
    lines.append("| " + " | ".join(headers_str) + " |")
    lines.append("| " + " | ".join(["---"] * len(headers_str)) + " |")
    for row in rows:
        escaped = [cell.replace("|", "\\|") for cell in row]
        lines.append("| " + " | ".join(escaped) + " |")

    return "\n".join(lines)


def parse_time_range(from_time=None, to_time=None, hours=None):
    """
    解析时间范围参数

    Args:
        from_time: 开始时间字符串 "YYYY-MM-DD HH:MM"
        to_time: 结束时间字符串 "YYYY-MM-DD HH:MM"
        hours: 最近 N 小时
    Returns:
        tuple: (from_timestamp, to_timestamp)
    """
    if from_time and to_time:
        fmt = "%Y-%m-%d %H:%M"
        ft = int(datetime.strptime(from_time, fmt).timestamp())
        tt = int(datetime.strptime(to_time, fmt).timestamp())
        return ft, tt
    elif hours:
        now = datetime.now()
        ft = int((now - timedelta(hours=hours)).timestamp())
        tt = int(now.timestamp())
        return ft, tt
    else:
        # 默认最近 24 小时
        now = datetime.now()
        ft = int((now - timedelta(hours=24)).timestamp())
        tt = int(now.timestamp())
        return ft, tt


def run_git(repo_path, *args):
    """在指定仓库路径下执行 git 命令"""
    import subprocess
    return subprocess.run(
        ["git", *args],
        cwd=str(repo_path),
        capture_output=True,
        text=True,
    )


def find_latest_release_branch(repo_path):
    """从远程分支中找到日期最接近今天的 release 分支"""
    import re
    result = run_git(repo_path, "branch", "-r")
    if result.returncode != 0:
        return None

    pattern = re.compile(r"origin/(release/v(\d{4}\.\d{2}\.\d{2}).*)")
    today = datetime.now().date()
    best_branch = None
    best_date = None

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
        if branch_date <= today and (best_date is None or branch_date > best_date):
            best_date = branch_date
            best_branch = branch_name

    return best_branch


def print_output(data, fmt="json"):
    """统一输出函数"""
    if fmt == "json":
        print(format_json(data))
    elif fmt == "markdown":
        if isinstance(data, dict) and "headers" in data and "rows" in data:
            print(format_markdown_table(data["headers"], data["rows"]))
        else:
            print(format_json(data))
    else:
        print(format_json(data))
