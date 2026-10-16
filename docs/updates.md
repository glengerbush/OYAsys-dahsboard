# Settings updates

Install this release manually once to enable the update helper. After that, open **Settings → Software updates → Check for updates**. If a newer compatible stable release exists, choose **Install update**. The monitor briefly stops while its binary is replaced and verified. History and settings are preserved. A failed startup attempts rollback and reports the result.

The dashboard never checks GitHub automatically. Opening Settings reads local status only. Progress polling stops when Settings closes or the page is hidden. There is no additional idle updater process: the existing systemd manager watches a request path and starts a temporary Python 3 standard-library helper. Python, networking, checksum verification, and backups consume resources only during the requested operation.

Everyone allowed to reach this dashboard can request an update, just as they can change Settings. Keep the existing LAN/Tailscale access restrictions. Exact Host/Origin and CSRF checks protect browser actions. The helper accepts only check/install requests for this repository's stable releases. It cannot run a command or download a URL supplied by a browser.

## Release source and compatibility

The source is `glengerbush/OYAsys-dahsboard` on GitHub, using the latest stable release. Drafts, prereleases, older versions, unsupported platforms, and incompatible manifests are not installed. The source repository was public when this feature was implemented; no token is required for public releases. If no release is published, Settings reports that honestly. A commit or successful Actions artifact alone is not an update.

Each release contains:

- `oyasys-linux-x86_64.tar.gz`: executable, initial installer, helper, units, and documentation.
- `oyasys-linux-x86_64.tar.gz.sha256`: manual download verification.
- `oyasys-update.json`: version, platform, archive hash, update protocol, and database schema.

Version 1 of the updater protocol replaces only the executable, which embeds the frontend. It does not execute scripts from the downloaded archive or modify systemd, proxy, drivers, fan control, or environment configuration. Releases requiring a new updater protocol or database schema require a manual installation. The release publisher must bump the protocol for changes requiring deployment-file updates; do not mark such releases compatible simply because the binary builds.

Downloads use verified HTTPS and SHA-256. Trust rests in this repository's release publishing permissions and GitHub; a checksum is not an independent publisher signature. The GitHub API is documented at [releases](https://docs.github.com/en/rest/releases/releases) and [release assets](https://docs.github.com/en/rest/releases/assets). Systemd path units use the existing service manager: [systemd.path](https://www.freedesktop.org/software/systemd/man/latest/systemd.path.html).

## Installation and recovery details

The installer requires Python 3.9+ with SQLite and SSL support, plus the standard util-linux `flock` command. It installs `/opt/oyasys/update.py`, enables `oyasys-update.path`, and enables an update oneshot at boot to recover interrupted work. Normal monitoring remains under the `oyasys` account. The application can write only `/var/lib/oyasys-updater/inbox`; release metadata, status, staging, and recovery files are root-owned. Optional private-repository credentials belong in `/etc/oyasys/update-token`, owned by root with mode `0600`; they are never sent to the dashboard or forwarded to redirected download hosts.

The helper rechecks the selected release before installation and rejects changed assets. It accepts at most a 64 MiB archive, 128 MiB expanded tar, 1,024 archive members, and a 16 GiB database backup. These are safety bounds, not operating targets. Candidate `--version` and database operations run as the service user. The existing database lock and direct HTTP health response protect replacement/startup; no proxy restart is needed.

Successful installation or rollback deletes temporary database copies, preserving the configured retention policy. One prior binary remains in `last-backup`. An unresolved transaction keeps its recovery database until recovery succeeds; this exceptional copy can contain readings that later expire from the live database.

Settings writes are temporarily rejected while installation or recovery is unresolved, including writes from other tabs. This prevents an acknowledged settings change from being undone by database rollback. The browser keeps showing progress through a candidate restart and waits for the helper's confirmed outcome before reloading; rollback and failure remain visible.

For a failed or interrupted operation, inspect:

```sh
systemctl status oyasys-update.path oyasys-update.service
journalctl -u oyasys-update.service -n 50 --no-pager
cat /var/lib/oyasys-updater/status.json
```

After correcting a reported permission, space, or configuration problem, `sudo systemctl start oyasys-update.service` retries journal recovery. An interrupted transaction is finalized or rolled back; it never resumes downloading/installing a new candidate. Keep `transaction.json` and `last-backup` while recovery is unresolved. Do not run the installer over a pending transaction. If recovery cannot complete, stop the monitor and retain these files for manual recovery; do not delete a backup merely to clear the UI error.

A stale inbox receipt deliberately blocks ambiguous retries. Clear an abandoned receipt only after stopping the path watcher, confirming the update service is inactive, and confirming there is no unresolved transaction or queued installation. Then re-enable the watcher. This is an exceptional administrative recovery step, not part of normal one-click updates.

The updater has a finite systemd start limit (10 starts per minute), preventing repeated process launches if an installation error leaves its request unconsumed. After correcting that error, run `sudo systemctl reset-failed oyasys-update.service oyasys-update.path`, then `sudo systemctl start oyasys-update.path`. Normal operation requires no reset.

## Publishing an update

Make and review changes, update the package version in `Cargo.toml` and `Cargo.lock`, then test and commit. When ready to publish, push a matching stable tag, for example `v0.2.1` for version `0.2.1`. The Actions workflow runs Rust, browser, persistence, telemetry, and updater checks, builds on Ubuntu 22.04, verifies the tag matches the packaged binary, and publishes the three assets. A failed check prevents publication. Each release tag is unique; do not overwrite an existing version.

No release was published as part of implementing this feature. Initial server installation, real systemd activation, compatibility, proxy reachability, and resource qualification remain owner-run. Local tests use temporary files and service/network adapters; they cannot qualify the actual Minisforum installation.
