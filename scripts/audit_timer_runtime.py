#!/usr/bin/env python3
"""只读检查 TyClaw 运行版本、Skill 超时配置和定时任务迁移风险。

默认不输出任务正文、用户标识、配置密钥、完整进程命令或 workspace 路径。
本脚本不修改文件、不停止服务，也不连接外部网络。
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Tuple


DETACHED_RE = re.compile(r"(?:^|[\s;&|])(setsid|nohup)(?:$|[\s;&|])|(?<![>&])&(?![&0-9])", re.I)
POLLING_RE = re.compile(r"(?:^|[\s;&|])(sleep|ps|tail)(?:$|[\s;&|])", re.I)
SHELL_RE = re.compile(
    r"(?:^|[\s;&|])(python(?:3(?:\.\d+)?)?|sh|bash|zsh)(?:$|[\s;&|])"
    r"|/skills/|/scripts/|(?:^|[\s;&|])[^\s;&|]+\.sh(?:$|[\s;&|])",
    re.I,
)
WRAPPER_RE = re.compile(r"(?:driver|wrapper)[^\s/]*\.sh|qa_refresh_driver\.sh", re.I)
SKILL_REFERENCE_RE = re.compile(
    r"/skills/|(?:^|[^a-z0-9_-])finance-[a-z0-9-]+(?:$|[^a-z0-9_-])"
    r"|(?:^|[\s：:（(])skill(?:\.md)?(?:$|[\s：:）。)])",
    re.I,
)
MANAGED_FOREGROUND_MARKERS = ("单次前台 exec", "禁止", "skill_execution 上限")


def fingerprint(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8", errors="replace")).hexdigest()[:12]


def sha256_file(path: Path) -> Optional[str]:
    try:
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest()
    except OSError:
        return None


def file_metadata(path: Path) -> Dict[str, Any]:
    result: Dict[str, Any] = {"exists": path.is_file()}
    if not result["exists"]:
        return result
    try:
        stat = path.stat()
        result.update(
            {
                "size": stat.st_size,
                "mtime": datetime.fromtimestamp(stat.st_mtime, timezone.utc).astimezone().isoformat(),
                "sha256": sha256_file(path),
            }
        )
    except OSError:
        result["metadata_error"] = True
    return result


def _strip_yaml_value(value: str) -> str:
    value = value.split(" #", 1)[0].strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
        return value[1:-1]
    return value


def _positive_int(value: str) -> Optional[int]:
    value = _strip_yaml_value(value)
    if not value.isdigit():
        return None
    parsed = int(value)
    return parsed if parsed > 0 else None


def extract_skill_execution_config(text: str) -> Dict[str, Any]:
    """解析 config.yaml 的安全子集，不加载或返回其他配置节。"""
    result: Dict[str, Any] = {
        "present": False,
        "default_timeout_secs": None,
        "skills": {},
        "parse_warnings": [],
    }
    lines = text.splitlines()
    section_start: Optional[int] = None
    section_indent = 0
    for index, raw in enumerate(lines):
        match = re.match(r"^(\s*)skill_execution\s*:\s*(?:#.*)?$", raw)
        if match:
            section_start = index + 1
            section_indent = len(match.group(1))
            result["present"] = True
            break
    if section_start is None:
        return result

    in_skills = False
    current_skill: Optional[str] = None
    skills_indent = 0
    current_skill_indent = 0
    for raw in lines[section_start:]:
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        indent = len(raw) - len(raw.lstrip())
        if indent <= section_indent:
            break
        stripped = raw.strip()
        default_match = re.match(r"default_timeout_secs\s*:\s*(.+)$", stripped)
        if default_match and not in_skills:
            result["default_timeout_secs"] = _positive_int(default_match.group(1))
            if result["default_timeout_secs"] is None:
                result["parse_warnings"].append("invalid_default_timeout")
            continue
        if re.match(r"skills\s*:\s*(?:#.*)?$", stripped):
            in_skills = True
            skills_indent = indent
            current_skill = None
            continue
        if not in_skills or indent <= skills_indent:
            continue
        skill_match = re.match(r"([^:#][^:]*)\s*:\s*(?:#.*)?$", stripped)
        if skill_match:
            current_skill = _strip_yaml_value(skill_match.group(1).strip())
            current_skill_indent = indent
            continue
        timeout_match = re.match(r"timeout_secs\s*:\s*(.+)$", stripped)
        if timeout_match and current_skill and indent > current_skill_indent:
            timeout = _positive_int(timeout_match.group(1))
            if timeout is None:
                result["parse_warnings"].append(
                    "invalid_skill_timeout:" + fingerprint(current_skill)
                )
            else:
                result["skills"][current_skill] = timeout
    return result


def classify_target(message: str) -> str:
    lower = message.lower()
    if any(
        token in lower
        for token in (
            "finance-payment",
            "run_payment_report.py",
            "重点付款",
        )
    ):
        return "finance_payment"
    if any(
        token in lower
        for token in (
            "finance-revenue-qa",
            "finance-revenue",
            "revenue_qa.py",
            "run_ar_report.py",
            "qa_refresh_driver",
            "回款问答",
            "回款刷新",
        )
    ):
        return "revenue_refresh"
    return "other"


def classify_message(message: str) -> Tuple[str, str, List[str]]:
    target = classify_target(message)
    lower = message.lower()
    skill = bool(SKILL_REFERENCE_RE.search(message)) or target != "other"
    managed_foreground = skill and all(marker in lower for marker in MANAGED_FOREGROUND_MARKERS)
    if managed_foreground:
        return target, "managed_skill_foreground", []
    risk_flags: List[str] = []
    if DETACHED_RE.search(message):
        risk_flags.append("detached_process")
    if POLLING_RE.search(message):
        risk_flags.append("polling")
    if WRAPPER_RE.search(message):
        risk_flags.append("wrapper_script")
    shell = bool(SHELL_RE.search(message))
    if shell and skill:
        execution_class = "legacy_skill_shell"
    elif shell:
        execution_class = "legacy_shell"
    elif skill:
        execution_class = "legacy_skill_agent"
    else:
        execution_class = "agent_message"
    return target, execution_class, sorted(risk_flags)


def summarize_schedule(schedule: Any) -> Dict[str, Any]:
    if not isinstance(schedule, dict):
        return {"kind": "invalid"}
    kind = str(schedule.get("kind", "unknown"))
    if kind == "cron":
        return {
            "kind": kind,
            "expr": str(schedule.get("expr", ""))[:80],
            "tz": str(schedule.get("tz") or "local")[:80],
        }
    if kind == "every":
        return {"kind": kind, "interval_ms": schedule.get("interval_ms")}
    if kind == "at":
        return {"kind": kind, "at_ms": schedule.get("at_ms")}
    return {"kind": kind}


def summarize_job(job: Dict[str, Any]) -> Dict[str, Any]:
    payload = job.get("payload") if isinstance(job.get("payload"), dict) else {}
    message = payload.get("message") if isinstance(payload.get("message"), str) else ""
    target, execution_class, risk_flags = classify_message(message)
    state = job.get("state") if isinstance(job.get("state"), dict) else {}
    channel = payload.get("channel")
    safe_channels = {"cli", "dingtalk", "dingtalk_group", "dingtalk_private"}
    return {
        "job_id": str(job.get("id", "missing"))[:64],
        "enabled": bool(job.get("enabled", True)),
        "schedule": summarize_schedule(job.get("schedule")),
        "delivery_channel": channel if channel in safe_channels else ("unset" if not channel else "other"),
        "target": target,
        "execution_class": execution_class,
        "migration_required": execution_class.startswith("legacy_"),
        "risk_flags": risk_flags,
        "last_status": str(state.get("last_status") or "unknown")[:40],
        "last_run_at_ms": state.get("last_run_at_ms"),
        "next_run_at_ms": state.get("next_run_at_ms"),
    }


def scan_timer_stores(works_dir: Path) -> Dict[str, Any]:
    jobs: List[Dict[str, Any]] = []
    parse_errors: List[Dict[str, str]] = []
    store_count = 0
    if works_dir.is_dir():
        paths: Iterable[Path] = works_dir.glob("*/*/timer_jobs.json")
    else:
        paths = []
    for path in sorted(paths):
        store_count += 1
        store_id = fingerprint(str(path.parent.relative_to(works_dir)))
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            raw_jobs = data.get("jobs", []) if isinstance(data, dict) else []
            if not isinstance(raw_jobs, list):
                raise ValueError("jobs_not_list")
            for raw_job in raw_jobs:
                if not isinstance(raw_job, dict):
                    parse_errors.append({"store": store_id, "error": "job_not_object"})
                    continue
                item = summarize_job(raw_job)
                item["store"] = store_id
                jobs.append(item)
        except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
            parse_errors.append({"store": store_id, "error": type(error).__name__})

    class_counts = Counter(job["execution_class"] for job in jobs)
    target_counts = Counter(job["target"] for job in jobs)
    risk_counts = Counter(flag for job in jobs for flag in job["risk_flags"])
    return {
        "works_dir_exists": works_dir.is_dir(),
        "store_count": store_count,
        "job_count": len(jobs),
        "enabled_job_count": sum(1 for job in jobs if job["enabled"]),
        "migration_required_count": sum(1 for job in jobs if job["migration_required"]),
        "class_counts": dict(sorted(class_counts.items())),
        "target_counts": dict(sorted(target_counts.items())),
        "risk_counts": dict(sorted(risk_counts.items())),
        "jobs": jobs,
        "parse_errors": parse_errors,
    }


def run_readonly(command: List[str], cwd: Optional[Path] = None) -> Tuple[Optional[str], Optional[str]]:
    try:
        completed = subprocess.run(
            command,
            cwd=str(cwd) if cwd else None,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return None, type(error).__name__
    if completed.returncode != 0:
        return None, "exit_" + str(completed.returncode)
    return completed.stdout.strip(), None


def summarize_docker_top(output: str) -> Dict[str, int]:
    rows = [line.split() for line in output.splitlines() if line.strip()]
    if rows and rows[0] and rows[0][0].upper() == "PID":
        rows = rows[1:]
    commands = [row[3].lower() for row in rows if len(row) >= 4]
    python_count = sum(1 for command in commands if command.startswith("python"))
    shell_count = sum(1 for command in commands if command in {"sh", "bash", "zsh"})
    ignored_count = sum(1 for command in commands if command in {"sleep"})
    return {
        "process_count": len(commands),
        "python_count": python_count,
        "shell_count": shell_count,
        "other_process_count": len(commands) - python_count - shell_count - ignored_count,
    }


def inspect_containers() -> Dict[str, Any]:
    output, error = run_readonly(
        ["docker", "ps", "--filter", "name=tyclaw-", "--format", "{{.Names}}"]
    )
    if output is None:
        return {"available": False, "error": error, "containers": []}
    containers: List[Dict[str, Any]] = []
    for name in sorted(line.strip() for line in output.splitlines() if line.strip()):
        top, top_error = run_readonly(
            ["docker", "top", name, "-eo", "pid,ppid,pgid,comm"]
        )
        item: Dict[str, Any] = {"container": fingerprint(name)}
        if top is None:
            item.update({"available": False, "error": top_error})
        else:
            item.update({"available": True, **summarize_docker_top(top)})
        containers.append(item)
    return {"available": True, "container_count": len(containers), "containers": containers}


def inspect_git(repo_dir: Path) -> Dict[str, Any]:
    head, head_error = run_readonly(["git", "rev-parse", "HEAD"], repo_dir)
    commit_time, time_error = run_readonly(
        ["git", "show", "-s", "--format=%cI", "HEAD"], repo_dir
    )
    return {
        "available": head is not None,
        "head": head,
        "head_commit_time": commit_time,
        "error": head_error or time_error,
    }


def inspect_service(service: str, run_dir: Path, repo_dir: Path) -> Dict[str, Any]:
    properties = [
        "ActiveState",
        "SubState",
        "MainPID",
        "ExecMainStartTimestamp",
        "ActiveEnterTimestamp",
    ]
    output, error = run_readonly(
        ["systemctl", "--user", "show", service, "--property=" + ",".join(properties)]
    )
    result: Dict[str, Any] = {"available": output is not None, "error": error}
    values: Dict[str, str] = {}
    if output:
        for line in output.splitlines():
            key, separator, value = line.partition("=")
            if separator and key in properties:
                values[key] = value
    pid_text = values.get("MainPID", "0")
    pid = int(pid_text) if pid_text.isdigit() else 0
    result.update(
        {
            "active_state": values.get("ActiveState", "unknown"),
            "sub_state": values.get("SubState", "unknown"),
            "main_pid": pid or None,
            "exec_start": values.get("ExecMainStartTimestamp") or None,
            "active_since": values.get("ActiveEnterTimestamp") or None,
        }
    )

    deployed = file_metadata(run_dir / "tyclaw")
    built = file_metadata(repo_dir / "target" / "release" / "tyclaw")
    running: Dict[str, Any] = {"exists": False}
    if pid:
        proc_exe = Path("/proc") / str(pid) / "exe"
        try:
            resolved = proc_exe.resolve(strict=True)
            running = file_metadata(resolved)
            running["matches_deployed"] = bool(
                running.get("sha256")
                and running.get("sha256") == deployed.get("sha256")
            )
        except OSError:
            running = {"exists": False, "read_error": True}
    deployed["matches_local_release"] = bool(
        deployed.get("sha256") and deployed.get("sha256") == built.get("sha256")
    )
    result["binary"] = {"running": running, "deployed": deployed, "local_release": built}
    return result


def inspect_config(config_path: Path) -> Dict[str, Any]:
    if not config_path.is_file():
        return {"exists": False, "skill_execution": extract_skill_execution_config("")}
    try:
        text = config_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        return {"exists": True, "read_error": type(error).__name__}
    return {"exists": True, "skill_execution": extract_skill_execution_config(text)}


def build_report(args: argparse.Namespace) -> Dict[str, Any]:
    repo_dir = Path(args.repo_dir).resolve()
    run_dir = Path(args.run_dir).resolve()
    report: Dict[str, Any] = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).astimezone().isoformat(),
        "mode": "read_only_redacted",
        "git": inspect_git(repo_dir),
        "config": inspect_config(run_dir / "config" / "config.yaml"),
        "timers": scan_timer_stores(run_dir / "works"),
        "containers": inspect_containers(),
    }
    report["service"] = (
        {"skipped": True}
        if args.skip_systemd
        else inspect_service(args.service, run_dir, repo_dir)
    )
    return report


def render_text(report: Dict[str, Any]) -> str:
    lines = [
        "TyClaw timer/runtime audit (read-only, redacted)",
        "generated_at=" + report["generated_at"],
    ]
    git = report["git"]
    lines.append(
        "git head={} commit_time={}".format(
            (git.get("head") or "unavailable")[:12], git.get("head_commit_time") or "unavailable"
        )
    )
    service = report["service"]
    if service.get("skipped"):
        lines.append("service check=skipped")
    else:
        lines.append(
            "service active={} sub={} pid={} started={}".format(
                service.get("active_state"),
                service.get("sub_state"),
                service.get("main_pid"),
                service.get("exec_start") or "unknown",
            )
        )
        binary = service.get("binary", {})
        lines.append(
            "binary running_matches_deployed={} deployed_matches_local_release={}".format(
                binary.get("running", {}).get("matches_deployed"),
                binary.get("deployed", {}).get("matches_local_release"),
            )
        )
    skill = report.get("config", {}).get("skill_execution", {})
    lines.append(
        "skill_execution present={} default_timeout_secs={} overrides={}".format(
            skill.get("present"),
            skill.get("default_timeout_secs"),
            json.dumps(skill.get("skills", {}), ensure_ascii=False, sort_keys=True),
        )
    )
    timers = report["timers"]
    lines.append(
        "timers stores={} jobs={} enabled={} migration_required={} parse_errors={}".format(
            timers["store_count"],
            timers["job_count"],
            timers["enabled_job_count"],
            timers["migration_required_count"],
            len(timers["parse_errors"]),
        )
    )
    lines.append("timer_classes=" + json.dumps(timers["class_counts"], sort_keys=True))
    lines.append("timer_targets=" + json.dumps(timers["target_counts"], sort_keys=True))
    lines.append("timer_risks=" + json.dumps(timers["risk_counts"], sort_keys=True))
    containers = report["containers"]
    lines.append(
        "containers available={} count={}".format(
            containers.get("available"), containers.get("container_count", 0)
        )
    )
    for container in containers.get("containers", []):
        lines.append(
            "container id={} available={} processes={} python={} shell={} other={}".format(
                container["container"],
                container.get("available"),
                container.get("process_count", "unknown"),
                container.get("python_count", "unknown"),
                container.get("shell_count", "unknown"),
                container.get("other_process_count", "unknown"),
            )
        )
    for job in timers["jobs"]:
        schedule = json.dumps(job["schedule"], ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        lines.append(
            "job id={} store={} enabled={} target={} class={} migration={} risks={} schedule={} last={}".format(
                job["job_id"],
                job["store"],
                job["enabled"],
                job["target"],
                job["execution_class"],
                job["migration_required"],
                ",".join(job["risk_flags"]) or "none",
                schedule,
                job["last_status"],
            )
        )
    for error in timers["parse_errors"]:
        lines.append("parse_error store={} type={}".format(error["store"], error["error"]))
    return "\n".join(lines)


def parse_args(argv: Optional[List[str]] = None) -> argparse.Namespace:
    script_repo = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-dir", default=str(script_repo), help="tyclaw.rs Git 仓库目录")
    parser.add_argument("--run-dir", default=str(script_repo / "workspace"), help="TyClaw run-dir")
    parser.add_argument("--service", default="tyclaw.service", help="systemd user service 名称")
    parser.add_argument("--skip-systemd", action="store_true", help="跳过 systemd 与 /proc 检查")
    parser.add_argument("--json", action="store_true", help="输出脱敏 JSON")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="发现需迁移任务、解析错误或服务非 active 时返回退出码 1",
    )
    return parser.parse_args(argv)


def main(argv: Optional[List[str]] = None) -> int:
    args = parse_args(argv)
    report = build_report(args)
    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
    else:
        print(render_text(report))
    if not args.strict:
        return 0
    timers = report["timers"]
    service = report["service"]
    unhealthy_service = not service.get("skipped") and service.get("active_state") != "active"
    return int(
        timers["migration_required_count"] > 0
        or bool(timers["parse_errors"])
        or unhealthy_service
    )


if __name__ == "__main__":
    sys.exit(main())
