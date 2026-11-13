"""The client and the generated emitters against a fake Beacon: what is sent, retries, and results.

Run from clients/python: `python3 -m unittest discover -s tests`.
"""

from __future__ import annotations

import json
import os
import sys
import threading
import unittest
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import cast
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE.parent), str(HERE / "gen")]

import assistant_kinds  # noqa: E402 - found through the path set above
import backup_kinds  # noqa: E402
import beacon_client  # noqa: E402
import ship_kinds  # noqa: E402


class Request:
    def __init__(self, method: str, path: str, headers: dict[str, str], body: object) -> None:
        self.method = method
        self.path = path
        self.headers = headers
        self.body = body


class FakeBeacon:
    """A server that records each request and answers from a script (then 200 with an item)."""

    def __init__(self) -> None:
        self.requests: list[Request] = []
        self.answers: list[tuple[int, object]] = []
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                body: object = json.loads(self.rfile.read(length) or b"null")
                headers = {name.lower(): value for name, value in self.headers.items()}
                fake.requests.append(Request("POST", self.path, headers, body))
                status, answer = fake.answers.pop(0) if fake.answers else (200, {"outcome": "opened", "item": {"id": 7}})
                data = json.dumps(answer).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, format: str, *args: object) -> None:
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True).start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def body(self, index: int = -1) -> dict[str, object]:
        body = self.requests[index].body
        assert isinstance(body, dict)
        return cast("dict[str, object]", body)


