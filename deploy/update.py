#!/usr/bin/env python3
"""On-demand OYAsys updater. Installed root-owned; never a persistent daemon."""
import dataclasses
import fcntl
import hashlib
import gzip
import http.client
import ipaddress
import itertools
import json
import os
from pathlib import Path
import pwd
import re
import resource
import select
import shutil
import sqlite3
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

REPOSITORY = "glengerbush/OYAsys-dahsboard"
API = "https://api.github.com/repos/" + REPOSITORY
ARCHIVE = "oyasys-linux-x86_64.tar.gz"
MANIFEST = "oyasys-update.json"
MAX_ARCHIVE = 64 * 1024 * 1024
MAX_DATABASE = 16 * 1024 * 1024 * 1024
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
SHA256 = re.compile(r"[0-9a-f]{64}\Z")


class UpdateError(Exception):
    pass


@dataclasses.dataclass
class Paths:
    app: Path = Path("/opt/oyasys")
    state: Path = Path("/var/lib/oyasys-updater")
    data: Path = Path("/var/lib/oyasys")
    config: Path = Path("/etc/oyasys")
    owner: int = 0
    uid: int = 0
    gid: int = 0

    @property
    def binary(self): return self.app / "oyasys"
    @property
    def inbox(self): return self.state / "inbox"
    @property
    def status(self): return self.state / "status.json"
    @property
    def journal(self): return self.state / "transaction.json"
    @property
    def backup(self): return self.state / "last-backup"


def version(value):
    if not isinstance(value, str) or len(value) > 48 or not VERSION.fullmatch(value):
        raise UpdateError("Expected an exact stable X.Y.Z version")
    return tuple(int(part) for part in value.split("."))


def directory(path, owner):
    """Walk without following any symlink, checking the protected final directory."""
    descriptor = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for component in Path(path).absolute().parts[1:]:
            next_descriptor = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = next_descriptor
            if owner == 0:
                ancestor = os.fstat(descriptor)
                if ancestor.st_uid != 0 or ancestor.st_mode & 0o022:
                    raise UpdateError(f"Unsafe privileged directory ancestor: {path}")
        metadata = os.fstat(descriptor)
        if metadata.st_uid != owner or metadata.st_mode & 0o022:
            raise UpdateError(f"Unsafe directory ownership/permissions: {path}")
    finally:
        os.close(descriptor)


def open_regular(path, owner, maximum, allow_links=False):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    metadata = os.fstat(descriptor)
    if (not stat.S_ISREG(metadata.st_mode) or (metadata.st_nlink != 1 and not allow_links)
            or metadata.st_uid != owner or metadata.st_size > maximum
            or (owner == 0 and metadata.st_mode & 0o022)):
        os.close(descriptor)
        raise UpdateError(f"Unsafe or oversized file: {Path(path).name}")
    return os.fdopen(descriptor, "rb")


def load_json(path, owner, maximum=65536, allow_links=False):
    with open_regular(path, owner, maximum, allow_links) as source:
        raw = source.read(maximum + 1)
    if len(raw) > maximum:
        raise UpdateError("JSON file exceeds size limit")
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise UpdateError("Duplicate JSON field")
            result[key] = value
        return result
    value = json.loads(raw, object_pairs_hook=unique)
    if not isinstance(value, dict):
        raise UpdateError("Expected a JSON object")
    return value


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try: os.fsync(descriptor)
    finally: os.close(descriptor)


def atomic_json(path, value, mode=0o600):
    descriptor, temporary = tempfile.mkstemp(prefix=".json-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w") as output:
            os.fchmod(output.fileno(), mode)
            json.dump(value, output, separators=(",", ":"), allow_nan=False)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        sync_directory(path.parent)
    finally:
        if os.path.lexists(temporary): os.unlink(temporary)


def digest(path, owner, limit):
    result = hashlib.sha256()
    with open_regular(path, owner, limit) as source:
        for block in iter(lambda: source.read(65536), b""):
            result.update(block)
    return result.hexdigest()


