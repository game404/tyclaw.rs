"""
Git Worktree 管理工具
管理非 release 环境的 worktree 生命周期（创建/更新/状态/按需确保）
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
from datetime import datetime
from pathlib import Path

from utils import load_config, get_project_root, get_repos as _get_repos_cfg, run_git, find_latest_release_branch


def current_branch(repo_path: str) -> str:
    result = run_git(repo_path, "branch", "--show-current")
    return result.stdout.strip()


def branch_exists_remote(repo_path: str, branch: str) -> bool:
    """检查远程分支是否存在"""
    result = run_git(repo_path, "rev-parse", "--verify", f"origin/{branch}")
    return result.returncode == 0


def resolve_branch(repo_path: str, env_name: str) -> str | None:
    """解析分支名：先试原名，不存在则加 dev/ 前缀"""
    if branch_exists_remote(repo_path, env_name):
        return env_name
    prefixed = f"dev/{env_name}"
    if branch_exists_remote(repo_path, prefixed):
        return prefixed
    return None


def is_git_repo(path: str) -> bool:
    """检查路径是否是 git 仓库（包括 worktree）"""
    p = Path(path)
    return (p / ".git").exists() or (p / ".git").is_file()


def get_all_repos(config: dict) -> dict[str, str]:
    """从配置中获取所有 git 仓库路径（用于 release 更新和状态查看）"""
    return {
        name: r["path"]
        for name, r in _get_repos_cfg(config, git_only=True).items()
        if os.path.isdir(r["path"])
    }


def get_repos(config: dict) -> dict[str, str]:
    """从配置中获取需要 worktree 管理的仓库路径"""
    return {
        name: r["path"]
        for name, r in _get_repos_cfg(config, worktree_only=True).items()
        if os.path.isdir(r["path"])
    }


def setup_worktree(repo_path: str, repo_name: str, env_name: str,
                   wt_base: str) -> dict:
    """为单个仓库创建 worktree"""
    wt_path = os.path.join(wt_base, env_name, repo_name)
    result = {"repo": repo_name, "env": env_name, "worktree": wt_path}

    if is_git_repo(wt_path):
        result["status"] = "EXISTS"
        return result

    print(f"  [{repo_name}] Fetching origin...", file=sys.stderr)
    fetch = run_git(repo_path, "fetch", "origin", "--prune")
    if fetch.returncode != 0:
        print(f"  [{repo_name}] fetch failed: {fetch.stderr.strip()}", file=sys.stderr)
        result["status"] = "FETCH_FAILED"
        return result

    branch = resolve_branch(repo_path, env_name)
    if not branch:
        print(f"  [{repo_name}] branch '{env_name}' (and 'dev/{env_name}') not found", file=sys.stderr)
        result["status"] = "BRANCH_NOT_FOUND"
        return result

    result["branch"] = branch

    os.makedirs(os.path.dirname(wt_path), exist_ok=True)
    wt_result = run_git(repo_path, "worktree", "add", wt_path, f"origin/{branch}",
                        "--detach")
    if wt_result.returncode != 0:
        # 有时 detach 模式需要先 checkout
        wt_result = run_git(repo_path, "worktree", "add", "-b",
                            f"wt/{env_name}/{repo_name}", wt_path, f"origin/{branch}")
    if wt_result.returncode != 0:
        print(f"  [{repo_name}] worktree add failed: {wt_result.stderr.strip()}", file=sys.stderr)
        result["status"] = "WORKTREE_ADD_FAILED"
        return result

    print(f"  [{repo_name}] Created worktree: {branch} -> {wt_path}", file=sys.stderr)
    result["status"] = "CREATED"
    return result


def update_worktree(wt_path: str, repo_name: str,
                    env_name: str = "") -> dict:
    """更新单个 worktree（git pull 或 fetch+reset）"""
    result = {"repo": repo_name, "worktree": wt_path}
    if not is_git_repo(wt_path):
        result["status"] = "NOT_FOUND"
        return result

    cur = current_branch(wt_path)
    if cur:
        pull = run_git(wt_path, "pull", "origin", cur)
        if pull.returncode != 0:
            result["status"] = "PULL_FAILED"
            result["error"] = pull.stderr.strip()
        else:
            result["status"] = "UPDATED"
        return result

    # detached HEAD: fetch + resolve branch + reset --hard
    fetch = run_git(wt_path, "fetch", "origin")
    if fetch.returncode != 0:
        result["status"] = "FETCH_FAILED"
        return result

    branch = resolve_branch(wt_path, env_name) if env_name else None
    if not branch:
        result["status"] = "DETACHED_NO_BRANCH"
        return result

    reset = run_git(wt_path, "reset", "--hard", f"origin/{branch}")
    if reset.returncode != 0:
        result["status"] = "RESET_FAILED"
        result["error"] = reset.stderr.strip()
    else:
        result["status"] = "UPDATED"
    return result


def update_release_repo(repo_path: str, repo_name: str) -> dict:
    """更新原始仓库（release），包括检测新 release 分支"""
    result = {"repo": repo_name, "path": repo_path}
    if not os.path.isdir(repo_path):
        result["status"] = "PATH_NOT_FOUND"
        return result

    fetch = run_git(repo_path, "fetch", "origin", "--prune")
    if fetch.returncode != 0:
        result["status"] = "FETCH_FAILED"
        return result

    target = find_latest_release_branch(repo_path)
    cur = current_branch(repo_path)
    result["current"] = cur
    result["target"] = target

    if not target:
        result["status"] = "NO_RELEASE_BRANCH"
        return result

    if cur == target:
        pull = run_git(repo_path, "pull", "origin", target)
        result["status"] = "UP_TO_DATE"
        return result

    # 需要切换到新 release 分支
    from sync_branches import has_uncommitted_changes, checkout_branch
    if has_uncommitted_changes(repo_path):
        result["status"] = "DIRTY"
        return result

    if checkout_branch(repo_path, target):
        result["status"] = "SWITCHED"
    else:
        result["status"] = "CHECKOUT_FAILED"

    return result


def cmd_setup(config: dict):
    """创建预建 worktree"""
    code_cfg = config.get("code", {})
    wt_base = code_cfg.get("worktree_base", "")
    wt_envs = code_cfg.get("worktree_envs", [])

    if not wt_base:
        print("Error: code.worktree_base not configured", file=sys.stderr)
        sys.exit(1)
    if not wt_envs:
        print("No worktree_envs configured, nothing to setup.", file=sys.stderr)
        return

    # 获取原始仓库路径（绕过 BUGHUNTER_ENV）
    repos = get_repos(config)
    if not repos:
        print("Error: No code paths configured", file=sys.stderr)
        sys.exit(1)

    print(f"=== Setting up worktrees (base: {wt_base}) ===")
    for env_name in wt_envs:
        print(f"\n--- Environment: {env_name} ---")
        for repo_name, repo_path in repos.items():
            result = setup_worktree(repo_path, repo_name, env_name, wt_base)
            print(f"  [{repo_name}] {result['status']}", file=sys.stderr)


def _run_post_update(repo_path: str, command: str):
    """执行仓库的 post_update 命令（在仓库目录下执行）"""
    print(f"  [post_update] Running in {os.path.basename(repo_path)}: {command}",
          file=sys.stderr)
    try:
        result = subprocess.run(
            ["bash", "-c", command],
            cwd=repo_path,
            capture_output=True,
            text=True,
            timeout=120,
        )
        if result.returncode == 0:
            print(f"  [post_update] Done", file=sys.stderr)
        else:
            print(f"  [post_update] Failed: {result.stderr.strip()[:200]}",
                  file=sys.stderr)
    except subprocess.TimeoutExpired:
        print(f"  [post_update] Timed out (120s)", file=sys.stderr)


def _get_post_update_commands(config: dict) -> dict[str, str]:
    """从 repos 配置中提取 post_update 命令"""
    raw = config.get("code", {}).get("repos", {})
    return {
        name: cfg["post_update"]
        for name, cfg in raw.items()
        if isinstance(cfg, dict) and cfg.get("post_update")
    }


def cmd_update(config: dict, release_only: bool = False):
    """更新仓库。release_only=True 时只更新 release 仓库，跳过 worktree"""
    code_cfg = config.get("code", {})
    wt_base = code_cfg.get("worktree_base", "")
    all_repos = get_all_repos(config)
    post_updates = _get_post_update_commands(config)

    release_lines = []
    for repo_name, repo_path in all_repos.items():
        result = update_release_repo(repo_path, repo_name)
        status = result["status"]
        if status == "SWITCHED":
            release_lines.append(f"  [{repo_name}] {result['current']} -> {result['target']}")
        if repo_name in post_updates:
            _run_post_update(repo_path, post_updates[repo_name])
    if release_lines:
        print("=== Updating release repos ===")
        for line in release_lines:
            print(line)

    if release_only or not wt_base or not os.path.isdir(wt_base):
        return

    print("\n=== Updating worktrees ===")
    for env_dir in sorted(Path(wt_base).iterdir()):
        if not env_dir.is_dir():
            continue
        env_name = env_dir.name
        for repo_dir in sorted(env_dir.iterdir()):
            if not repo_dir.is_dir():
                continue
            repo_name = repo_dir.name
            result = update_worktree(str(repo_dir), repo_name, env_name)
            print(f"  [{env_name}/{repo_name}] {result['status']}")
            if repo_name in post_updates:
                _run_post_update(str(repo_dir), post_updates[repo_name])


def cmd_status(config: dict):
    """显示当前状态"""
    code_cfg = config.get("code", {})
    wt_base = code_cfg.get("worktree_base", "")
    all_repos = get_all_repos(config)

    print("=== Release repos (original) ===")
    for repo_name, repo_path in all_repos.items():
        cur = current_branch(repo_path) if os.path.isdir(repo_path) else "N/A"
        print(f"  [{repo_name}] {repo_path} ({cur})")

    if not wt_base:
        print("\nNo worktree_base configured.")
        return

    print(f"\n=== Worktrees (base: {wt_base}) ===")
    wt_base_path = Path(wt_base)
    if not wt_base_path.exists():
        print("  (no worktrees created yet)")
        return

    for env_dir in sorted(wt_base_path.iterdir()):
        if not env_dir.is_dir():
            continue
        env_name = env_dir.name
        for repo_dir in sorted(env_dir.iterdir()):
            if not repo_dir.is_dir():
                continue
            repo_name = repo_dir.name
            if is_git_repo(str(repo_dir)):
                cur = current_branch(str(repo_dir)) or "(detached)"
                print(f"  [{env_name}/{repo_name}] {repo_dir} ({cur})")
            else:
                print(f"  [{env_name}/{repo_name}] {repo_dir} (not a git worktree)")


def cmd_ensure(config: dict, env_name: str):
    """确保指定环境的 worktree 存在（按需创建）"""
    code_cfg = config.get("code", {})
    wt_base = code_cfg.get("worktree_base", "")

    if not wt_base:
        print("Error: code.worktree_base not configured", file=sys.stderr)
        sys.exit(1)

    repos = get_repos(config)
    if not repos:
        print("Error: No code paths configured", file=sys.stderr)
        sys.exit(1)

    created = 0
    existed = 0
    failed = 0
    for repo_name, repo_path in repos.items():
        result = setup_worktree(repo_path, repo_name, env_name, wt_base)
        status = result["status"]
        if status == "CREATED":
            created += 1
        elif status == "EXISTS":
            existed += 1
        else:
            failed += 1

    print(f"Environment '{env_name}': {created} created, {existed} existed, {failed} failed")


def main():
    parser = argparse.ArgumentParser(
        description="管理 Git Worktree 环境",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "示例:\n"
            "  %(prog)s setup                 创建预建 worktree\n"
            "  %(prog)s update                更新所有仓库和 worktree\n"
            "  %(prog)s update --release-only 仅更新 release 仓库\n"
            "  %(prog)s status                查看当前状态\n"
            "  %(prog)s ensure --env dev      确保 dev worktree 存在"
        ),
    )
    parser.add_argument("action", choices=["setup", "update", "status", "ensure"],
                        help="操作类型")
    parser.add_argument("--env", help="环境名（ensure 时必需）")
    parser.add_argument("--release-only", action="store_true",
                        help="update 时只更新 release 仓库，跳过 worktree")
    args = parser.parse_args()

    # 读取原始 config（不经过 BUGHUNTER_ENV 映射）
    env_backup = os.environ.pop("BUGHUNTER_ENV", None)
    try:
        config = load_config()
    finally:
        if env_backup is not None:
            os.environ["BUGHUNTER_ENV"] = env_backup

    if args.action == "setup":
        cmd_setup(config)
    elif args.action == "update":
        cmd_update(config, release_only=args.release_only)
    elif args.action == "status":
        cmd_status(config)
    elif args.action == "ensure":
        if not args.env:
            print("Error: --env is required for ensure", file=sys.stderr)
            sys.exit(1)
        cmd_ensure(config, args.env)


if __name__ == "__main__":
    main()