class ClientTest(unittest.TestCase):
    def setUp(self) -> None:
        self.beacon = FakeBeacon()
        self.env = mock.patch.dict(os.environ, {"BEACON_URL": self.beacon.url}, clear=False)
        self.env.start()
        for name in ("BEACON_TOKEN", "BEACON_STRICT", "BEACON_DISABLED"):
            os.environ.pop(name, None)
        # No real waiting between attempts; the delays asked for are recorded.
        self.sleeps: list[float] = []
        self.sleep = mock.patch("time.sleep", side_effect=self.sleeps.append)
        self.sleep.start()

    def tearDown(self) -> None:
        self.sleep.stop()
        self.env.stop()
        self.beacon.close()

    def test_a_generated_function_sends_its_kind_facts_and_slots(self) -> None:
        os.environ["BEACON_TOKEN"] = "t0ken"
        at = datetime(2026, 11, 13, 2, 0, tzinfo=timezone.utc)
        sent = backup_kinds.backup_failed(
            last_ok=at, reason="The disk is full", key="nightly", labels={"service": "backup"}, url="/system/backups"
        )
        self.assertEqual(sent, backup_kinds.Sent(ok=True, status=200, outcome="opened", item=7, problem=None))
        request = self.beacon.requests[0]
        self.assertEqual(request.path, "/v1/events")
        self.assertEqual(request.headers["content-type"], "application/json")
        self.assertEqual(request.headers["authorization"], "Bearer t0ken")
        self.assertRegex(request.headers["idempotency-key"], r"^[0-9a-f]{32}$")
        self.assertEqual(
            self.beacon.body(),
            {
                "kind": "backup.failed",
                "key": "nightly",
                "facts": {"last_ok": 1794535200000},
                "slots": {"reason": "The disk is full"},
                "labels": {"service": "backup"},
                "url": "/system/backups",
            },
        )

    def test_durations_and_times_are_whole_milliseconds(self) -> None:
        backup_kinds.backup_finished(size="2 GB", took=timedelta(minutes=4))
        backup_kinds.backup_stale(since=90000, last_ok=1794535200000)
        self.assertEqual(self.beacon.body(0)["facts"], {"size": "2 GB", "took": 240000})
        self.assertEqual(self.beacon.body(1)["facts"], {"since": 90000, "last_ok": 1794535200000})
        self.assertNotIn("authorization", self.beacon.requests[0].headers)

    def test_a_naive_datetime_is_refused(self) -> None:
        with self.assertRaises(TypeError):
            backup_kinds.backup_failed(last_ok=datetime(2026, 11, 13, 2, 0), reason="x")
        self.assertEqual(self.beacon.requests, [])

    def test_resolving_sends_the_state_and_no_values(self) -> None:
        backup_kinds.backup_failed_resolved(key="nightly")
        self.assertEqual(self.beacon.body(), {"kind": "backup.failed", "key": "nightly", "state": "resolved"})
        assistant_kinds.assistant_approval_resolved(context="assistant:thread:42")
        self.assertEqual(self.beacon.body()["context"], "assistant:thread:42")

    def test_a_context_kind_sends_its_context(self) -> None:
        assistant_kinds.assistant_approval(thread="42", question="Delete the old logs?", context="assistant:thread:42")
        body = self.beacon.body()
        self.assertEqual(body["context"], "assistant:thread:42")
        self.assertEqual(body["facts"], {"thread": "42"})
        self.assertNotIn("labels", body)

    def test_a_category_is_sent_only_when_given(self) -> None:
        thread, tool, command, context = "42", "shell", "rm -r old-logs", "assistant:thread:42"
        assistant_kinds.assistant_permission(
            thread=thread, tool=tool, command=command, context=context, category="PERMISSION_DESTRUCTIVE"
        )
        self.assertEqual(self.beacon.body()["category"], "PERMISSION_DESTRUCTIVE")
        assistant_kinds.assistant_permission(thread=thread, tool=tool, command=command, context=context)
        self.assertNotIn("category", self.beacon.body())
        entry = assistant_kinds.assistant_permission_entry(
            thread=thread, tool=tool, command=command, context=context, category="PERMISSION"
        )
        assistant_kinds.snapshot([entry])
        self.assertEqual(
            self.beacon.body()["firing"],
            [
                {
                    "kind": "assistant.permission",
                    "key": "",
                    "facts": {"thread": "42"},
                    "slots": {"command": "rm -r old-logs", "tool": "shell"},
                    "context": "assistant:thread:42",
                    "category": "PERMISSION",
                }
            ],
        )
        beacon_client.send("assistant.permission", category="PERMISSION")
        self.assertEqual(self.beacon.body()["category"], "PERMISSION")

    def test_a_snapshot_lists_its_entries(self) -> None:
        self.beacon.answers.append((200, {"ok": True, "opened": [4183], "observed": True}))
        sent = backup_kinds.snapshot(
            [backup_kinds.backup_space_entry(used_percent=96, labels={"service": "backup"})],
            kinds=["backup.space"],
            stale_after="30m",
        )
        self.assertEqual(sent, backup_kinds.Sent(ok=True, status=200, outcome=None, item=None, problem=None))
        request = self.beacon.requests[0]
        self.assertEqual(request.path, "/v1/snapshots")
        body = self.beacon.body()
        self.assertIsInstance(body.pop("sent_at"), int)
        self.assertEqual(
            body,
            {
                "source": "backup",
                "observed": True,
                "firing": [
                    {"kind": "backup.space", "key": "", "facts": {"used_percent": 96}, "labels": {"service": "backup"}}
                ],
                "kinds": ["backup.space"],
                "stale_after": "30m",
            },
        )

    def test_unobserved_sends_the_reason_and_no_entries(self) -> None:
        backup_kinds.unobserved("The bucket's status page didn't load.")
        body = self.beacon.body()
        self.assertEqual(body["observed"], False)
        self.assertEqual(body["reason"], "The bucket's status page didn't load.")
        self.assertNotIn("firing", body)

    def test_5xx_is_retried_with_the_same_key_after_1_and_3_seconds(self) -> None:
        self.beacon.answers[:] = [(503, {}), (503, {})]
        sent = backup_kinds.backup_space(used_percent=96)
        self.assertTrue(sent.ok)
        self.assertEqual(len(self.beacon.requests), 3)
        keys = {r.headers["idempotency-key"] for r in self.beacon.requests}
        self.assertEqual(len(keys), 1)
        self.assertEqual(self.sleeps, [1.0, 3.0])

    def test_three_attempts_at_most(self) -> None:
        self.beacon.answers[:] = [(429, {}), (408, {}), (500, {}), (200, {})]
        sent = backup_kinds.backup_space(used_percent=96)
        self.assertFalse(sent.ok)
        self.assertEqual(sent.status, 500)
        self.assertEqual(len(self.beacon.requests), 3)

    def test_a_refusal_is_final_and_carries_the_problem(self) -> None:
        problem = {"at": "event kind", "message": "`backup.faild` is not a declared kind", "fix": "did you mean `backup.failed`?"}
        self.beacon.answers.append((422, {"error": problem, "problems": [problem]}))
        sent = beacon_client.send("backup.faild")
        self.assertEqual(len(self.beacon.requests), 1)
        self.assertEqual(sent.status, 422)
        self.assertFalse(sent.ok)
        self.assertEqual(
            sent.problem, "event kind: `backup.faild` is not a declared kind\n  fix: did you mean `backup.failed`?"
        )

    def test_no_answer_is_status_0_after_three_attempts(self) -> None:
        self.beacon.close()
        sent = backup_kinds.backup_space(used_percent=96)
        self.assertFalse(sent.ok)
        self.assertEqual(sent.status, 0)
        self.assertIn("no answer from Beacon", sent.problem or "")
        self.assertEqual(self.sleeps, [1.0, 3.0])

    def test_strict_mode_raises(self) -> None:
        self.beacon.answers.append((422, {"error": {"at": "event", "message": "no", "fix": ""}}))
        os.environ["BEACON_STRICT"] = "1"
        with self.assertRaises(backup_kinds.BeaconError) as raised:
            backup_kinds.backup_space(used_percent=96)
        self.assertEqual(str(raised.exception), "event: no")

    def test_configure_overrides_the_environment(self) -> None:
        # Each generated file has its own settings; only this test configures ship_kinds.
        ship_kinds.configure(url=self.beacon.url, strict=True)
        os.environ["BEACON_URL"] = "http://127.0.0.1:9"
        self.beacon.answers.append((400, {}))
        with self.assertRaises(ship_kinds.BeaconError):
            ship_kinds.ship_deployed(app="web", summary="Two fixes")
        ship_kinds.configure(disabled=True)
        self.assertEqual(ship_kinds.ship_deployed(app="web", summary="Two fixes").status, 0)
        self.assertEqual(len(self.beacon.requests), 1)

    def test_disabled_sends_nothing(self) -> None:
        os.environ["BEACON_DISABLED"] = "1"
        sent = backup_kinds.backup_space(used_percent=96)
        self.assertEqual(sent, backup_kinds.Sent(ok=True, status=0, outcome=None, item=None, problem=None))
        self.assertEqual(self.beacon.requests, [])

    def test_the_default_address(self) -> None:
        self.assertEqual(beacon_client.DEFAULT_URL, "http://127.0.0.1:8210")
        self.assertEqual(backup_kinds.DEFAULT_URL, beacon_client.DEFAULT_URL)

    def test_the_standalone_client_sends_any_kind(self) -> None:
        beacon_client.send("ship.deployed", facts={"app": "web"}, slots={"summary": "Two fixes"}, resolve=False)
        self.assertEqual(
            self.beacon.body(),
            {"kind": "ship.deployed", "key": "", "facts": {"app": "web"}, "slots": {"summary": "Two fixes"}},
        )
        beacon_client.snapshot("ship", [beacon_client.entry("ship.failed", facts={"app": "web"}, slots={"cause": "x"})])
        self.assertEqual(self.beacon.body()["source"], "ship")


if __name__ == "__main__":
    unittest.main()