def copy_regular(source, target, owner, maximum, mode=0o600):
    with open_regular(source, owner, maximum) as input_file:
        descriptor, temporary = tempfile.mkstemp(prefix=".copy-", dir=target.parent)
        try:
            with os.fdopen(descriptor, "wb") as output:
                os.fchmod(output.fileno(), mode)
                total = 0
                while block := input_file.read(65536):
                    total += len(block)
                    if total > maximum: raise UpdateError("File grew beyond its limit")
                    output.write(block)
                output.flush(); os.fsync(output.fileno())
            os.replace(temporary, target)
            sync_directory(target.parent)
        finally:
            if os.path.lexists(temporary): os.unlink(temporary)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, target):
        return None


class Github:
    def __init__(self, paths):
        self.paths = paths
        self.token = None
        self.loaded_token = False
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def credentials(self):
        if self.loaded_token: return
        token_path = self.paths.config / "update-token"
        if os.path.lexists(token_path):
            with open_regular(token_path, self.paths.owner, 4096) as token:
                if os.fstat(token.fileno()).st_mode & 0o077:
                    raise UpdateError("Update token must be root-only (0600)")
                self.token = token.read().decode().strip()
            if not re.fullmatch(r"[A-Za-z0-9_]+", self.token):
                raise UpdateError("Invalid update token format")
        self.loaded_token = True

    def transfer(self, url, output, maximum, accept):
        self.credentials()
        deadline = time.monotonic() + 45
        for redirect in range(5):
            parsed = urllib.parse.urlsplit(url)
            if (parsed.scheme != "https" or parsed.username or parsed.password or parsed.port not in (None, 443)
                    or parsed.hostname not in {"api.github.com", "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"}):
                raise UpdateError("Release download redirected to an unsupported HTTPS host")
            headers = {"User-Agent": "OYAsys-updater/1", "Accept": accept, "X-GitHub-Api-Version": "2022-11-28"}
            if redirect == 0 and parsed.hostname == "api.github.com" and self.token:
                headers["Authorization"] = "Bearer " + self.token
            try:
                response = self.opener.open(urllib.request.Request(url, headers=headers), timeout=10)
            except urllib.error.HTTPError as error:
                if error.code in (301, 302, 303, 307, 308):
                    location = error.headers.get("Location")
                    error.close()
                    if not location: raise UpdateError("Release download redirect has no location")
                    url = urllib.parse.urljoin(url, location)
                    if time.monotonic() > deadline: raise UpdateError("Release download timed out")
                    continue
                raise
            result = hashlib.sha256()
            total = 0
            with response:
                if int(response.headers.get("Content-Length", "0")) > maximum:
                    raise UpdateError("Release download exceeds size limit")
                while block := response.read1(65536):
                    total += len(block)
                    if total > maximum or time.monotonic() > deadline:
                        raise UpdateError("Release download exceeded size/time limit")
                    output.write(block); result.update(block)
            return result.hexdigest()
        raise UpdateError("Too many release download redirects")

    def latest(self):
        import io
        output = io.BytesIO()
        try: self.transfer(API + "/releases/latest", output, 1024 * 1024, "application/vnd.github+json")
        except urllib.error.HTTPError as error:
            if error.code == 404:
                error.close()
                return None
            raise UpdateError(f"GitHub release check failed (HTTP {error.code})") from None
        result = json.loads(output.getvalue())
        if not isinstance(result, dict): raise UpdateError("Invalid GitHub release response")
        return result

    def asset(self, identifier, target, maximum):
        descriptor = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        try:
            with os.fdopen(descriptor, "wb") as output:
                result = self.transfer(API + f"/releases/assets/{identifier}", output, maximum, "application/octet-stream")
                output.flush(); os.fsync(output.fileno())
            return result
        except Exception:
            target.unlink(missing_ok=True)
            raise


def candidate_limits():
    resource.setrlimit(resource.RLIMIT_CPU, (5, 5))
    resource.setrlimit(resource.RLIMIT_FSIZE, (4096, 4096))
    resource.setrlimit(resource.RLIMIT_AS, (256 * 1024 * 1024, 256 * 1024 * 1024))


