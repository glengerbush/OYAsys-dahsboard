#!/usr/bin/env python3
"""Hermetic updater boundary tests: disposable paths and external-service adapters."""
import importlib.util
from contextlib import closing
import hashlib
import io
import json
import os
import fcntl
import sqlite3
import subprocess
import urllib.error
from email.message import Message
from pathlib import Path
import sys
import tempfile
import tarfile
import unittest
from unittest.mock import patch

MODULE = Path(__file__).resolve().parents[1] / "deploy" / "update.py"

class UpdaterBoundary(unittest.TestCase):
    def setup_updater(self):
        spec = importlib.util.spec_from_file_location("oyasys_updater", MODULE)
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module; spec.loader.exec_module(module)
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        paths = module.Paths(root / "app", root / "updater", root / "data", root / "etc", os.getuid(), os.getuid(), os.getgid())
        for path in [paths.app, paths.state, paths.data, paths.config, paths.inbox]: path.mkdir(exist_ok=True)
        old = b"\x7fELF\x02\x01\x01" + bytes(11) + b"\x3e\x00" + bytes(44) + b"0.1.0"
        new = old[:-5] + b"0.2.0"
        paths.binary.write_bytes(old)
        host = FakeSystem(paths)
        github = FakeGithub(new)
        updater = module.Updater(paths, host, github)
        return paths, host, github, updater, old, new

    def request(self, paths, updater, action, **fields):
        (paths.inbox / "request.json").write_text(json.dumps({"id": "test-request", "action": action} | fields))
        updater.run()
        return json.loads(paths.status.read_text())

    def test_checksum_failure_leaves_running_service_untouched(self):
        paths, host, github, updater, old, new = self.setup_updater()
        self.assertEqual(self.request(paths, updater, "check")["state"], "available")
        github.corrupt = True
        status = self.request(paths, updater, "install", version="0.2.0")
        self.assertEqual(status["state"], "error")
        self.assertIn("checksum", status["message"].lower())
        self.assertEqual(paths.binary.read_bytes(), old)
        self.assertEqual(host.stops, 0)

    def test_failed_start_rolls_back_binary_database_and_preserves_settings(self):
        paths, host, github, updater, old, new = self.setup_updater()
        database = paths.data / "history.sqlite3"
        database.write_bytes(b"committed settings and readings")
        host.fail_new = True
        self.request(paths, updater, "check")
        status = self.request(paths, updater, "install", version="0.2.0")
        self.assertEqual(status["state"], "rolled_back", status)
        self.assertEqual(paths.binary.read_bytes(), old)
        self.assertEqual(database.read_bytes(), b"committed settings and readings")
        self.assertFalse(paths.journal.exists())
        self.assertFalse((paths.backup / "database").exists(), "successful rollback must not retain expired data")

    def test_hostile_request_cannot_choose_files_or_commands(self):
        spec = importlib.util.spec_from_file_location("oyasys_updater", MODULE)
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = module.Paths(root / "app", root / "updater", root / "data", root / "etc", os.getuid(), os.getuid(), os.getgid())
            for path in [paths.app, paths.state, paths.data, paths.config, paths.inbox]:
                path.mkdir(exist_ok=True)
            (paths.inbox / "request.json").write_text(json.dumps({"id":"safe-id","action":"install","version":"../../etc/passwd"}))
            module.Updater(paths, object(), object()).run()
            status = json.loads(paths.status.read_text())
            self.assertEqual(status["state"], "error")
            self.assertFalse((paths.inbox / "request.json").exists())
            self.assertFalse(paths.binary.exists())

    def test_success_deletes_transaction_data_backup(self):
        paths, host, github, updater, old, new = self.setup_updater()
        (paths.data / "history.sqlite3").write_bytes(b"retained settings/history")
        self.request(paths, updater, "check")
        status = self.request(paths, updater, "install", version="0.2.0")
        self.assertEqual(status["state"], "updated", status)
        self.assertEqual(paths.binary.read_bytes(), new)
        self.assertEqual((paths.data / "history.sqlite3").read_bytes(), b"retained settings/history")
        self.assertFalse(paths.journal.exists())
        self.assertFalse((paths.backup / "database").exists())
        self.assertEqual((paths.backup / "binary").read_bytes(), old)

    def test_new_request_queued_during_finalization_survives(self):
        paths, host, github, updater, old, new = self.setup_updater()
        self.request(paths, updater, "check")
        def enqueue():
            (paths.inbox / "request.json").write_text(json.dumps({"id": "next-check", "action": "check"}))
        host.cleanup = enqueue  # External cleanup boundary: new HTTP request can arrive here.
        self.assertEqual(self.request(paths, updater, "install", version="0.2.0")["state"], "updated")
        self.assertTrue((paths.inbox / "request.json").exists())
        updater.run()
        self.assertEqual(json.loads(paths.status.read_text())["request_id"], "next-check")
        self.assertFalse((paths.inbox / "request.json").exists())

    def test_request_fifo_symlink_and_oversized_are_rejected_without_following(self):
        paths, host, github, updater, old, new = self.setup_updater()
        request = paths.inbox / "request.json"
        outside = paths.data / "outside"; outside.write_text("untouched")
        creators = [lambda: request.symlink_to(outside), lambda: os.mkfifo(request),
                    lambda: request.write_text("x" * 4097)]
        for create in creators:
            with self.subTest(create=create):
                create(); updater.run()
                self.assertEqual(json.loads(paths.status.read_text())["state"], "error")
                self.assertFalse(os.path.lexists(request))
                self.assertEqual(outside.read_text(), "untouched")
        self.assertEqual(host.stops, 0)

    def test_hardlinked_atomic_request_is_accepted(self):
        paths, host, github, updater, old, new = self.setup_updater()
        temporary = paths.inbox / "published-temp"
        temporary.write_text(json.dumps({"id": "two-links", "action": "check"}))
        os.link(temporary, paths.inbox / "request.json")
        updater.run()
        self.assertEqual(json.loads(paths.status.read_text())["state"], "available")
        self.assertTrue(temporary.exists())

    def test_orphan_stages_and_partial_manifest_removed_before_check(self):
        paths, host, github, updater, old, new = self.setup_updater()
        stage = paths.state / "stage-interrupted"; stage.mkdir()
        (stage / "archive").write_bytes(b"partial archive")
        (paths.state / "manifest.download").write_bytes(b"partial JSON")
        self.assertEqual(self.request(paths, updater, "check")["state"], "available")
        self.assertFalse(stage.exists())
        self.assertFalse((paths.state / "manifest.download").exists())

    def test_unsafe_or_duplicate_archive_never_stops_service(self):
        paths, host, github, updater, old, new = self.setup_updater()
        for kind in ("symlink", "traversal", "duplicate", "invalid-elf", "too-many"):
            with self.subTest(kind=kind):
                data = io.BytesIO()
                with tarfile.open(fileobj=data, mode="w:gz") as tar:
                    entry = tarfile.TarInfo("oyasys/oyasys"); entry.size = len(new)
                    if kind == "symlink": entry.type = tarfile.SYMTYPE; entry.linkname = "/etc/shadow"
                    if kind == "traversal": entry.name = "oyasys/../oyasys"
                    binary = b"X" * len(new) if kind == "invalid-elf" else new
                    tar.addfile(entry, io.BytesIO(binary))
                    if kind == "duplicate": tar.addfile(entry, io.BytesIO(new))
                    if kind == "too-many":
                        for n in range(1025): tar.addfile(tarfile.TarInfo("oyasys/extra" + str(n)))
                github.archive = data.getvalue()
                github.manifest["sha256"] = hashlib.sha256(github.archive).hexdigest()
                self.request(paths, updater, "check")
                self.assertEqual(self.request(paths, updater, "install", version="0.2.0")["state"], "error")
                self.assertEqual(paths.binary.read_bytes(), old)
                self.assertEqual(host.stops, 0)

    def test_incompatible_manifest_and_changed_release_rejected(self):
        paths, host, github, updater, old, new = self.setup_updater()
        github.manifest["database_schema"] = 2
        self.assertEqual(self.request(paths, updater, "check")["state"], "error")
        github.manifest["database_schema"] = 1
        self.request(paths, updater, "check")
        github.manifest["sha256"] = "0" * 64
        self.assertIn("changed", self.request(paths, updater, "install", version="0.2.0")["message"])
        self.assertEqual(host.stops, 0)

    def test_interrupted_install_restores_database_and_consumes_new_request_only_for_recovery(self):
        paths, host, github, updater, old, new = self.setup_updater()
        database = paths.data / "history.sqlite3"; database.write_bytes(b"committed history")
        self.request(paths, updater, "check")
        host.crash_new = True
        with self.assertRaises(SimulatedPowerLoss):
            self.request(paths, updater, "install", version="0.2.0")
        self.assertEqual(json.loads(paths.journal.read_text())["phase"], "installed")
        self.assertEqual(paths.binary.read_bytes(), new)
        host.crash_new = False
        status = self.request(paths, updater, "check")
        self.assertEqual(status["state"], "rolled_back", status)
        self.assertEqual(paths.binary.read_bytes(), old)
        self.assertEqual(database.read_bytes(), b"committed history")
        self.assertFalse(paths.journal.exists())

    def test_committed_finalization_recovers_without_deleted_database_backup(self):
        for phase, expected in (("healthy", "updated"), ("rolled_back", "rolled_back")):
            with self.subTest(phase=phase):
                paths, host, github, updater, old, new = self.setup_updater()
                paths.binary.write_bytes(new if phase == "healthy" else old)
                (paths.data / "history.sqlite3").write_bytes(b"already committed")
                paths.journal.write_text(json.dumps({"protocol": 1, "request_id": "interrupted",
                    "old_version": "0.1.0", "new_version": "0.2.0", "phase": phase, "database_exists": True}))
                updater.run()
                self.assertEqual(json.loads(paths.status.read_text())["state"], expected)
                self.assertFalse(paths.journal.exists())
                self.assertEqual((paths.data / "history.sqlite3").read_bytes(), b"already committed")

    def test_recovery_preserves_unrelated_manual_install(self):
        paths, host, github, updater, old, new = self.setup_updater()
        self.request(paths, updater, "check"); host.crash_new = True
        with self.assertRaises(SimulatedPowerLoss): self.request(paths, updater, "install", version="0.2.0")
        manual = new[:-5] + b"0.3.0"; paths.binary.write_bytes(manual)
        host.crash_new = False
        updater.run()
        status = json.loads(paths.status.read_text())
        self.assertEqual(status["state"], "error")
        self.assertIn("manual", status["message"])
        self.assertEqual(paths.binary.read_bytes(), manual)
        self.assertTrue(paths.journal.exists())

    @unittest.skipIf(os.geteuid() == 0, "Real database helper deliberately refuses root")
    def test_real_sqlite_backup_restore_and_symlinks_run_as_unprivileged_process(self):
        paths, host, github, updater, old, new = self.setup_updater()
        module = sys.modules["oyasys_updater"]; system = module.System(paths)
        paths.backup.mkdir()
        database = paths.data / "history.sqlite3"
        with closing(sqlite3.connect(database)) as db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("CREATE TABLE settings(value TEXT)")
            db.execute("INSERT INTO settings VALUES ('committed')")
            db.commit()
            self.assertTrue(system.backup_database(paths.backup / "database"))
            db.execute("UPDATE settings SET value='new' ")
            db.commit()
        outside = paths.data / "outside"; outside.write_bytes(b"untouched")
        database.unlink(); database.symlink_to(outside)
        for suffix in ("-wal", "-shm"):
            sidecar = Path(str(database) + suffix)
            sidecar.unlink(missing_ok=True); sidecar.symlink_to(outside)
        system.restore_database(paths.backup / "database")
        self.assertEqual(outside.read_bytes(), b"untouched")
        with closing(sqlite3.connect(database)) as db:
            self.assertEqual(db.execute("SELECT value FROM settings").fetchone()[0], "committed")
        database.unlink(); database.symlink_to(outside)
        with self.assertRaises(module.UpdateError): system.backup_database(paths.backup / "bad")
        system.cleanup()
        self.assertEqual(outside.read_bytes(), b"untouched")

    def test_existing_data_lock_prevents_backup_or_replacement(self):
        paths, host, github, updater, old, new = self.setup_updater()
        module = sys.modules["oyasys_updater"]; system = module.System(paths)
        system.control = lambda action: None  # No host systemd interaction.
        lock = paths.data / "oyasys.lock"; lock.touch()
        with lock.open("rb") as active:
            fcntl.flock(active, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(module.UpdateError, "Another dashboard"):
                system.stop()
            self.assertIsNone(system.data_lock)
        system.stop()
        self.assertIsNotNone(system.data_lock)
        system.start()
        self.assertIsNone(system.data_lock)

    def test_health_address_uses_direct_bind_and_configured_host(self):
        paths, host, github, updater, old, new = self.setup_updater()
        system = sys.modules["oyasys_updater"].System(paths)
        (paths.config / "environment").write_text('OYASYS_BIND=0.0.0.0:9850\nOYASYS_ORIGIN=https://oyasys.example:8443\n')
        self.assertEqual(system.address(), ("127.0.0.1", 9850, "oyasys.example:8443"))
        (paths.config / "environment").write_text('OYASYS_BIND=[::]:9850\nOYASYS_ORIGIN=https://oyasys.example\n')
        self.assertEqual(system.address(), ("::1", 9850, "oyasys.example"))

    def test_idle_boot_ignores_credentials_and_does_not_call_network(self):
        paths, host, github, updater, old, new = self.setup_updater()
        (paths.config / "update-token").write_text("invalid credential value")
        module = sys.modules["oyasys_updater"]
        module.Updater(paths, host, module.Github(paths)).run()
        self.assertFalse(paths.status.exists())

    def test_https_redirect_drops_token_and_bounds_response(self):
        paths, host, github, updater, old, new = self.setup_updater()
        module = sys.modules["oyasys_updater"]
        token = paths.config / "update-token"; token.write_text("github_test_token"); token.chmod(0o600)
        network = module.Github(paths)
        class Response(io.BytesIO):
            headers = {}
        class Opener:
            requests = []
            def open(self, request, timeout):
                self.requests.append(request)
                if len(self.requests) == 1:
                    headers = Message(); headers["Location"] = "https://release-assets.githubusercontent.com/file"
                    raise urllib.error.HTTPError(request.full_url, 302, "redirect", headers, None)
                return Response(b"0123456789")
        adapter = Opener(); network.opener = adapter
        with self.assertRaisesRegex(module.UpdateError, "size/time"):
            network.transfer(module.API + "/releases/assets/1", io.BytesIO(), 9, "application/octet-stream")
        self.assertEqual(adapter.requests[0].get_header("Authorization"), "Bearer github_test_token")
        self.assertIsNone(adapter.requests[1].get_header("Authorization"))


    def test_unapproved_redirect_rejected_before_second_network_call(self):
        paths, host, github, updater, old, new = self.setup_updater()
        module = sys.modules["oyasys_updater"]; network = module.Github(paths)
        class Opener:
            calls = 0
            def open(self, request, timeout):
                self.calls += 1
                headers = Message(); headers["Location"] = "https://attacker.example/file"
                raise urllib.error.HTTPError(request.full_url, 302, "redirect", headers, None)
        adapter = Opener(); network.opener = adapter
        with self.assertRaisesRegex(module.UpdateError, "unsupported HTTPS host"):
            network.transfer(module.API + "/releases/latest", io.BytesIO(), 1024, "application/json")
        self.assertEqual(adapter.calls, 1)

    def test_slow_chunk_crossing_deadline_is_rejected_before_write(self):
        paths, host, github, updater, old, new = self.setup_updater()
        module = sys.modules["oyasys_updater"]; network = module.Github(paths)
        now = [0]
        class Response(io.BytesIO):
            headers = {}
            def read1(self, size):
                now[0] = 46
                return super().read1(size)
        class Opener:
            def open(self, request, timeout): return Response(b"late data")
        network.opener = Opener(); output = io.BytesIO()
        with patch.object(module.time, "monotonic", side_effect=lambda: now[0]):
            with self.assertRaisesRegex(module.UpdateError, "size/time"):
                network.transfer(module.API + "/releases/latest", output, 1024, "application/json")
        self.assertEqual(output.getvalue(), b"")

    def test_symlinked_privileged_directory_rejected(self):
        paths, host, github, updater, old, new = self.setup_updater()
        target = paths.data / "target"; target.mkdir()
        paths.binary.unlink(); paths.app.rmdir(); paths.app.symlink_to(target)
        with self.assertRaises(OSError): updater.run()
        self.assertEqual(list(target.iterdir()), [])


class SimulatedPowerLoss(BaseException): pass

class FakeSystem:
    def __init__(self, paths): self.paths, self.stops, self.fail_new = paths, 0, False
    def version(self, binary): return binary.read_bytes()[-5:].decode()
    def stop(self): self.stops += 1
    def start(self):
        if getattr(self, "crash_new", False) and self.version(self.paths.binary) == "0.2.0":
            (self.paths.data / "history.sqlite3").write_bytes(b"uncommitted new history")
            raise SimulatedPowerLoss()
        if self.fail_new and self.version(self.paths.binary) == "0.2.0":
            (self.paths.data / "history.sqlite3").write_bytes(b"candidate changed database")
    def health(self, expected):
        if self.fail_new and expected == "0.2.0": raise RuntimeError("candidate health failed")
        assert self.version(self.paths.binary) == expected
    def backup_database(self, target):
        database = self.paths.data / "history.sqlite3"
        if not database.exists(): return False
        target.write_bytes(database.read_bytes()); return True
    def restore_database(self, source):
        database = self.paths.data / "history.sqlite3"
        if source is None: database.unlink(missing_ok=True)
        else: database.write_bytes(source.read_bytes())
    def cleanup(self): pass

class FakeGithub:
    def __init__(self, binary):
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w:gz") as tar:
            member = tarfile.TarInfo("oyasys/oyasys"); member.size = len(binary)
            tar.addfile(member, io.BytesIO(binary))
        self.archive = archive.getvalue()
        self.manifest = {"protocol": 1, "version": "0.2.0", "archive": "oyasys-linux-x86_64.tar.gz",
                         "sha256": hashlib.sha256(self.archive).hexdigest(), "platform": "linux-x86_64", "database_schema": 1}
        self.corrupt = False
    def latest(self):
        return {"tag_name": "v0.2.0", "draft": False, "prerelease": False,
                "assets": [{"id": 1, "name": "oyasys-update.json"}, {"id": 2, "name": "oyasys-linux-x86_64.tar.gz"}]}
    def asset(self, identifier, target, maximum):
        data = json.dumps(self.manifest).encode() if identifier == 1 else self.archive + (b"corrupt" if self.corrupt else b"")
        assert len(data) <= maximum
        target.write_bytes(data)
        return hashlib.sha256(data).hexdigest()

if __name__ == "__main__":
    unittest.main(verbosity=2)
