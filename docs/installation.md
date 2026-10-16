# Install OYAsys on the monitored server

OYAsys is a native Linux executable with embedded web assets and SQLite. It reads hardware, never controls fans. Installation and verification are manual on your server; no connection from the development computer is required.

## Release and compatibility

Use the Linux x86-64 release archive and its SHA-256 file. Verify before unpacking:

```sh
sha256sum -c oyasys-linux-x86_64.tar.gz.sha256
tar -xzf oyasys-linux-x86_64.tar.gz
cd oyasys
./oyasys discover
sudo sh deploy/install.sh
```

The installer creates a dedicated `oyasys` account, installs the executable and unit, and preserves existing data and configuration. It does not start the service or change networking, drivers, or fan control. A locally built archive inherits its build host's glibc requirement. Prefer the CI artifact built on Ubuntu 22.04 (glibc 2.35) for a Debian 12/DietPi installation; check `ldd --version` on your server. Older systems require a compatible GNU/Linux build. Do not use a musl build for native NVIDIA driver loading without separately qualifying it.

No Rust, Node, npm, or build tools are required on the server. Python 3 with its standard library is required by the on-demand Settings update helper; it does not run continuously. It also runs the optional profiling helper. NVIDIA metrics use the library already installed by the NVIDIA driver; other monitoring works without it.

## Verify sensors and the cooling fan

Run `./oyasys discover` as the intended service user once installed:

```sh
sudo -u oyasys /opt/oyasys/oyasys discover
```

Inspect the reported sensors, identities, and availability. The P100 cooling fan is believed to use chassis fan 2. This does **not** mean Linux's second sensor or `fan2_input`. Inspect your existing Linux cooling-profile script/configuration to identify the actual device and channel. Do not run PWM tests, change duty cycles, or stop that cooling service. OYAsys provides no fan-control action.

After verifying the mapping, select the matching RPM sensor in OYAsys Settings. Until then the P100 fan view says it is unassigned. A missing sensor is unavailable, not 0 RPM. If a driver does not expose the motherboard header, OYAsys cannot infer its speed. Grant only the particular read permissions needed; do not run the whole monitor as root to hide missing permissions. If NVIDIA permissions require an existing device-access group, add `oyasys` to that specific group and restart the monitor.

## Existing DNS and Coolify proxy

1. Ensure your actual OYAsys hostname resolves to the existing server IP through your DNS override or wildcard.
2. Inspect which proxy Coolify runs. The supplied example is for Traefik; retain Caddy if that is already configured.
3. Determine a **stable host address reachable from the proxy container**, such as its host-side bridge gateway. Configure OYAsys to bind that address on port 9850. Container `localhost` is not the host's loopback. Check whether `host.docker.internal` already resolves inside the proxy; otherwise use the verified host address or the proxy's managed host-gateway mapping.
4. Edit `/etc/oyasys/environment`, preserving its simple `KEY=value` format. Example values below are placeholders, not a network configuration to copy blindly:

```ini
OYASYS_BIND=HOST_BRIDGE_IP:9850
OYASYS_ORIGIN=https://oyasys.YOUR_DOMAIN
```

5. Through Coolify's **Servers → your server → Proxy → Dynamic Configurations**, add a hostname route to that reachable address. Adapt `deploy/traefik.example.yaml`: the example is HTTP-only, with a placeholder LAN allowlist. For HTTPS, use your existing HTTPS entry point and existing certificate/wildcard configuration. The certificate must cover the chosen hostname. Do not install another proxy or certificate daemon.
6. Restrict both the proxy route and the direct port to the intended proxy/LAN/Tailscale paths, using the existing firewall and proxy configuration. Use the actual source addresses the proxy sees after subnet routing. Never trust arbitrary forwarded client headers; retain the proxy's existing trusted-forwarder policy. A DNS override alone does not block public access.
7. Start and inspect:

```sh
sudo systemctl enable --now oyasys.service
systemctl status oyasys.service
journalctl -u oyasys.service -n 30 --no-pager
```

