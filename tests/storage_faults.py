#!/usr/bin/env python3
"""External storage fault tests; Linux/Python stdlib and a C compiler (~35 seconds).

Build first, then: python3 tests/storage_faults.py --binary target/debug/oyasys
All application assertions use its real HTTP interface. SQLite is used only to
construct and inspect a foreign future-schema fixture, never application tables.
RLIMIT_FSIZE is applied to the child process only; no filesystem fills or mounts.
"""

import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import resource
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request


NOW = 2_000_000_000_000
BINARY = Path("target/debug/oyasys").resolve()


SLOW_CHECKPOINT = r"""
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* Delay database header writes, without changing SQLite's result. WAL appends
   remain unaffected, exposing publication races during the following checkpoint. */
ssize_t pwrite64(int fd, const void *buffer, size_t count, off64_t offset) {
    ssize_t (*real_write)(int, const void *, size_t, off64_t) = dlsym(RTLD_NEXT, "pwrite64");
    const char *armed = getenv("OYASYS_TEST_SLOW_CHECKPOINT");
    if (offset == 0 && armed && access(armed, F_OK) == 0) {
        char descriptor[64], path[4096];
        snprintf(descriptor, sizeof(descriptor), "/proc/self/fd/%d", fd);
        ssize_t length = readlink(descriptor, path, sizeof(path) - 1);
        if (length >= 0) {
            path[length] = '\0';
            const char *suffix = "/history.sqlite3";
            size_t size = strlen(suffix);
            if ((size_t)length >= size && !strcmp(path + length - size, suffix)) {
                FILE *marker = fopen(armed, "w");
                if (marker) { fputs("delayed", marker); fclose(marker); }
                usleep(300000);
            }
        }
    }
    return real_write(fd, buffer, count, offset);
}
"""


def ignore_file_limit_signal():
    # An ignored disposition survives exec: SQLite receives EFBIG instead of
    # SIGXFSZ terminating the server, so its actual error handling is exercised.
    signal.signal(signal.SIGXFSZ, signal.SIG_IGN)


