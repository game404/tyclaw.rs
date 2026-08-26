import json
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "migrate_timer_exec_policy.py"


class TimerMigrationTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.store = self.root / "works" / "ab" / "workspace" / "timer_jobs.json"
        self.store.parent.mkdir(parents=True)
        self.original = {
            "version": 1,
            "jobs": [
                {
                    "id": "1aa59c11-full-id",
                    "name": "重点付款",
                    "payload": {"message": "运行 finance-payment；setsid 后台执行，再 sleep 20、ps、tail 轮询"},
                    "schedule": {"kind": "cron", "expr": "0 0 * * *"},
                    "state": {"last_status": "failed"},
                },
                {
                    "id": "c9cd54b0-full-id",
                    "name": "回款刷新",
                    "payload": {"message": "生成 qa_refresh_driver.sh 并用 setsid 后台运行"},
                    "schedule": {"kind": "cron", "expr": "0 3 * * *"},
                    "state": {},
                },
                {
                    "id": "faa28743-full-id",
                    "name": "未知任务",
                    "payload": {"message": "请运行 unknown_daily.sh 生成每日数据"},
                    "schedule": {"kind": "cron", "expr": "0 19 * * *"},
                    "state": {},
                },
            ],
        }
        self.store.write_text(json.dumps(self.original, ensure_ascii=False), encoding="utf-8")

    def tearDown(self):
        self.temp.cleanup()

    def run_script(self, *args):
        return subprocess.run(
            ["python3", str(SCRIPT), "--workspace-root", str(self.root), *args],
            check=True, capture_output=True, text=True,
        )

    def test_dry_run_reports_changes_without_writing(self):
        result = self.run_script()
        self.assertEqual(json.loads(self.store.read_text(encoding="utf-8")), self.original)
        self.assertIn("would_migrate=2", result.stdout)
        self.assertIn("manual_review_required=1", result.stdout)
        self.assertFalse(list(self.store.parent.glob("timer_jobs.json.bak.*")))

    def test_apply_preserves_identity_schedule_state_and_creates_backup(self):
        self.run_script("--apply")
        migrated = json.loads(self.store.read_text(encoding="utf-8"))
        for before, after in zip(self.original["jobs"], migrated["jobs"]):
            self.assertEqual(after["id"], before["id"])
            self.assertEqual(after["schedule"], before["schedule"])
            self.assertEqual(after["state"], before["state"])
        payment = migrated["jobs"][0]["payload"]["message"]
        revenue = migrated["jobs"][1]["payload"]["message"]
        self.assertIn("单次前台 exec", payment)
        self.assertIn("本次生成且非空", payment)
        self.assertIn("finance-revenue", revenue)
        self.assertIn("finance-revenue-qa", revenue)
        self.assertIn("前一步失败立即停止", revenue)
        self.assertEqual(migrated["jobs"][2], self.original["jobs"][2])
        backups = list(self.store.parent.glob("timer_jobs.json.bak.*"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(json.loads(backups[0].read_text(encoding="utf-8")), self.original)


if __name__ == "__main__":
    unittest.main()
