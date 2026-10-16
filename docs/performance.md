# Resource qualification

Minimize measured CPU and memory while preserving correct collection and charts. There is no arbitrary pass/fail budget and no claim of a universal minimum. This document separates local evidence from pending Minisforum/P100 measurements.

## Reproduce

Build with `cargo build --release --locked`. Use `tools/profile.py` against the running release executable, following [installation.md](installation.md). It records process CPU seconds, one-core CPU percentage, RSS/PSS, thread count, I/O, process counters, host cache context, and optional persistent-file sizes. It is a temporary external observer. CPU ticks have finite resolution; a zero delta means below that measurement's resolution.

Run comparable intervals with monitoring stopped, idle collection, one visible browser, several clients, a full-range request, and retention cleanup. Include multiple 10-second samples and 30-second maintenance cycles. Repeat cold and warm requests and open/close cycles. The HTTP load helper approximates request traffic; browser rendering must be measured in a real browser separately. Context-switch counters are not a complete kernel wakeup trace. Optional deeper wakeup tracing should remain temporary.

Synthetic qualification data:

```sh
python3 tools/make_fixture.py --days 30 --metrics 50 --output /tmp/oyasys-30days.json
./target/release/oyasys serve --fixture /tmp/oyasys-30days.json --data-dir /tmp/oyasys-qualification
```

Fixture import intentionally runs only for an empty database. It is development-only and loads the JSON fixture into memory; exclude import/compilation from steady-state numbers. After seeding, restart with a tiny fixture containing the same catalog and last sample to avoid including allocator retention from large JSON import. Never use fixture readings for actual monitoring.

## SQLite encoding comparison — local, 2026-10-16

`python3 tools/compare_storage.py` compares 259,200 collections (30 days at 10 seconds), each containing the same 50 finite numeric readings, under identical indexes, 512 KiB cache, WAL settings, and one transaction per collection. Each format runs in an isolated Python process. SQLite 3.53.4; these are encoding experiments, not process-footprint or P100 measurements.

| Format | Database bytes | Write CPU seconds | Read/decode all CPU seconds | Experiment peak RSS |
| --- | ---: | ---: | ---: | ---: |
| Versioned dense BLOB | 122,355,712 | 11.224 | 1.472 | 19,036 KiB |
| Wide numeric row | 137,154,560 | 14.130 | 4.654 | 19,312 KiB |

Dense storage was about 11% smaller, 21% less write CPU, and 68% less full-read/decode CPU in this experiment. Both produced the same checksum. Keep dense records for v1; Python binding costs mean these differences are evidence for this workload, not universal Rust performance ratios. Real catalogs with nulls or integer-like values may compress differently in SQLite's wide representation. Storage estimates in Settings use observed occupied pages and include fixed overhead, which dominates tiny histories.

## Native release measurements — local, 2026-10-16

Intel Core i7-1370P, Omarchy Linux, release build, 49 discovered kernel metrics, default 10-second cadence. NVIDIA/NVML hardware was unavailable. These short local observations establish a useful baseline, not target-server qualification. Other desktop work was present; host-wide cache/memory changes cannot be attributed solely to this service.

| Scenario | Interval | Service CPU seconds | One-core CPU | Peak RSS | Threads |
| --- | ---: | ---: | ---: | ---: | ---: |
| Native collection, no browser | 60s | 0.03 | 0.050% | 6,124 KiB | 3 |
| Three simultaneous incremental clients | 60s | 0.04 | 0.067% | 6,372 KiB | 3 |
| One visible Chromium dashboard | 60s | 0.04 | 0.067% | 9,160 KiB | 3 |

The visible-browser run used actual native readings and the accumulated local history, after initial page load. Client main-thread task time was 0.473 seconds during the minute; JavaScript heap used was about 3.3 MB (not total Chromium memory). No page errors occurred. Six subsequent open/load/close cycles left service RSS between 9,188 and 9,196 KiB, without continuing growth in that short experiment. Rendering work stays on the viewing device.