Visit the configured hostname. Verify it from the LAN and Tailscale, and verify that disallowed network paths and direct port access are rejected. Settings require the exact configured browser origin; a mismatch is intentionally rejected. Keep loopback defaults until the proxy/network route is ready. Binding to all interfaces is unnecessary when a stable proxy-facing interface exists.

Reference: [Coolify host-service routing](https://coolify.io/docs/core/networking/proxy/traefik/load-balancing).

## Settings and data

Settings provides sampling (1–3,600 seconds, default 10), full-detail retention (1–365 days, default 30), and the verified P100 cooling-fan source. No login is present: everyone with network access can change settings. Same-origin request checks still prevent unrelated websites from changing them through a visitor's browser.

Reducing retention requires an explicit deletion acknowledgement. Expired data disappears from query results immediately; bounded maintenance reclaims records and free space without blocking a large transaction. Increasing retention cannot restore expired history. No permanent summaries remain beyond retention.

History and settings live in `/var/lib/oyasys`; updates do not replace this directory. Telemetry favors low-I/O WAL synchronization: abrupt power loss may lose recent telemetry transactions. Settings use a durable transaction. For a consistent manual backup, stop OYAsys, copy the complete data directory, then restart; do not copy only the main database while ignoring a live WAL.

## Measure on this server

The profiler reads process/host counters; it never changes sampling or retention. Keep normal workloads stable, use the same intervals, and repeat comparisons. Idle includes no browser; a live browser should also be tested for client rendering costs separately.

```sh
# Host-only baseline with OYAsys stopped (optional, brief gap in history).
python3 tools/profile.py --seconds 120

# Run after OYAsys starts. Include several sample and maintenance cycles.
oyasys_pid=$(systemctl show --property MainPID --value oyasys.service)
python3 tools/profile.py --pid "$oyasys_pid" --seconds 120
python3 tools/profile.py --pid "$oyasys_pid" --seconds 120 --url https://oyasys.YOUR_DOMAIN --scenario live
python3 tools/profile.py --pid "$oyasys_pid" --seconds 120 --url https://oyasys.YOUR_DOMAIN --scenario clients
python3 tools/profile.py --pid "$oyasys_pid" --seconds 120 --url https://oyasys.YOUR_DOMAIN --scenario history
```

Use normal certificate verification. Do not disable TLS checks to work around a routing/certificate problem. The profiler may need same-user/root permission to read PSS and I/O; unavailable counters are identified rather than invented. Add `--data-dir /var/lib/oyasys` when permitted to inspect file growth.

Record one-core CPU percentage and CPU seconds, RSS/PSS, worker count, context-switch and I/O deltas, host cache changes, and database/WAL growth. Compare monitoring stopped, idle collection, live clients, long-range queries, and retention cleanup. The profile helper is a temporary observer and its overhead is separate. Host differences include unrelated processes, so do not attribute all movement to OYAsys. Local developer measurements do not prove Minisforum/P100 performance.

## Update, rollback, and remove

After installing version 0.2.0 or later with its helper, use **Settings → Software updates → Check for updates**, then **Install update** when offered. Checks occur only on request. Installation keeps a temporary recovery backup, briefly restarts monitoring, and verifies startup. See [update operations and release publishing](updates.md) for compatibility, recovery, and the release workflow.

For the initial upgrade from an older installation, or an update requiring deployment changes, stop the monitor, retain a consistent data backup, unpack a verified new release, rerun the installer, then start it. Configuration and history remain in place:

```sh
sudo systemctl stop oyasys
sudo sh deploy/install.sh
sudo systemctl start oyasys
```

Retain the prior executable for rollback. If a newer release migrates the schema, restore its corresponding stopped-service data backup before reverting to an incompatible executable. Never run two instances against the same data directory.

To disable, first stop/disable `oyasys-update.path`, let any active update finish, then disable `oyasys-update.service` and `oyasys.service`. Remove only OYAsys's executable, helper, units, and its specific proxy route when uninstalling; preserve `/var/lib/oyasys` and `/etc/oyasys` unless you separately choose to delete them. Inspect `/var/lib/oyasys-updater` for unresolved recovery state before removing it. No fan-control cleanup is needed because OYAsys never owns that configuration.
