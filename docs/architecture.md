# Architecture

One Rust process owns three fixed threads: the current-thread HTTP runtime, a collector/SQLite writer, and one history reader. Work queues and HTTP connections are bounded. Browser clients reuse completed samples; they never start another collector. Monitoring requires no subprocess. Software updates and folder analysis use temporary helpers only on explicit request.

## Modules

- `telemetry`: discovers kernel sources, keeps reusable file readers, and samples them on the selected cadence. Counter differences remain integer-exact until conversion to rates. Discovery is cached, explicitly refreshable, and retried after failures with backoff. Optional NVML keeps its library and device handles alive; unsupported GPU telemetry leaves other collectors working.
- `history`: owns SQLite persistence, settings durability, expiration, compact encoding, and streamed history reduction. One writer and one reader each use a 128 KiB SQLite page-cache target, with memory mapping disabled.
- `app`: schedules non-overlapping samples, shares completed state, validates HTTP requests and settings, and coordinates bounded work. Missed deadlines are skipped. Settings use revision checks, configured Host/Origin, a random per-process CSRF token, and bounded JSON bodies.
- `assets`/`web`: the executable serves embedded fingerprinted assets, precompressed during compilation. Browser JavaScript renders local uPlot canvas charts and pauses when hidden. No server-side rendering or browser framework runtime.
- `update`: bridges protected HTTP actions to a bounded filesystem inbox. It performs no networking, subprocess work, or background polling. A root-owned systemd oneshot helper consumes requests to check the fixed GitHub release source or install a verified compatible binary. The systemd path unit has no persistent helper process. Release credentials, if needed, stay outside the dashboard's readable files. See [update operations](updates.md).

The native systemd service runs under a dedicated account. Existing Coolify routes the chosen hostname to its host-facing listening address. The operator verifies network restrictions and fan identity locally on the actual server.

## Stored format and durability

SQLite `user_version=1` defines catalogs, samples, settings, and metadata. A sample row contains a monotonically increasing sequence, UTC milliseconds, catalog ID, collection cadence in milliseconds, and one BLOB. A time index supports range scans. Catalogs carry metric identity, label, unit, group, and source independently of individual observations.

The BLOB starts with version byte `1`, followed by one little-endian IEEE-754 `f64` per metric in catalog order. Finite values are gauges; NaN represents unavailable. Zero remains a valid measurement. Unrecognized versions or mismatched lengths fail visibly. Raw kernel counters are differenced as `u64` inside telemetry; the database stores resulting rates, not lossy floating-point copies of raw counters.

Telemetry commits one completed collection at a time in WAL mode with `synchronous=NORMAL`; sudden power loss can lose recent telemetry. Settings switch to `FULL` and commit before success. A durable sequence high-water mark prevents sequence reuse after expiration. A monotonic expiration floor prevents shortened retention from being undone by lengthening it before physical cleanup finishes.

Expiration hides old records immediately. Maintenance deletes bounded batches, reuses freed pages, incrementally vacuums, and checkpoints; there is no full periodic VACUUM or retained summary table. Space reclamation is eventual. Long readers have work/time/cancellation bounds so they cannot pin a WAL indefinitely. SQLite checkpoint and journal-size limits are soft during active readers, not a hard disk quota.

A persistence error leaves truthful live readings with `persisted=false`, reports unavailable history, and retries opening storage with backoff. There is no in-memory backlog of failed samples. Recovery leaves gaps rather than inventing data.

## Queries

The HTTP contract is in [api.md](api.md). Historical requests select at most 64 metrics, 2,000 points per series, and 365 days. Large scans are streamed through display buckets retaining sampled extrema, first/last values, timestamps, and missing intervals. Results exceeding the work or gap budget return an explicit error asking for a narrower range. No query silently substitutes a partial result. Display reduction never changes stored full-detail readings.

Incremental updates use sequence cursors. A cursor outside retained coverage requests a bounded range reload. The service accepts at most one active historical worker with a short queue; overload receives a retryable response. Settings, collection, and latest state remain independent of a long scan.

## Verification boundaries

Integration tests use the actual HTTP executable with deterministic synthetic readings, a controllable clock, and disposable SQLite. Browser tests use the same server. Resource experiments and manual commissioning are described in [performance.md](performance.md) and [installation.md](installation.md).

Local tests cannot verify NVIDIA driver ABI behavior on the user's P100, motherboard channel labeling, service-account permissions, or existing proxy configuration. Those remain manual commissioning items. No code writes PWM, fan, GPU control, or cooling-profile settings.

## Filesystem capacity and folder analysis

The physical-drive overview reuses cached whole-device discovery from sysfs, including unmounted drives. Partitions and virtual device-mapper/RAID layers do not become extra physical-drive cards; their existing I/O metrics remain available. Capacities are read once per discovery and are not added to sampled history. Unknown capacity stays unavailable.

Capacity bars reuse the existing filesystem sample: used bytes, bytes available to unprivileged processes, and their difference from total capacity as reserved space. They add no hardware reads or polling.

An explicit Analyze folders request starts one temporary worker and GNU `/usr/bin/du`, running as the existing service account at nice level 19. It measures allocated space, does not follow symlinks or cross filesystem device boundaries, and returns the largest 12 immediate folders, an aggregate for other folders, and files/directory metadata at the mount root. Hard links count once; shared extents, snapshots, and filesystem metadata can differ from filesystem usage.

Only one scan runs at a time. Limits are 10 seconds elapsed, 5 CPU seconds, 64 MiB child address space, and 256 KiB captured output. Scan completion or cancellation reaps the child and releases the worker. No scan runs at startup or on a timer; results remain only in the requesting browser until reload. Incomplete scans are labeled partial and may omit unfinished folders. Monitoring and history remain independent.

Existing service permissions and sandboxing still apply: protected home directories, Docker data, and other inaccessible paths may be absent. No elevated helper or permission changes are installed. GNU coreutils `du` is required only for this optional action.