class System:
    """OS boundary. Candidate and every database operation run unprivileged."""
    def __init__(self, paths):
        self.paths = paths
        self.data_lock = None
        self.environment = {"PATH": "/usr/bin:/bin", "LANG": "C", "HOME": str(paths.data)}

    def identity(self):
        if self.paths.uid == 0: raise UpdateError("The oyasys service account must not be root")
        if os.geteuid() == 0:
            return {"user": self.paths.uid, "group": self.paths.gid, "extra_groups": []}
        if os.geteuid() != self.paths.uid: raise UpdateError("Cannot assume service-user identity")
        return {}  # Hermetic unprivileged qualification runs as its existing UID.

    def version(self, binary):
        with tempfile.TemporaryFile() as output:
            subprocess.run([str(binary), "--version"], stdout=output, stderr=output,
                           timeout=8, check=True, **self.identity(),
                           env=self.environment, preexec_fn=candidate_limits)
            output.seek(0)
            text = output.read(4096).decode("utf-8", errors="replace").strip()
        match = re.fullmatch(r"oyasys ([0-9]+\.[0-9]+\.[0-9]+)", text)
        if not match: raise UpdateError("Executable did not report a stable OYAsys version")
        version(match[1])
        return match[1]

    def control(self, action):
        with tempfile.TemporaryFile() as errors:
            result = subprocess.run(["/usr/bin/systemctl", action, "oyasys.service"],
                                    stdout=subprocess.DEVNULL, stderr=errors, timeout=45,
                                    env={"PATH": "/usr/bin:/bin", "LANG": "C"})
            if result.returncode:
                errors.seek(0)
                raise UpdateError(f"Service {action} failed: {errors.read(4096).decode(errors='replace')}")
    def stop(self):
        self.control("stop")
        if self.data_lock is not None: return
        directory(self.paths.data, self.paths.uid)
        descriptor = os.open(self.paths.data / "oyasys.lock", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            metadata = os.fstat(descriptor)
            if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != self.paths.uid
                    or metadata.st_nlink != 1 or metadata.st_size != 0):
                raise UpdateError("Unsafe service data lock")
            try: fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise UpdateError("Another dashboard process owns the data lock; no database files changed") from None
        except BaseException:
            os.close(descriptor)
            raise
        self.data_lock = descriptor

    def start(self):
        if self.data_lock is not None:
            os.close(self.data_lock)
            self.data_lock = None
        self.control("start")

    def address(self):
        settings = {"OYASYS_BIND": "127.0.0.1:9850", "OYASYS_ORIGIN": "http://127.0.0.1:9850"}
        with open_regular(self.paths.config / "environment", self.paths.owner, 16384) as source:
            for line in source.read().decode().splitlines():
                line = line.strip()
                if not line or line.startswith("#"): continue
                key, separator, value = line.partition("=")
                if key not in settings: continue
                if not separator: raise UpdateError("Invalid updater environment configuration")
                if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'": value = value[1:-1]
                settings[key] = value
        bind = settings["OYASYS_BIND"]
        host, port = bind.rsplit(":", 1)
        host = host.strip("[]")
        address = ipaddress.ip_address(host)
        if address.is_unspecified: host = "::1" if address.version == 6 else "127.0.0.1"
        port = int(port)
        if not 1 <= port <= 65535: raise UpdateError("Invalid configured port")
        origin = urllib.parse.urlsplit(settings["OYASYS_ORIGIN"])
        if (origin.scheme not in ("http", "https") or not origin.netloc or origin.username or origin.password
                or origin.path not in ("", "/") or origin.query or origin.fragment):
            raise UpdateError("Invalid configured origin")
        return host, port, origin.netloc

    def health(self, expected):
        host, port, authority = self.address()
        deadline = time.monotonic() + 40
        last = "No health response"
        while time.monotonic() < deadline:
            connection = http.client.HTTPConnection(host, port, timeout=2)
            try:
                connection.request("GET", "/api/v1/health", headers={"Host": authority, "Connection": "close"})
                response = connection.getresponse()
                raw = response.read(65537)
                if len(raw) > 65536: raise UpdateError("Oversized health response")
                data = json.loads(raw)
                if response.status == 200 and data.get("version") == expected and data.get("status") == "ok" and data.get("storage", {}).get("available") is True:
                    return
                last = "Expected version and healthy history storage were not reported"
            except (OSError, ValueError, http.client.HTTPException) as error:
                last = str(error)
            finally: connection.close()
            time.sleep(0.5)
        raise UpdateError("Service health verification failed: " + last)

    def helper(self, action, **options):
        return subprocess.Popen(["/usr/bin/python3", "-I", str(Path(__file__).resolve()), action, str(self.paths.data)],
                                **self.identity(), env=self.environment, **options)

    def backup_database(self, target):
        with tempfile.TemporaryFile() as errors:
            process = self.helper("--database-backup", stdout=subprocess.PIPE, stderr=errors)
            deadline, total, marker = time.monotonic() + 150, 0, None
            try:
                with target.open("xb") as output:
                    os.fchmod(output.fileno(), 0o600)
                    while True:
                        remaining = deadline - time.monotonic()
                        if remaining <= 0 or not select.select([process.stdout], [], [], remaining)[0]:
                            raise UpdateError("Consistent database backup timed out")
                        block = os.read(process.stdout.fileno(), 65536)
                        if not block: break
                        if marker is None:
                            marker, block = block[:1], block[1:]
                        total += len(block)
                        if total > MAX_DATABASE: raise UpdateError("Database exceeds 16 GiB automatic-backup limit")
                        output.write(block)
                    output.flush(); os.fsync(output.fileno())
                result = process.wait(timeout=max(1, deadline - time.monotonic()))
                if result != 0 or marker not in (b"0", b"1"):
                    errors.seek(0)
                    raise UpdateError("Database backup failed: " + errors.read(4096).decode(errors="replace"))
                if marker == b"0": target.unlink()
                sync_directory(target.parent)
                return marker == b"1"
            finally:
                if process.poll() is None: process.kill(); process.wait()
                process.stdout.close()

    def restore_database(self, source):
        with tempfile.TemporaryFile() as errors:
            input_file = open_regular(source, self.paths.owner, MAX_DATABASE) if source else subprocess.DEVNULL
            try:
                process = self.helper("--database-restore" if source else "--database-remove", stdin=input_file,
                                      stdout=subprocess.DEVNULL, stderr=errors)
                try: result = process.wait(timeout=150)
                except subprocess.TimeoutExpired:
                    process.kill(); process.wait(); raise UpdateError("Database restoration timed out") from None
                if result:
                    errors.seek(0)
                    raise UpdateError("Database restore failed: " + errors.read(4096).decode(errors="replace"))
            finally:
                if source: input_file.close()

    def cleanup(self):
        with tempfile.TemporaryFile() as errors:
            process = self.helper("--database-cleanup", stdout=subprocess.DEVNULL, stderr=errors)
            try: result = process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill(); process.wait(); raise UpdateError("Temporary database cleanup timed out") from None
            if result: raise UpdateError("Temporary database cleanup failed; inspect service-user permissions")


