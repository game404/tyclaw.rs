"""
分支同步工具
确保 server / client / config 三个仓库都切到最新的线上 release 分支
"""

import argparse
import os
import re
import subprocess
import sys
from datetime import datetime

from utils import load_config, get_repos as _get_repos_cfg, run_git, find_latest_release_branch


def fetch_origin(repo_path: str) -> bool:
    result = run_git(repo_path, "fetch", "origin", "--prune")
    if result.returncode != 0:
        print(f"  [WARN] git fetch failed: {result.stderr.strip()}", file=sys.stderr)
        return False
    return True


def has_uncommitted_changes(repo_path: str) -> bool:
    result = run_git(repo_path, "status", "--porcelain")
    return bool(result.stdout.strip())


def current_branch(repo_path: str) -> str:
    result = run_git(repo_path, "branch", "--show-current")
    return result.stdout.strip()


def checkout_branch(repo_path: str, branch: str) -> bool:
    # 先尝试本地是否已有该分支
    local_branch = branch
    result = run_git(repo_path, "rev-parse", "--verify", local_branch)
    if result.returncode == 0:
        result = run_git(repo_path, "checkout", local_branch)
    else:
        result = run_git(repo_path, "checkout", "-b", local_branch, f"origin/{local_branch}")

    if result.returncode != 0:
        print(f"  [ERROR] checkout failed: {result.stderr.strip()}")
        return False

    # pull 最新代码
    pull = run_git(repo_path, "pull", "origin", local_branch)
    if pull.returncode != 0:
        print(f"  [WARN] pull failed: {pull.stderr.strip()}")
    return True


def sync_repo(name: str, repo_path: str) -> dict:
    """同步单个仓库，返回结果摘要"""
    print(f"\n[{name}] {repo_path}")

    result = {"name": name, "path": repo_path}

    if not os.path.isdir(repo_path):
        print(f"  [ERROR] Repo path does not exist: {repo_path}")
        result["status"] = "PATH_NOT_FOUND"
        return result

    if not os.path.isdir(os.path.join(repo_path, ".git")):
        print(f"  [ERROR] Not a git repository: {repo_path}")
        result["status"] = "NOT_GIT_REPO"
        return result

    if not fetch_origin(repo_path):
        result["status"] = "FETCH_FAILED"
        return result

    target = find_latest_release_branch(repo_path)
    if not target:
        print("  [ERROR] No release branch found")
        result["status"] = "NO_RELEASE_BRANCH"
        return result

    cur = current_branch(repo_path)
    result["from"] = cur
    result["target"] = target

    if cur == target:
        # 已在目标分支，只 pull
        pull = run_git(repo_path, "pull", "origin", target)
        if pull.returncode != 0:
            print(f"  [WARN] pull failed: {pull.stderr.strip()}")
        print(f"  Already on {target} (pulled latest)")
        result["status"] = "UP_TO_DATE"
        return result

    if has_uncommitted_changes(repo_path):
        print(f"  [SKIP] Uncommitted changes detected, cannot switch to {target}")
        result["status"] = "DIRTY"
        return result

    if checkout_branch(repo_path, target):
        print(f"  Switched: {cur} -> {target}")
        result["status"] = "SWITCHED"
    else:
        result["status"] = "CHECKOUT_FAILED"

    return result


def main():
    parser = argparse.ArgumentParser(description="Sync repos to latest release branch")
    parser.add_argument("--dry-run", action="store_true", help="Only show target branches, don't switch")
    args = parser.parse_args()

    config = load_config()
    repos = {
        name: r["path"]
        for name, r in _get_repos_cfg(config, git_only=True).items()
    }
    if not repos:
        print("Error: No code paths configured in config.yaml", file=sys.stderr)
        sys.exit(1)

    if args.dry_run:
        print("=== Dry Run: showing target branches ===")
        for name, path in repos.items():
            fetch_origin(path)
            target = find_latest_release_branch(path)
            cur = current_branch(path)
            status = "OK" if cur == target else "NEED_SWITCH"
            print(f"  [{name}] current={cur}  target={target}  ({status})")
        return

    print("=== Syncing repos to latest release branch ===")
    results = []
    for name, path in repos.items():
        results.append(sync_repo(name, path))

    print("\n=== Summary ===")
    for r in results:
        status = r["status"]
        target = r.get("target", "N/A")
        if status == "UP_TO_DATE":
            print(f"  [{r['name']}] Already on {target}")
        elif status == "SWITCHED":
            print(f"  [{r['name']}] {r['from']} -> {target}")
        elif status == "DIRTY":
            print(f"  [{r['name']}] SKIPPED (uncommitted changes), target was {target}")
        elif status == "PATH_NOT_FOUND":
            print(f"  [{r['name']}] FAILED (path not found: {r['path']})")
        elif status == "NOT_GIT_REPO":
            print(f"  [{r['name']}] FAILED (not a git repo: {r['path']})")
        else:
            print(f"  [{r['name']}] FAILED ({status})")


if __name__ == "__main__":
    main()
