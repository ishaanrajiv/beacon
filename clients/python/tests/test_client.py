"""The client and the generated emitters against a fake Beacon: what is sent, retries, and results.

Run from clients/python: `python3 -m unittest discover -s tests`.
"""

from __future__ import annotations

import errno
import json
import os
import stat
import sys
import tempfile
import threading
import unittest
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable, cast
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
        for name in ("BEACON_TOKEN", "BEACON_STRICT", "BEACON_DISABLED", "BEACON_SPOOL_DIR"):
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


class SpoolTest(unittest.TestCase):
    """The spool, against a fake Beacon that refuses (503, or no answer), then accepts."""

    def setUp(self) -> None:
        self.beacon = FakeBeacon()
        self.folder = tempfile.TemporaryDirectory()
        self.spool = os.path.join(self.folder.name, "spool")
        self.env = mock.patch.dict(os.environ, {"BEACON_URL": self.beacon.url, "BEACON_SPOOL_DIR": self.spool})
        self.env.start()
        for name in ("BEACON_TOKEN", "BEACON_STRICT", "BEACON_DISABLED"):
            os.environ.pop(name, None)
        self.sleep = mock.patch("time.sleep")
        self.sleep.start()
        self.clock = 1794535200000
        self.now = mock.patch.object(beacon_client._Runtime, "now_ms", self.now_ms)  # pyright: ignore[reportPrivateUsage] - the test sets the spool's clock
        self.now.start()
        beacon_client._beacon.dropped_total = 0  # pyright: ignore[reportPrivateUsage] - a fresh count per test

    def tearDown(self) -> None:
        self.now.stop()
        self.sleep.stop()
        self.env.stop()
        self.beacon.close()
        self.folder.cleanup()

    def now_ms(self) -> int:
        return self.clock

    def refuse(self, times: int = 3) -> None:
        refusal: tuple[int, object] = (503, {})
        self.beacon.answers.extend([refusal] * times)

    def files(self) -> list[dict[str, object]]:
        names = sorted(n for n in os.listdir(self.spool) if n.endswith(".json") and not n.startswith("."))
        return [cast("dict[str, object]", json.loads(Path(self.spool, n).read_text())) for n in names]

    def test_a_send_beacon_cant_take_is_queued_then_sent_first_with_its_key(self) -> None:
        self.refuse()
        sent = beacon_client.send("backup.space", facts={"used_percent": 96})
        self.assertTrue(sent.ok)
        self.assertTrue(sent.queued)
        self.assertEqual(sent.status, 503)
        [spooled] = self.files()
        self.assertEqual(spooled["path"], "/v1/events")
        self.assertEqual(spooled["queued_at"], self.clock)
        first_key = self.beacon.requests[0].headers["idempotency-key"]
        self.assertEqual(spooled["key"], first_key)
        # Beacon is back: the next send delivers the queued one first, with the key it was first sent with.
        self.beacon.requests.clear()
        sent = beacon_client.send("backup.finished", facts={"size": "2 GB", "took": 1000})
        self.assertEqual((sent.ok, sent.queued), (True, False))
        self.assertEqual([self.beacon.body(i)["kind"] for i in range(2)], ["backup.space", "backup.finished"])
        self.assertEqual(self.beacon.requests[0].headers["idempotency-key"], first_key)
        self.assertEqual(self.files(), [])

    def test_no_answer_is_queued_too_and_a_generated_file_flushes(self) -> None:
        self.beacon.close()
        sent = backup_kinds.backup_space(used_percent=96)
        self.assertTrue(sent.queued)
        self.assertIn("no answer from Beacon", sent.problem or "")
        self.assertEqual(len(self.files()), 1)
        flushed = backup_kinds.flush()
        self.assertEqual((flushed.sent, flushed.left), (0, 1))
        self.beacon = FakeBeacon()
        os.environ["BEACON_URL"] = self.beacon.url
        flushed = backup_kinds.flush()
        self.assertEqual((flushed.sent, flushed.left, flushed.dropped), (1, 0, 0))
        self.assertEqual(self.beacon.body()["kind"], "backup.space")

    def test_a_refusal_is_never_queued(self) -> None:
        self.beacon.answers.append((422, {"error": {"at": "event kind", "message": "no", "fix": ""}}))
        sent = beacon_client.send("backup.faild")
        self.assertEqual((sent.ok, sent.queued, sent.status), (False, False, 422))
        self.assertFalse(os.path.exists(self.spool) and self.files())

    def test_a_flush_keeps_order_and_stops_at_the_first_that_cant_go(self) -> None:
        for n in range(3):
            self.refuse()
            beacon_client.send("backup.space", key=f"k{n}", facts={"used_percent": 90 + n})
            self.clock += 1
        # Queued in order behind the first: the 2nd and 3rd sends tried the spool once, then waited.
        self.assertEqual([cast("dict[str, object]", f["body"])["key"] for f in self.files()], ["k0", "k1", "k2"])
        self.beacon.requests.clear()
        self.beacon.answers[:] = [(200, {}), (503, {})]
        flushed = beacon_client.flush()
        self.assertEqual((flushed.sent, flushed.left), (1, 2))
        self.assertEqual([self.beacon.body(i)["key"] for i in range(2)], ["k0", "k1"])
        flushed = beacon_client.flush()
        self.assertEqual((flushed.sent, flushed.left), (2, 0))
        self.assertEqual([self.beacon.body(i)["key"] for i in range(2, 4)], ["k1", "k2"])

    def test_a_refusal_during_a_flush_is_dropped_and_counted(self) -> None:
        for n in range(2):
            self.refuse()
            beacon_client.send("backup.space", key=f"k{n}")
            self.clock += 1
        self.beacon.answers[:] = [(422, {}), (200, {})]
        flushed = beacon_client.flush()
        self.assertEqual((flushed.sent, flushed.left, flushed.dropped), (1, 0, 1))

    def test_after_23_hours_a_request_is_dropped_and_counted(self) -> None:
        self.refuse()
        beacon_client.send("backup.space")
        # An hour inside Beacon's 24-hour idempotency window, which may have begun before this was queued.
        self.clock += 23 * 60 * 60 * 1000 + 1
        self.beacon.requests.clear()
        flushed = beacon_client.flush()
        self.assertEqual((flushed.sent, flushed.left, flushed.dropped, flushed.dropped_total), (0, 0, 1, 1))
        self.assertEqual(self.beacon.requests, [])

    def test_a_newer_snapshot_replaces_a_queued_one_and_an_old_one_is_dropped(self) -> None:
        with mock.patch("time.time", return_value=self.clock / 1000):
            self.refuse()
            beacon_client.snapshot("backup", [beacon_client.entry("backup.space", facts={"used_percent": 96})])
            self.refuse(1)
            beacon_client.snapshot("ship", [])
            self.refuse(1)
            beacon_client.snapshot("backup", [])
        self.assertEqual([cast("dict[str, object]", f["body"])["source"] for f in self.files()], ["ship", "backup"])
        self.assertEqual(cast("dict[str, object]", self.files()[1]["body"])["firing"], [])
        self.clock += 5 * 60 * 1000 + 1
        self.beacon.requests.clear()
        flushed = beacon_client.flush()
        self.assertEqual((flushed.sent, flushed.dropped), (0, 2))
        self.assertEqual(self.beacon.requests, [])

    def names(self) -> list[str]:
        return sorted(n for n in os.listdir(self.spool) if n.endswith(".json") and not n.startswith("."))

    def keys(self) -> list[object]:
        return [cast("dict[str, object]", f["body"])["key"] for f in self.files()]

    def request_text(self, key: str) -> str:
        body = {"kind": "backup.space", "key": key}
        return json.dumps({"path": "/v1/events", "key": "0" * 32, "queued_at": self.clock, "body": body})

    def another_process_queued(self, name: str, key: str) -> None:
        os.makedirs(self.spool, exist_ok=True)
        Path(self.spool, name).write_text(self.request_text(key))

    def test_a_new_file_sorts_after_any_already_there_whatever_wrote_it(self) -> None:
        # Another process's clock is ahead of this one, and it named its file the old way.
        self.another_process_queued(f"{self.clock + 5000:015d}-000003-0a1b2c3d.json", "ahead")
        self.refuse(1)
        beacon_client.send("backup.space", key="mine")
        self.assertEqual(self.keys(), ["ahead", "mine"])
        self.assertEqual(self.names()[-1], f"{self.clock + 5000:015d}-000004.json")

    def test_within_a_millisecond_the_sequence_goes_on_from_the_highest_there(self) -> None:
        self.another_process_queued(f"{self.clock:015d}-000500.json", "theirs")
        self.refuse(1)
        beacon_client.send("backup.space", key="mine")
        self.assertEqual(self.names(), [f"{self.clock:015d}-000500.json", f"{self.clock:015d}-000501.json"])
        # Past the last sequence, the next millisecond.
        self.another_process_queued(f"{self.clock:015d}-999999.json", "full")
        self.refuse(1)
        beacon_client.send("backup.space", key="next")
        self.assertEqual(self.names()[-1], f"{self.clock + 1:015d}-000000.json")
        self.assertEqual(self.keys(), ["theirs", "mine", "full", "next"])

    def test_a_name_another_process_takes_first_is_not_overwritten(self) -> None:
        taken: list[str] = []

        def first_taken(real: Callable[[str, str], None]) -> Callable[[str, str], None]:
            def publish(src: str, dst: str) -> None:
                if not taken and not os.path.basename(dst).startswith("."):
                    taken.append(dst)
                    Path(dst).write_text(self.request_text("theirs"))
                real(src, dst)

            return publish

        with mock.patch("os.link", first_taken(os.link)), mock.patch("os.replace", first_taken(os.replace)):
            self.refuse()
            sent = beacon_client.send("backup.space", key="mine")
        self.assertTrue(sent.queued, sent.problem)
        self.assertEqual(self.keys(), ["theirs", "mine"])

    @unittest.skipUnless(os.name == "posix", "Unix file modes")
    def test_the_folder_and_its_files_are_private_to_this_user(self) -> None:
        temporary: list[int] = []

        def link(src: str, dst: str) -> None:
            temporary.append(stat.S_IMODE(os.stat(src).st_mode))
            real_link(src, dst)

        real_link = os.link
        umask = os.umask(0o022)
        try:
            with mock.patch("os.link", link):
                self.refuse()
                beacon_client.send("backup.space", key="mine")
            # A folder that is already there keeps its mode.
            shared = os.path.join(self.folder.name, "shared")
            os.mkdir(shared)
            os.chmod(shared, 0o750)
            os.environ["BEACON_SPOOL_DIR"] = shared
            self.refuse()
            beacon_client.send("backup.space", key="shared")
        finally:
            os.umask(umask)
        self.assertEqual(stat.S_IMODE(os.stat(self.spool).st_mode), 0o700)
        self.assertEqual(temporary, [0o600])
        [name] = self.names()
        self.assertEqual(stat.S_IMODE(os.stat(os.path.join(self.spool, name)).st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(os.stat(shared).st_mode), 0o750)

    def test_a_snapshot_whose_replacement_cant_be_written_is_kept(self) -> None:
        real_open = open

        def full_disk(file: str, mode: str = "r", *args: Any, **kwargs: Any) -> Any:
            if any(c in mode for c in "wxa"):
                raise OSError(errno.ENOSPC, "No space left on device")
            return real_open(file, mode, *args, **kwargs)

        with mock.patch("time.time", return_value=self.clock / 1000):
            self.refuse()
            beacon_client.snapshot("backup", [beacon_client.entry("backup.space", facts={"used_percent": 96})])
            self.refuse(1)
            with mock.patch("builtins.open", full_disk):
                sent = beacon_client.snapshot("backup", [])
        self.assertEqual((sent.ok, sent.queued), (False, False))
        self.assertIn("can't be written", sent.problem or "")
        [kept] = self.files()
        self.assertEqual(len(cast("list[object]", cast("dict[str, object]", kept["body"])["firing"])), 1)

    def test_a_request_is_synced_to_disk_before_what_it_replaces_is_removed(self) -> None:
        steps: list[str] = []
        real_fsync, real_link, real_remove = os.fsync, os.link, os.remove

        def fsync(fd: int) -> None:
            steps.append("sync folder" if stat.S_ISDIR(os.fstat(fd).st_mode) else "sync file")
            real_fsync(fd)

        def link(src: str, dst: str) -> None:
            steps.append("publish")
            real_link(src, dst)

        def remove(path: str) -> None:
            steps.append("remove temporary" if os.path.basename(path).startswith(".") else "remove replaced")
            real_remove(path)

        with mock.patch("time.time", return_value=self.clock / 1000):
            self.refuse()
            beacon_client.snapshot("backup", [beacon_client.entry("backup.space", facts={"used_percent": 96})])
            self.refuse(1)
            with mock.patch("os.fsync", fsync), mock.patch("os.link", link), mock.patch("os.remove", remove):
                sent = beacon_client.snapshot("backup", [])
        self.assertTrue(sent.queued, sent.problem)
        self.assertEqual(steps, ["sync file", "publish", "remove temporary", "sync folder", "remove replaced"])

    def test_a_write_that_fails_part_way_leaves_no_temporary_file(self) -> None:
        real_open = open

        class Half:
            def __init__(self, file: Any) -> None:
                self.file = file

            def __enter__(self) -> "Half":
                return self

            def __exit__(self, *error: object) -> None:
                self.file.close()

            def write(self, data: bytes) -> int:
                self.file.write(data[: len(data) // 2])
                raise OSError(errno.ENOSPC, "No space left on device")

        def filling_disk(file: str, mode: str = "r", *args: Any, **kwargs: Any) -> Any:
            opened = real_open(file, mode, *args, **kwargs)
            return Half(opened) if "x" in mode else opened

        self.refuse()
        with mock.patch("builtins.open", filling_disk):
            sent = beacon_client.send("backup.space", key="mine")
        self.assertEqual((sent.ok, sent.queued), (False, False))
        self.assertIn("can't be written", sent.problem or "")
        self.assertEqual(os.listdir(self.spool), [])

    def test_a_temporary_file_a_stopped_process_left_is_removed_once_stale(self) -> None:
        os.makedirs(self.spool)

        def left(name: str, minutes_ago: int) -> str:
            path = os.path.join(self.spool, name)
            Path(path).write_text("half a requ")
            at = (self.clock - minutes_ago * 60 * 1000) / 1000
            os.utime(path, (at, at))
            return name

        stale = left(".0123456789abcdef.tmp", 11)
        being_written = left(".fedcba9876543210.tmp", 1)
        kinds = left(".kinds-backup.json", 600)
        beacon_client.flush()
        self.assertEqual(sorted(os.listdir(self.spool)), sorted([being_written, kinds]))
        # Queuing sweeps too: a service that only ever sends still clears them.
        stale = left(stale, 11)
        self.refuse()
        beacon_client.send("backup.space", key="mine")
        self.assertNotIn(stale, os.listdir(self.spool))
        self.assertEqual(len(self.names()), 1)

    def test_a_flush_leaves_a_new_request_that_took_the_name_of_the_one_it_sent(self) -> None:
        self.refuse()
        beacon_client.send("backup.space", key="first")
        [name] = self.names()
        real_attempt = beacon_client._Runtime.attempt  # pyright: ignore[reportPrivateUsage] - the test stands between the send and the removal

        def attempt(runtime: Any, path: str, data: bytes, key: str) -> beacon_client.Sent:
            answer = real_attempt(runtime, path, data, key)
            # Meanwhile another flush sent and removed the file, and a writer whose clock had
            # stepped back queued a new request under the same name.
            request = {"path": "/v1/events", "key": "f" * 32, "queued_at": self.clock, "body": {"kind": "backup.space", "key": "second"}}
            Path(self.spool, name).write_text(json.dumps(request))
            return answer

        with mock.patch.object(beacon_client._Runtime, "attempt", attempt):  # pyright: ignore[reportPrivateUsage] - as above
            flushed = beacon_client.flush()
        self.assertEqual((flushed.sent, flushed.dropped), (1, 0))
        self.assertEqual(self.keys(), ["second"])
        flushed = beacon_client.flush()
        self.assertEqual((flushed.sent, flushed.left), (1, 0))
        self.assertEqual([cast("dict[str, object]", r.body)["key"] for r in self.beacon.requests[-2:]], ["first", "second"])

    def test_a_replaced_snapshot_whose_name_a_new_request_took_is_not_removed(self) -> None:
        with mock.patch("time.time", return_value=self.clock / 1000):
            self.refuse()
            beacon_client.snapshot("backup", [beacon_client.entry("backup.space", facts={"used_percent": 96})])
            [name] = self.names()
            real_link = os.link

            def link(src: str, dst: str) -> None:
                real_link(src, dst)
                Path(self.spool, name).write_text(self.request_text("theirs"))

            self.refuse(1)
            with mock.patch("os.link", link):
                sent = beacon_client.snapshot("backup", [])
        self.assertTrue(sent.queued, sent.problem)
        self.assertEqual(len(self.names()), 2)
        self.assertEqual(cast("dict[str, object]", self.files()[0]["body"])["key"], "theirs")

    def test_the_spool_is_bounded_oldest_dropped_first(self) -> None:
        sent: beacon_client.Sent | None = None
        with mock.patch.object(beacon_client, "SPOOL_MAX_FILES", 2):
            for n in range(3):
                self.refuse(1 if n else 3)
                sent = beacon_client.send("backup.space", key=f"k{n}")
                self.clock += 1
        self.assertIn("dropped the 1 oldest", (sent.problem if sent else None) or "")
        self.assertEqual([cast("dict[str, object]", f["body"])["key"] for f in self.files()], ["k1", "k2"])
        self.assertEqual(beacon_client.flush().dropped_total, 1)

    def test_a_request_larger_than_the_whole_spool_is_not_queued_and_evicts_nothing(self) -> None:
        for n in range(2):
            self.refuse(1 if n else 3)
            beacon_client.send("backup.space", key=f"k{n}")
        with mock.patch.object(beacon_client, "SPOOL_MAX_BYTES", 10):
            self.refuse(1)
            sent = beacon_client.send("backup.space", key="big")
        self.assertEqual((sent.ok, sent.queued), (False, False))
        self.assertIn("larger than the spool's limit", sent.problem or "")
        self.assertEqual(self.keys(), ["k0", "k1"])
        self.assertEqual(beacon_client.flush().dropped_total, 1)

    def test_the_spool_limit_is_in_bytes(self) -> None:
        key = "\u00e9" * 1000
        request = json.dumps({"path": "/v1/events", "key": "0" * 32, "queued_at": self.clock, "body": {"kind": "backup.space", "key": key}})
        # The request fits in characters, not in bytes.
        with mock.patch.object(beacon_client, "SPOOL_MAX_BYTES", len(request.encode()) - 1):
            self.refuse()
            sent = beacon_client.send("backup.space", key=key)
        self.assertEqual((sent.ok, sent.queued), (False, False))
        self.assertFalse(os.path.exists(self.spool) and self.files())

    def test_off_without_a_folder(self) -> None:
        os.environ.pop("BEACON_SPOOL_DIR")
        self.refuse()
        sent = beacon_client.send("backup.space")
        self.assertEqual((sent.ok, sent.queued), (False, False))
        self.assertEqual(beacon_client.flush(), beacon_client.Flushed(sent=0, left=0, dropped=0, dropped_total=0, problem=None))


if __name__ == "__main__":
    unittest.main()
