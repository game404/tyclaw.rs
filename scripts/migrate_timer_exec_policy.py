#!/usr/bin/env python3
"""将已知旧 Timer 正文迁移为受管的单次前台 Skill 执行约束。

默认仅 dry-run；只有显式传入 --apply 才原子写入并保留备份。
"""

import argparse
import json
import os
import shutil
import tempfile
from datetime import datetime, timezone
from pathlib import Path


PAYMENT_PREFIXES = ("1aa59c11", "954631b2")
REVENUE_PREFIXES = ("c9cd54b0",)
KNOWN_OTHER_PREFIXES = ("52bb6d84",)
RISK_TOKENS = ("setsid", "nohup", "sleep ", "ps ", "tail ", " &", "driver.sh", "driver.py")

PAYMENT_MESSAGE = """执行重点付款 Skill（finance-payment）。必须直接调用 SKILL.md 指定的正式入口，使用单次前台 exec 并等待退出；禁止 setsid、nohup、后台 &、sleep/ps/tail 轮询以及 work/tmp 下的临时 Python/Shell wrapper。执行时长由 skill_execution 上限管理。仅发送本次生成且非空的 Excel 文件；脚本失败或没有有效文件时明确报错，不发送历史文件。"""

REVENUE_MESSAGE = """执行回款刷新流程。按各自 SKILL.md 的正式入口，先以前台 exec 运行 finance-revenue 并等待退出，再以前台 exec 运行 finance-revenue-qa 并等待退出；前一步失败立即停止。禁止生成临时 driver、setsid、nohup、后台 & 或 sleep/ps/tail 轮询。执行时长分别由 skill_execution 上限管理。只发送本次成功生成且非空的产物。"""

OTHER_MESSAGE = """执行该任务对应的 Skill。必须直接调用 SKILL.md 指定的正式入口，使用单次前台 exec 并等待退出；禁止 setsid、nohup、后台 &、sleep/ps/tail 轮询以及临时 Python/Shell wrapper。执行时长由 skill_execution 上限管理。"""


def replacement(job_id):
    if job_id.startswith(PAYMENT_PREFIXES):
        return PAYMENT_MESSAGE
    if job_id.startswith(REVENUE_PREFIXES):
        return REVENUE_MESSAGE
    if job_id.startswith(KNOWN_OTHER_PREFIXES):
        return OTHER_MESSAGE
    return None


def has_risk(message):
    lowered = message.lower()
    return any(token in lowered for token in RISK_TOKENS)


def write_atomic(path, data):
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    backup = path.with_name(f"{path.name}.bak.{stamp}")
    shutil.copy2(path, backup)
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(data, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp_name, path)
    except Exception:
        try:
            os.unlink(temp_name)
        except FileNotFoundError:
            pass
        raise


def process_store(path, apply):
    store = json.loads(path.read_text(encoding="utf-8"))
    changed = 0
    manual = 0
    for job in store.get("jobs", []):
        job_id = str(job.get("id", ""))
        payload = job.get("payload") or {}
        message = str(payload.get("message", ""))
        target = replacement(job_id)
        if target is not None and message != target:
            payload["message"] = target
            job["payload"] = payload
            changed += 1
            print(f"job={job_id[:8]} action={'migrated' if apply else 'would_migrate'}")
        elif target is None and has_risk(message):
            manual += 1
            print(f"job={job_id[:8]} action=manual_review_required")
    if apply and changed:
        write_atomic(path, store)
    return changed, manual


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace-root", type=Path, required=True, help="包含 works/ 的 TyClaw workspace 根目录")
    parser.add_argument("--apply", action="store_true", help="备份后原子写入；默认只检查")
    args = parser.parse_args()

    stores = sorted((args.workspace_root / "works").glob("*/*/timer_jobs.json"))
    changed = manual = 0
    for store in stores:
        store_changed, store_manual = process_store(store, args.apply)
        changed += store_changed
        manual += store_manual
    label = "migrated" if args.apply else "would_migrate"
    print(f"mode={'apply' if args.apply else 'dry-run'} stores={len(stores)} {label}={changed} manual_review_required={manual}")


if __name__ == "__main__":
    main()
