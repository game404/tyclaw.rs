from __future__ import annotations

import io
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest.mock import patch

from scripts import send_dingtalk_markdown as target


class FakeResponse:
    def __init__(self, status_code: int, data=None, invalid_json: bool = False):
        self.status_code = status_code
        self._data = data
        self._invalid_json = invalid_json

    def json(self):
        if self._invalid_json:
            raise ValueError("raw secret response")
        return self._data


class SendDingTalkMarkdownTests(unittest.TestCase):
    def test_user_id_is_required_before_config_or_network_access(self):
        with patch.object(target, "load_dingtalk_config") as load_config:
            with self.assertRaises(SystemExit) as raised, redirect_stderr(io.StringIO()):
                target.main([])
        self.assertEqual(raised.exception.code, 2)
        load_config.assert_not_called()

    def test_empty_user_id_fails_before_config_or_network_access(self):
        with patch.object(target, "load_dingtalk_config") as load_config, redirect_stderr(
            io.StringIO()
        ):
            code = target.main(["--user-id", ""])
        self.assertEqual(code, 1)
        load_config.assert_not_called()

    def test_dry_run_is_redacted(self):
        output = io.StringIO()
        user_id = "sensitive-user-id-123"
        with patch.object(
            target,
            "load_dingtalk_config",
            return_value={"client_id": "sensitive-robot-code", "client_secret": "secret"},
        ), redirect_stdout(output):
            code = target.main(
                ["--user-id", user_id, "--dry-run", "--text", "sensitive-message-content"]
            )
        rendered = output.getvalue()
        self.assertEqual(code, 0)
        self.assertIn('"recipient_count": 1', rendered)
        self.assertNotIn(user_id, rendered)
        self.assertNotIn("sensitive-robot-code", rendered)
        self.assertNotIn("sensitive-message-content", rendered)

    def test_success_does_not_print_raw_response_or_process_key(self):
        output = io.StringIO()
        user_id = "sensitive-user-id-456"
        with patch.object(
            target,
            "load_dingtalk_config",
            return_value={"client_id": "app", "client_secret": "secret"},
        ), patch.object(target, "get_access_token", return_value="token"), patch.object(
            target,
            "send_markdown",
            return_value={"processQueryKey": "sensitive-process-key"},
        ), redirect_stdout(output):
            code = target.main(["--user-id", user_id])
        rendered = output.getvalue()
        self.assertEqual(code, 0)
        self.assertNotIn(user_id, rendered)
        self.assertNotIn("sensitive-process-key", rendered)

    def test_all_partial_failure_categories_return_two_and_are_redacted(self):
        output = io.StringIO()
        user_ids = ["sensitive-invalid", "sensitive-flow", "sensitive-filtered"]
        result = {
            "invalidStaffIdList": [user_ids[0]],
            "flowControlledStaffIdList": [user_ids[1]],
            "filteredStaffIdList": [user_ids[2]],
            "processQueryKey": "sensitive-process-key",
        }
        with patch.object(
            target,
            "load_dingtalk_config",
            return_value={"client_id": "app", "client_secret": "secret"},
        ), patch.object(target, "get_access_token", return_value="token"), patch.object(
            target, "send_markdown", return_value=result
        ), redirect_stdout(output):
            arguments = [item for user_id in user_ids for item in ("--user-id", user_id)]
            code = target.main(arguments)
        rendered = output.getvalue()
        self.assertEqual(code, 2)
        self.assertIn("无效=1", rendered)
        self.assertIn("限流=1", rendered)
        self.assertIn("过滤=1", rendered)
        for sensitive in [*user_ids, "sensitive-process-key"]:
            self.assertNotIn(sensitive, rendered)

    def test_known_api_error_is_sanitized(self):
        sensitive_id = "sensitive-user-id-in-message"
        response = FakeResponse(
            400,
            {
                "code": "staffId.notExisted",
                "message": f"staff {sensitive_id} not found",
                "requestId": "request-123",
            },
        )
        with patch.object(target.requests, "post", return_value=response):
            with self.assertRaises(target.DingTalkTestError) as raised:
                target.send_markdown("token", "robot", [sensitive_id], "title", "body")
        message = str(raised.exception)
        self.assertIn("staffId.notExisted", message)
        self.assertIn("request-123", message)
        self.assertNotIn(sensitive_id, message)

    def test_http_200_invalid_json_is_protocol_error_without_raw_body(self):
        response = FakeResponse(200, invalid_json=True)
        with patch.object(target.requests, "post", return_value=response):
            with self.assertRaises(target.DingTalkTestError) as raised:
                target.send_markdown("token", "robot", ["user"], "title", "body")
        self.assertIn("无效 JSON", str(raised.exception))
        self.assertNotIn("raw secret response", str(raised.exception))

    def test_config_and_api_errors_return_one(self):
        for failure in ("config", "api"):
            with self.subTest(failure=failure), redirect_stderr(io.StringIO()):
                if failure == "config":
                    patches = (
                        patch.object(
                            target,
                            "load_dingtalk_config",
                            side_effect=target.DingTalkTestError("配置错误"),
                        ),
                        patch.object(target, "get_access_token"),
                    )
                else:
                    patches = (
                        patch.object(
                            target,
                            "load_dingtalk_config",
                            return_value={"client_id": "app", "client_secret": "secret"},
                        ),
                        patch.object(
                            target,
                            "get_access_token",
                            side_effect=target.DingTalkTestError("API 错误"),
                        ),
                    )
                with patches[0], patches[1]:
                    code = target.main(["--user-id", "sensitive-user-id"])
                self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