def database_helper(action, data):
    """Private subprocess entrypoint; refuses root and touches only service data."""
    if os.geteuid() == 0: raise UpdateError("Database helpers must never run as root")
    directory(data, os.geteuid())
    descriptor = os.open(data, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        if action == "--database-cleanup":
            for entry in itertools.islice(os.scandir(data), 4096):
                if entry.name.startswith(".oyasys-backup-"):
                    if entry.is_dir(follow_symlinks=False): shutil.rmtree(entry.path)
                    else: os.unlink(entry.name, dir_fd=descriptor)
                if entry.name.startswith(".oyasys-restore-"): os.unlink(entry.name, dir_fd=descriptor)
            return
        if action == "--database-backup":
            try: metadata = os.stat("history.sqlite3", dir_fd=descriptor, follow_symlinks=False)
            except FileNotFoundError:
                sys.stdout.buffer.write(b"0"); return
            for name in ("history.sqlite3", "history.sqlite3-wal", "history.sqlite3-shm"):
                try: metadata = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
                except FileNotFoundError: continue
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid() or metadata.st_nlink != 1:
                    raise UpdateError("Refusing non-regular, linked, or foreign-owned database files")
            with tempfile.TemporaryDirectory(prefix=".oyasys-backup-", dir=data) as temporary:
                copied = Path(temporary) / "database"
                source = sqlite3.connect("file:" + urllib.parse.quote(str(data / "history.sqlite3")) + "?mode=ro", uri=True, timeout=2)
                destination = sqlite3.connect(copied)
                try:
                    source.execute("PRAGMA cache_size=-512")
                    destination.execute("PRAGMA cache_size=-512")
                    source.backup(destination, pages=256, sleep=0.01)
                finally: source.close(); destination.close()
                if copied.stat().st_size > MAX_DATABASE: raise UpdateError("Database backup exceeds 16 GiB limit")
                sys.stdout.buffer.write(b"1")
                with copied.open("rb") as input_file: shutil.copyfileobj(input_file, sys.stdout.buffer, 65536)
            return
        if action not in ("--database-restore", "--database-remove"):
            raise UpdateError("Unknown database helper operation")
        temporary = None
        if action == "--database-restore":
            output_fd, temporary = tempfile.mkstemp(prefix=".oyasys-restore-", dir=data)
            try:
                with os.fdopen(output_fd, "wb") as output:
                    total = 0
                    while block := sys.stdin.buffer.read(65536):
                        total += len(block)
                        if total > MAX_DATABASE: raise UpdateError("Database restore exceeds size limit")
                        output.write(block)
                    output.flush(); os.fsync(output.fileno())
                # Replace names through an already-open directory; unlink never
                # follows hostile WAL/SHM or main-database symlinks.
                for name in ("history.sqlite3-wal", "history.sqlite3-shm"):
                    try: os.unlink(name, dir_fd=descriptor)
                    except FileNotFoundError: pass
                os.replace(Path(temporary).name, "history.sqlite3", src_dir_fd=descriptor, dst_dir_fd=descriptor)
            finally:
                if os.path.lexists(temporary): os.unlink(temporary)
        else:
            for name in ("history.sqlite3", "history.sqlite3-wal", "history.sqlite3-shm"):
                try: os.unlink(name, dir_fd=descriptor)
                except FileNotFoundError: pass
        os.fsync(descriptor)
    finally: os.close(descriptor)


class Updater:
    def __init__(self, paths, system, github):
        self.paths, self.system, self.github = paths, system, github

    def status(self, identifier, state, message, latest=None):
        atomic_json(self.paths.status, {"request_id": identifier, "state": state,
                    "latest_version": latest, "message": message[:1500],
                    "checked_at": int(time.time())}, 0o644)

    def run(self):
        directory(self.paths.state, self.paths.owner)
        directory(self.paths.app, self.paths.owner)
        directory(self.paths.config, self.paths.owner)
        directory(self.paths.inbox, self.paths.uid)
        descriptor = os.open(self.paths.state / "lock", os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            try: fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError: return
            self.process()
        finally: os.close(descriptor)

    def process(self):
        request_path = self.paths.inbox / "request.json"
        request = None
        identifier = "invalid-request"
        original = None
        consumed = False
        try:
            if os.path.lexists(request_path):
                initial = os.lstat(request_path)
                original = (initial.st_dev, initial.st_ino)
                request = load_json(request_path, self.paths.uid, 4096, allow_links=True)
                if set(request) - {"action", "version", "id"}:
                    raise UpdateError("Unknown request fields")
                identifier = request.get("id", "")
                if not isinstance(identifier, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", identifier):
                    identifier = "invalid-request"
                    raise UpdateError("Invalid request identifier")
                if request.get("action") not in ("check", "install"):
                    raise UpdateError("Unknown update action")
                if request["action"] == "install": version(request.get("version"))
                elif "version" in request: raise UpdateError("Check requests must not specify a version")
                self.status(identifier, "checking" if request["action"] == "check" else "installing", "Processing request")
                os.unlink(request_path)
                consumed = True
            self.clean_staging()
            if self.paths.journal.exists():
                self.recover(identifier if request else None)
                return  # Never resume or start an install after interrupted recovery.
            if request is None: return
            if request["action"] == "check": self.check(identifier)
            else: self.install(identifier, request["version"])
        except Exception as error:
            self.status(identifier, "error", str(error))
        finally:
            # A new request may arrive after terminal status. Never delete it.
            # Rejected original entries are unlinked without following links.
            if original is not None and not consumed:
                try:
                    current = os.lstat(request_path)
                    if (current.st_dev, current.st_ino) == original: os.unlink(request_path)
                except FileNotFoundError: pass

    def clean_staging(self):
        # The process lock guarantees no other helper owns these scratch paths.
        # Recovery requires only last-backup; stages never contain committed data.
        with os.scandir(self.paths.state) as entries:
            for count, entry in enumerate(entries, 1):
                if count > 4096: raise UpdateError("Updater state contains too many files; manual cleanup required")
                if not (entry.name.startswith(("stage-", ".json-", ".copy-")) or entry.name == "manifest.download"): continue
                metadata = entry.stat(follow_symlinks=False)
                if metadata.st_uid != self.paths.owner or metadata.st_mode & 0o022:
                    raise UpdateError("Unsafe orphan update staging file; manual cleanup required")
                if stat.S_ISDIR(metadata.st_mode):
                    directory(Path(entry.path), self.paths.owner)
                    shutil.rmtree(entry.path)
                else: os.unlink(entry.path)

    def check(self, identifier):
        current = self.system.version(self.paths.binary)
        release = self.release()
        if release is None:
            (self.paths.state / "checked-release.json").unlink(missing_ok=True)
            self.status(identifier, "current", "No stable release has been published yet")
            return
        atomic_json(self.paths.state / "checked-release.json", release)
        newer = version(release["version"]) > version(current)
        self.status(identifier, "available" if newer else "current",
                    f"Version {release['version']} available" if newer else f"Installed version {current} is current",
                    release["version"])

    def release(self):
        release = self.github.latest()
        if release is None: return None
        tag = release.get("tag_name", "")
        if not isinstance(tag, str) or not tag.startswith("v") or release.get("draft") is not False or release.get("prerelease") is not False:
            raise UpdateError("Only published stable releases are supported")
        version(tag[1:])
        assets = release.get("assets")
        if not isinstance(assets, list) or len(assets) > 100:
            raise UpdateError("Invalid release asset catalog")
        selected = {}
        for name in (ARCHIVE, MANIFEST):
            matches = [asset for asset in assets if isinstance(asset, dict) and asset.get("name") == name]
            if len(matches) != 1: raise UpdateError(f"Release must contain exactly one {name}")
            asset = matches[0]
            if type(asset.get("id")) is not int or asset["id"] <= 0:
                raise UpdateError("Invalid release asset ID")
            expected = asset.get("digest")
            if expected is not None and (not isinstance(expected, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", expected)):
                raise UpdateError("Unsupported GitHub asset digest")
            selected[name] = {"id": asset["id"], "digest": expected}
        manifest_path = self.paths.state / "manifest.download"
        try:
            obtained = self.github.asset(selected[MANIFEST]["id"], manifest_path, 65536)
            expected = selected[MANIFEST]["digest"]
            if expected and expected != "sha256:" + obtained:
                raise UpdateError("GitHub manifest digest mismatch")
            manifest = load_json(manifest_path, self.paths.owner)
        finally:
            manifest_path.unlink(missing_ok=True)
        if (manifest.get("protocol") != 1 or type(manifest.get("protocol")) is not int
                or manifest.get("database_schema") != 1 or type(manifest.get("database_schema")) is not int
                or manifest.get("platform") != "linux-x86_64" or manifest.get("archive") != ARCHIVE
                or manifest.get("version") != tag[1:] or not isinstance(manifest.get("sha256"), str)
                or not SHA256.fullmatch(manifest["sha256"])):
            raise UpdateError("Release manifest is incompatible or invalid; automatic installs require protocol 1/schema 1/linux-x86_64")
        return {"version": tag[1:], "manifest": manifest, "asset": selected[ARCHIVE]}

    def install(self, identifier, requested):
        checked = load_json(self.paths.state / "checked-release.json", self.paths.owner)
        if checked.get("version") != requested:
            raise UpdateError("Check for this exact version before installing")
        current = self.system.version(self.paths.binary)
        if version(requested) <= version(current):
            raise UpdateError("Automatic updates must be newer than the installed version")
        release = self.release()
        if release != checked:
            raise UpdateError("Release changed since the check; check again before installing")
        with tempfile.TemporaryDirectory(prefix="stage-", dir=self.paths.state) as temporary:
            stage = Path(temporary)
            os.chmod(stage, 0o711)  # Candidate is readable/executable as oyasys, never writable.
            archive = stage / ARCHIVE
            obtained = self.github.asset(release["asset"]["id"], archive, MAX_ARCHIVE)
            if obtained != release["manifest"]["sha256"]:
                raise UpdateError("Release archive checksum mismatch")
            api_digest = release["asset"]["digest"]
            if api_digest and api_digest != "sha256:" + obtained:
                raise UpdateError("GitHub archive checksum mismatch")
            candidate = extract_binary(archive, stage)
            if self.system.version(candidate) != requested:
                raise UpdateError("Candidate --version differs from the checked release")
            self.apply(identifier, current, requested, candidate)

    def apply(self, identifier, current, requested, candidate):
        self.paths.backup.mkdir(mode=0o700, exist_ok=True)
        directory(self.paths.backup, self.paths.owner)
        copy_regular(self.paths.binary, self.paths.backup / "binary", self.paths.owner, MAX_ARCHIVE, 0o755)
        transaction = {"protocol": 1, "request_id": identifier, "old_version": current,
                       "new_version": requested, "phase": "prepared", "database_exists": False,
                       "binary_sha256": digest(self.paths.backup / "binary", self.paths.owner, MAX_ARCHIVE)}
        atomic_json(self.paths.journal, transaction)
        try:
            self.system.stop()
            transaction["phase"] = "stopped"; atomic_json(self.paths.journal, transaction)
            database = self.paths.backup / "database"
            database.unlink(missing_ok=True)
            transaction["database_exists"] = self.system.backup_database(database)
            if transaction["database_exists"]:
                transaction["database_sha256"] = digest(database, self.paths.owner, MAX_DATABASE)
            transaction["phase"] = "backed_up"; atomic_json(self.paths.journal, transaction)
            copy_regular(candidate, self.paths.binary, self.paths.owner, MAX_ARCHIVE, 0o755)
            transaction["phase"] = "installed"; atomic_json(self.paths.journal, transaction)
            self.system.start()
            self.system.health(requested)
            transaction["phase"] = "healthy"; atomic_json(self.paths.journal, transaction)
        except Exception as error:
            try:
                self.rollback(transaction)
                transaction["phase"] = "rolled_back"; atomic_json(self.paths.journal, transaction)
                self.status(identifier, "rolled_back", f"Update failed; restored {current}: {error}", requested)
                self.finish()
            except Exception as recovery_error:
                raise UpdateError(f"Update failed ({error}); recovery incomplete ({recovery_error}). Transaction retained; stop the service and follow manual recovery instructions.") from None
            return
        self.status(identifier, "updated", f"Updated to {requested}; service and history storage healthy", requested)
        self.finish()

    def recover(self, identifier):
        transaction = load_json(self.paths.journal, self.paths.owner)
        self.validate_transaction(transaction)
        identifier = identifier or transaction["request_id"]
        self.status(identifier, "installing", "Recovering interrupted update; no new installation will start", transaction["new_version"])
        if transaction["phase"] in ("healthy", "rolled_back"):
            expected = transaction["new_version"] if transaction["phase"] == "healthy" else transaction["old_version"]
            self.system.start(); self.system.health(expected)
            self.status(identifier, "updated" if transaction["phase"] == "healthy" else "rolled_back", "Interrupted finalization completed; service healthy", transaction["new_version"])
            self.finish()
            return
        self.rollback(transaction)
        transaction["phase"] = "rolled_back"; atomic_json(self.paths.journal, transaction)
        self.status(identifier, "rolled_back", "Interrupted update restored; check again before another installation", transaction["new_version"])
        self.finish()

    def validate_transaction(self, transaction):
        if (transaction.get("protocol") != 1 or transaction.get("phase") not in ("prepared", "stopped", "backed_up", "installed", "healthy", "rolled_back")
                or not isinstance(transaction.get("request_id"), str) or type(transaction.get("database_exists")) is not bool):
            raise UpdateError("Unrecognized transaction journal; manual recovery required")
        version(transaction.get("old_version")); version(transaction.get("new_version"))

    def rollback(self, transaction):
        self.validate_transaction(transaction)
        if self.system.version(self.paths.binary) not in (transaction["old_version"], transaction["new_version"]):
            raise UpdateError("Installed binary differs from interrupted transaction; preserve manual installation and recover manually")
        if transaction["phase"] in ("backed_up", "installed", "healthy"):
            directory(self.paths.backup, self.paths.owner)
            binary = self.paths.backup / "binary"
            database = self.paths.backup / "database"
            if digest(binary, self.paths.owner, MAX_ARCHIVE) != transaction.get("binary_sha256"):
                raise UpdateError("Rollback binary checksum mismatch")
            if transaction["database_exists"] and digest(database, self.paths.owner, MAX_DATABASE) != transaction.get("database_sha256"):
                raise UpdateError("Rollback database checksum mismatch")
            self.system.stop()
            copy_regular(binary, self.paths.binary, self.paths.owner, MAX_ARCHIVE, 0o755)
            self.system.restore_database(database if transaction["database_exists"] else None)
        self.system.start()
        self.system.health(transaction["old_version"])

    def finish(self):
        self.system.cleanup()
        (self.paths.backup / "database").unlink(missing_ok=True)
        if self.paths.backup.exists(): sync_directory(self.paths.backup)
        self.paths.journal.unlink(missing_ok=True)
        sync_directory(self.paths.state)


def extract_binary(archive, stage):
    # First inflate to a bounded regular file. This bounds even malicious tar
    # extension headers before tarfile parses them; no unbounded getmembers().
    expanded = stage / "release.tar"
    total = 0
    deadline = time.monotonic() + 20
    with gzip.open(archive, "rb") as source, expanded.open("xb") as output:
        while block := source.read(65536):
            total += len(block)
            if total > 128 * 1024 * 1024 or time.monotonic() > deadline:
                raise UpdateError("Expanded archive exceeded size/time limit")
            output.write(block)
    candidate = stage / "candidate"
    found = False
    with tarfile.open(expanded, "r|") as archive_file:
        for count, member in enumerate(archive_file, 1):
            if count > 1024 or time.monotonic() > deadline:
                raise UpdateError("Archive member/time limit exceeded")
            parts = member.name.split("/")
            if (len(member.name) > 1024 or parts[0] != "oyasys" or any(part in ("..", ".") for part in parts)
                    or not (member.isdir() or member.isreg())):
                raise UpdateError("Unsafe archive member")
            if member.name != "oyasys/oyasys": continue
            if found or not member.isreg() or member.size < 64 or member.size > MAX_ARCHIVE:
                raise UpdateError("Archive must contain one bounded regular oyasys/oyasys executable")
            found = True
            source = archive_file.extractfile(member)
            with source, candidate.open("xb") as output:
                copied = 0
                while block := source.read(65536):
                    copied += len(block)
                    if copied > member.size: raise UpdateError("Invalid executable member size")
                    output.write(block)
                if copied != member.size: raise UpdateError("Truncated executable member")
                output.flush(); os.fsync(output.fileno())
    if not found: raise UpdateError("Release archive has no executable")
    with candidate.open("rb") as source: header = source.read(64)
    if header[:7] != b"\x7fELF\x02\x01\x01" or header[18:20] != b"\x3e\x00":
        raise UpdateError("Release executable must be a little-endian ELF64 x86_64 binary")
    candidate.chmod(0o755)
    return candidate


def main():
    if len(sys.argv) == 3 and sys.argv[1].startswith("--database-"):
        database_helper(sys.argv[1], Path(sys.argv[2]))
        return
    if len(sys.argv) != 1 or os.geteuid() != 0:
        raise UpdateError("Run the installed updater through oyasys-update.service as root")
    account = pwd.getpwnam("oyasys")
    if account.pw_uid == 0: raise UpdateError("OYAsys service account must not be root")
    paths = Paths(uid=account.pw_uid, gid=account.pw_gid)
    Updater(paths, System(paths), Github(paths)).run()


if __name__ == "__main__":
    try: main()
    except Exception as error:
        print("OYAsys updater: " + str(error), file=sys.stderr)
        sys.exit(1)