All 18 requests in the three-client run succeeded. The initial one-slot queue returned 9/27 overload responses under simultaneous polls; expanding the short queue to four entries removed those routine failures while retaining one worker. This trades a few queued request objects for usable concurrent tabs, without extra threads.

The 50-metric 30-day history fixture initially seeded 259,200 collections. Later queries include ongoing samples and rolling expiration. Two final range queries took 0.649s/0.478s with 1.09 total service CPU-seconds over 20 seconds and 8,128 KiB peak RSS. Both filesystem reads were warm; the first request was cold only with respect to this process's history reader. Do not present these as cold-disk results. Stored application data was about 123MB including catalog/index overhead; WAL adds a small changing amount.

A 31.7-second contention run completed 55 history requests while collecting three new samples. Latest-state requests stayed below 6ms in that run. Peaks and missing intervals survived reduction; an abandoned query did not leave the worker pinned. Earlier saturation exercised explicit 429 overload replies. The final queue bounds remain enforced; four concurrent callers can fit the active worker plus short queue.

### Retained optimizations

Typed history/update responses avoid an intermediate JSON value tree. Long-query peak RSS dropped from about 12MiB in the initial build to under 9MiB before the cache reduction; these runs were not controlled enough to attribute every byte to that change. Incremental responses additionally cap total returned values at 8,192, preventing a 1,000-row catch-up from expanding millions of entries for a wide catalog.

A controlled warm comparison then alternated 512/128KiB SQLite caches against the same 50-metric range and cloned database, with compilation stopped. Both produced 30,064 display points. Across three trials, CPU seconds were 0.81/0.43/0.47 for 512KiB and 0.73/0.43/0.48 for 128KiB; the latter saved 520KiB of service RSS in that comparison (other runs saved roughly 792KiB). Keep 128KiB per connection: a measurable memory reduction without an observed material CPU regression. This is evidence for the chosen workload, not a universal optimum. PSS during paired runs is affected by shared executable/library pages and is not used for that memory comparison.

Raw local reports are retained in [measurements](measurements/), including initial baselines and final runs. RSS/PSS include only the process; kernel filesystem cache and driver allocations may live elsewhere. Zero physical-I/O counter deltas on this filesystem do not mean there were no writes: syscall/character counts and WAL growth show activity. No permanent helper or observer runs with the utility.

## Failure evidence

`tests/storage_faults.py` enforces a child-only file-size limit to cause real SQLite write failure without filling the user's disk. It verifies rejected settings leave prior settings intact, current readings remain available with `persisted=false`, and storage recovers after restoring the limit. 192 failed collections with 1,024 readings showed only 4 KiB RSS spread locally, supporting the absence of an unwritten-sample backlog. SIGKILL restart preserves committed settings/history; future schema versions remain untouched and unavailable rather than being overwritten.

## Pending target-host qualification

The actual Minisforum/P100 remains unmeasured. Record service-account sensor availability, verified fan identity, NVML/driver memory, idle/live/multiple-browser CPU and memory, long-range queries, cleanup, repeated opens, I/O, WAL high-water, and host page-cache effects there. Compare any proposed runtime/cache/batching changes on identical hardware and telemetry before adopting them. Direct NVML avoids per-sample process startup by construction; a measured NVML versus `nvidia-smi` baseline requires the NVIDIA host. No local result establishes that cost.

## Settings updater idle check

The 0.2.0 update bridge adds no permanent thread or network poll. A 60-second local native-monitoring run with `OYASYS_UPDATER_DIR` configured used **0.02 CPU-seconds**, **6,136 KiB peak RSS**, and **3 threads**. This is consistent with the earlier roughly 6 MiB idle result; single short runs cannot attribute a few KiB or hundredths of a CPU-second to a change. Raw evidence: [update-bridge-idle.json](measurements/update-bridge-idle.json).

The local check used a temporary inbox and no requested update; it did not activate the real systemd path unit or exercise Minisforum/NVML. The installed path unit uses the existing systemd manager and has no updater process while idle. Explicit updates temporarily consume Python, download, checksum, and backup resources. The helper exits afterward. This is an idle-cost qualification, not a claim that installation itself is free.
