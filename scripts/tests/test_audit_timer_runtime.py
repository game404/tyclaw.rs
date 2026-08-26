import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "audit_timer_runtime.py"
SPEC = importlib.util.spec_from_file_location("audit_timer_runtime", SCRIPT)
audit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(audit)


class AuditTimerRuntimeTests(unittest.TestCase):
    def test_extracts_only_skill_execution_config(self):
        config = """
provider:
  api_key: super-secret
skill_execution:
  default_timeout_secs: 1800
  skills:
    finance-payment:
      timeout_secs: 2700
    finance-revenue:
      timeout_secs: 7200
"""

        result = audit.extract_skill_execution_config(config)

        self.assertEqual(result["default_timeout_secs"], 1800)
        self.assertEqual(result["skills"]["finance-payment"], 2700)
        self.assertEqual(result["skills"]["finance-revenue"], 7200)
        self.assertNotIn("super-secret", json.dumps(result))

    def test_classifies_legacy_jobs_without_returning_payload_or_user(self):
        secret = "SENSITIVE-PAYLOAD-MARKER"
        jobs = [
            {
                "id": "1aa59c11",
                "name": "payment",
                "user_id": "private-user-id",
                "enabled": True,
                "schedule": {
                    "kind": "cron",
                    "expr": "0 0 * * *",
                    "tz": "Asia/Shanghai",
                },
                "payload": {
                    "user_id": "private-user-id",
                    "message": (
                        "setsid python3 /workspace/skills/finance/finance-payment/"
                        f"scripts/run_payment_report.py {secret} & sleep 20"
                    ),
                },
                "state": {"last_status": "error", "last_error": secret},
            },
            {
                "id": "c9cd54b0",
                "name": "refresh",
                "user_id": "another-private-user",
                "enabled": True,
                "schedule": {"kind": "cron", "expr": "0 3 * * *"},
                "payload": {
                    "message": (
                        "write qa_refresh_driver.sh then setsid sh qa_refresh_driver.sh &"
                        "; run revenue_qa.py"
                    )
                },
                "state": {"last_status": "success"},
            },
            {
                "id": "reminder1",
                "enabled": True,
                "schedule": {"kind": "at", "at_ms": 1780000000000},
                "payload": {"message": "提醒我开会"},
                "state": {},
            },
        ]

        results = [audit.summarize_job(job) for job in jobs]
        encoded = json.dumps(results, ensure_ascii=False)

        self.assertEqual(results[0]["target"], "finance_payment")
        self.assertEqual(results[0]["execution_class"], "legacy_skill_shell")
        self.assertIn("detached_process", results[0]["risk_flags"])
        self.assertIn("polling", results[0]["risk_flags"])
        self.assertEqual(results[1]["target"], "revenue_refresh")
        self.assertIn("wrapper_script", results[1]["risk_flags"])
        self.assertEqual(results[2]["execution_class"], "agent_message")
        self.assertNotIn(secret, encoded)
        self.assertNotIn("private-user", encoded)
        self.assertNotIn("message", results[0])
        self.assertNotIn("last_error", results[0])

    def test_scans_stores_and_reports_parse_errors_by_fingerprint(self):
        with tempfile.TemporaryDirectory() as tmp:
            works = Path(tmp) / "works"
            good = works / "aa" / "private-user-a"
            bad = works / "bb" / "private-user-b"
            good.mkdir(parents=True)
            bad.mkdir(parents=True)
            (good / "timer_jobs.json").write_text(
                json.dumps(
                    {
                        "version": 1,
                        "jobs": [
                            {
                                "id": "job00001",
                                "enabled": True,
                                "schedule": {"kind": "cron", "expr": "0 1 * * *"},
                                "payload": {"message": "普通提醒"},
                                "state": {},
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            (bad / "timer_jobs.json").write_text("not-json-private-value", encoding="utf-8")

            result = audit.scan_timer_stores(works)
            encoded = json.dumps(result, ensure_ascii=False)

            self.assertEqual(result["store_count"], 2)
            self.assertEqual(result["job_count"], 1)
            self.assertEqual(len(result["parse_errors"]), 1)
            self.assertNotIn("private-user-a", encoded)
            self.assertNotIn("private-user-b", encoded)
            self.assertNotIn("not-json-private-value", encoded)

    def test_natural_language_skill_task_requires_migration(self):
        result = audit.summarize_job(
            {
                "id": "natural1",
                "enabled": True,
                "schedule": {"kind": "cron", "expr": "0 4 * * *"},
                "payload": {"message": "每天用 finance-revenue 刷新回款数据"},
                "state": {},
            }
        )

        self.assertEqual(result["execution_class"], "legacy_skill_agent")
        self.assertTrue(result["migration_required"])

    def test_managed_foreground_skill_message_is_not_a_legacy_risk(self):
        result = audit.summarize_job(
            {
                "id": "1aa59c11",
                "enabled": True,
                "schedule": {"kind": "cron", "expr": "0 0 * * *"},
                "payload": {
                    "message": (
                        "执行重点付款 Skill（finance-payment）。必须直接调用 SKILL.md "
                        "指定的正式入口，使用单次前台 exec 并等待退出；"
                        "禁止 setsid、nohup、后台 &、sleep/ps/tail 轮询以及临时 wrapper。"
                        "执行时长由 skill_execution 上限管理。"
                    )
                },
                "state": {},
            }
        )

        self.assertEqual(result["execution_class"], "managed_skill_foreground")
        self.assertFalse(result["migration_required"])
        self.assertEqual(result["risk_flags"], [])

    def test_managed_generic_skill_md_message_is_not_flagged(self):
        result = audit.summarize_job(
            {
                "id": "52bb6d84",
                "enabled": True,
                "schedule": {"kind": "cron", "expr": "0 3 * * *"},
                "payload": {
                    "message": (
                        "执行该任务对应的 Skill。必须直接调用 SKILL.md 指定的正式入口，"
                        "使用单次前台 exec 并等待退出；禁止 setsid、nohup、后台 &。"
                        "执行时长由 skill_execution 上限管理。"
                    )
                },
                "state": {},
            }
        )

        self.assertEqual(result["execution_class"], "managed_skill_foreground")
        self.assertFalse(result["migration_required"])
        self.assertEqual(result["risk_flags"], [])

    def test_summarizes_docker_processes_without_arguments(self):
        output = """PID PPID PGID COMMAND
1 0 1 sleep
163 1 163 python3
170 163 163 sh
171 163 163 curl
"""

        result = audit.summarize_docker_top(output)

        self.assertEqual(result["process_count"], 4)
        self.assertEqual(result["python_count"], 1)
        self.assertEqual(result["shell_count"], 1)
        self.assertEqual(result["other_process_count"], 1)
        self.assertNotIn("arguments", result)


if __name__ == "__main__":
    unittest.main()