class Server:
    def __init__(self, directory, wide=False, environment=None):
        self.directory = Path(directory)
        self.clock = self.directory / "clock"
        self.fixture = self.directory / "fixture.json"
        self.data = self.directory / "data"
        self.child = None
        self.stderr = None
        self.environment = environment
        if not self.fixture.exists():
            metrics = [
                {"id": "gpu.fixture.temperature", "label": "Synthetic P100 temperature",
                 "unit": "°C", "group": "gpu", "source": "Synthetic P100"},
                {"id": "fan.fixture", "label": "Synthetic fan", "unit": "RPM",
                 "group": "fan", "source": "Synthetic fan"},
            ]
            if wide:
                metrics.extend({"id": f"fixture.{i}", "label": f"Synthetic channel {i}",
                                "unit": "%", "group": "cpu", "source": "Synthetic"}
                               for i in range(1022))
            values = {metric["id"]: 41.5 for metric in metrics}
            values["fan.fixture"] = 0
            self.fixture.write_text(json.dumps({
                "inventory": {"hostname": "storage-fault-fixture", "system": [],
                              "metrics": metrics, "warnings": [], "synthetic": True},
                "samples": [{"timestamp_ms": NOW - 20_000, "values": values},
                            {"timestamp_ms": NOW, "values": values}],
                "now_ms": NOW,
            }))
            self.advance(NOW)

    def advance(self, timestamp):
        # Atomic rename avoids a transient empty clock file during a write.
        temporary = self.clock.with_suffix(".next")
        temporary.write_text(str(timestamp))
        temporary.replace(self.clock)

    def start(self):
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        self.origin = f"http://127.0.0.1:{port}"
        self.stderr = tempfile.TemporaryFile()
        self.child = subprocess.Popen(
            [str(BINARY), "serve", "--bind", f"127.0.0.1:{port}",
             "--origin", self.origin, "--data-dir", str(self.data),
             "--fixture", str(self.fixture), "--fixture-clock", str(self.clock)],
            stdout=subprocess.DEVNULL, stderr=self.stderr,
            preexec_fn=ignore_file_limit_signal,
            env=self.environment,
        )
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.child.poll() is not None:
                self.stderr.seek(0)
                raise AssertionError(f"Server exited: {self.stderr.read().decode()}")
            try:
                return self.state()
            except (OSError, urllib.error.URLError):
                time.sleep(0.02)
        raise AssertionError("Server did not become available within ten seconds")

    def stop(self, abruptly=False):
        if self.child is not None:
            if self.child.poll() is None:
                (self.child.kill if abruptly else self.child.terminate)()
                self.child.wait(timeout=10)
            self.child = None
        if self.stderr is not None:
            self.stderr.close()
            self.stderr = None

    def request(self, path, payload=None):
        headers = {"Connection": "close"}
        if payload is not None:
            headers.update({"Content-Type": "application/json", "Origin": self.origin,
                            "X-OYAsys-CSRF": self.state()["csrf_token"]})
        request = urllib.request.Request(
            self.origin + "/api/v1" + path,
            data=None if payload is None else json.dumps(payload).encode(),
            method="GET" if payload is None else "PUT", headers=headers,
        )
        try:
            response = urllib.request.urlopen(request, timeout=5)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, json.load(response)

    def state(self):
        status, body = self.request("/state")
        if status != 200:
            raise AssertionError((status, body))
        return body

    def settings(self, **changes):
        current = self.state()["settings"]
        return self.request("/settings", current | changes | {"confirm_delete": True})

    def history(self, until):
        query = urllib.parse.urlencode({"from": NOW - 30_000, "to": until,
                                       "metrics": "gpu.fixture.temperature,fan.fixture",
                                       "points": 2000})
        return self.request("/history?" + query)

    def wait_snapshot(self, timestamp, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            state = self.state()
            if state["snapshot"] and state["snapshot"]["timestamp_ms"] == timestamp:
                return state
            time.sleep(0.02)
        raise AssertionError(f"No collection at {timestamp}: {state['snapshot']}")

    def rss_bytes(self):
        for line in Path(f"/proc/{self.child.pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
        raise AssertionError("Linux did not expose process RSS")


class StorageFaults(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="oyasys-storage-faults-")
        self.server = None

    def tearDown(self):
        if self.server:
            self.server.stop()
        self.temporary.cleanup()

    def test_interrupted_restart_preserves_committed_history_and_settings(self):
        self.server = server = Server(self.temporary.name)
        initial = server.start()
        self.assertTrue(initial["snapshot"]["persisted"])
        status, saved = server.settings(sampling_seconds=1, retention_days=14,
                                        p100_fan_id="fan.fixture")
        self.assertEqual(status, 200, saved)
        server.advance(NOW + 1000)
        self.assertTrue(server.wait_snapshot(NOW + 1000)["snapshot"]["persisted"])
        status, before = server.history(NOW + 1000)
        self.assertEqual(status, 200, before)
        server.stop(abruptly=True)  # SIGKILL: no application checkpoint/shutdown.
        restarted = server.start()
        self.assertEqual(restarted["settings"], saved)
        self.assertTrue(restarted["snapshot"]["persisted"])
        self.assertEqual(server.history(NOW + 1000), (200, before))

    def test_future_schema_is_rejected_without_rewriting_its_data(self):
        self.server = server = Server(self.temporary.name)
        server.data.mkdir()
        database = server.data / "history.sqlite3"
        sentinel = os.urandom(257)
        with closing(sqlite3.connect(database)) as connection, connection:
            connection.execute("PRAGMA user_version=999999")
            connection.execute("CREATE TABLE future_payload(value BLOB NOT NULL)")
            connection.execute("INSERT INTO future_payload VALUES(?)", (sentinel,))
        state = server.start()
        self.assertFalse(state["storage"]["available"])
        self.assertIn("Unsupported history schema", state["storage"]["error"])
        self.assertFalse(state["snapshot"]["persisted"])
        self.assertEqual(state["snapshot"]["values"]["gpu.fixture.temperature"], 41.5)
        self.assertEqual(server.history(NOW)[0], 503)
        self.assertEqual(server.settings(sampling_seconds=1)[0], 503)
        self.assertEqual(server.state()["settings"], state["settings"])
        server.stop()
        # This is a foreign database owned by the test, not inspection of an
        # OYAsys schema or private storage implementation.
        with closing(sqlite3.connect(database)) as connection:
            self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 999999)
            self.assertEqual(connection.execute("SELECT value FROM future_payload").fetchone()[0], sentinel)
            self.assertEqual(connection.execute("PRAGMA integrity_check").fetchone()[0], "ok")
            self.assertEqual(connection.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall(),
                             [("future_payload",)])

    @unittest.skipUnless(hasattr(resource, "prlimit"), "Linux prlimit required")
    def test_real_write_failure_keeps_live_data_rolls_back_and_recovers(self):
        temporary = Path(self.temporary.name)
        source = temporary / "slow-checkpoint.c"
        library = temporary / "slow-checkpoint.so"
        armed = temporary / "slow-checkpoint.armed"
        source.write_text(SLOW_CHECKPOINT)
        subprocess.run(["cc", "-shared", "-fPIC", "-O2", str(source), "-ldl", "-o", str(library)],
                       check=True)
        environment = os.environ | {"LD_PRELOAD": str(library),
                                    "OYASYS_TEST_SLOW_CHECKPOINT": str(armed)}
        self.server = server = Server(self.temporary.name, wide=True, environment=environment)
        server.start()
        status, saved = server.settings(sampling_seconds=1, retention_days=14)
        self.assertEqual(status, 200, saved)
        baseline_status, baseline = server.history(NOW)
        self.assertEqual(baseline_status, 200, baseline)
        old_limit = resource.prlimit(server.child.pid, resource.RLIMIT_FSIZE)
        resource.prlimit(server.child.pid, resource.RLIMIT_FSIZE, (0, old_limit[1]))
        try:
            status, error = server.settings(sampling_seconds=7, retention_days=21)
            self.assertEqual(status, 503, error)
            self.assertEqual(server.state()["settings"], saved)
            failed_times = []
            rss = []
            # 1024-metric collections make retaining failed snapshots expensive;
            # repeated updates exercise failure without any unwritten backlog.
            for index in range(1, 193):
                timestamp = NOW + index * 1000
                server.advance(timestamp)
                state = server.wait_snapshot(timestamp)
                self.assertFalse(state["snapshot"]["persisted"])
                self.assertFalse(state["storage"]["available"])
                self.assertEqual(state["snapshot"]["values"]["gpu.fixture.temperature"], 41.5)
                self.assertEqual(state["snapshot"]["values"]["fan.fixture"], 0)
                failed_times.append(timestamp)
                if index % 16 == 0:
                    rss.append(server.rss_bytes())
            # A regression guard over this failure run, not a product RAM target
            # or proof of a mathematical bound for every possible workload.
            self.assertLess(max(rss) - min(rss), 8 * 1024 * 1024, rss)
            self.assertEqual(server.state()["settings"], saved)
        finally:
            resource.prlimit(server.child.pid, resource.RLIMIT_FSIZE, old_limit)

        # Make slow checkpoint I/O deterministic on fast developer machines too.
        # Only this child is affected; all storage operations remain real.
        armed.touch()
        # Real backoff uses monotonic time. Keep collecting while the collector
        # retries; require recovery without restarting or fabricating old rows.
        deadline = time.monotonic() + 38
        timestamp = failed_times[-1]
        while time.monotonic() < deadline:
            timestamp += 1000
            server.advance(timestamp)
            recovered = server.wait_snapshot(timestamp)
            if recovered["snapshot"]["persisted"]:
                break
            failed_times.append(timestamp)
        else:
            self.fail("Storage did not recover within its 30-second retry window")
        self.assertTrue(recovered["storage"]["available"])
        self.assertEqual(armed.read_text(), "delayed", "Recovery did not exercise the delayed checkpoint")
        armed.unlink()
        status, history = server.history(timestamp)
        self.assertEqual(status, 200, history)
        points = history["series"][0]["points"]
        persisted_times = {point[0] for point in points if point[1] is not None}
        self.assertIn(timestamp, persisted_times)
        self.assertTrue(set(failed_times).isdisjoint(persisted_times))
        self.assertTrue({NOW - 20_000, NOW}.issubset(persisted_times))
        self.assertEqual(server.state()["settings"], saved)
        status, saved_after = server.settings(sampling_seconds=3)
        self.assertEqual(status, 200, saved_after)
        server.stop(abruptly=True)
        self.assertEqual(server.start()["settings"], saved_after)
        print(f"  192 failed wide collections; RSS spread {max(rss) - min(rss):,} bytes; "
              "live recovery and honest history gap verified", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/oyasys")
    arguments = parser.parse_args()
    BINARY = Path(arguments.binary).resolve()
    if not BINARY.is_file():
        parser.error(f"Build the executable first: {BINARY}")
    unittest.main(argv=[__file__], verbosity=2)
