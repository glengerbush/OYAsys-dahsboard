//! Read-only host telemetry. Discovery/static specifications are cached; sampling
//! reuses open kernel files and buffers and launches no child processes.
mod nvml;

use crate::model::{Availability, Inventory, Metric, Spec};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const RETRY: Duration = Duration::from_secs(300);
const CATALOG_LIMIT: usize = 4096;

struct Reader {
    file: Option<File>,
    error: Option<ReadFailure>,
}
struct ReadFailure {
    status: &'static str,
    reason: String,
}
impl ReadFailure {
    fn from_error(error: std::io::Error) -> Self {
        let status = match error.raw_os_error() {
            Some(libc::EACCES | libc::EPERM) => "permission_denied",
            Some(libc::ENOENT | libc::ENODEV) => "lost",
            Some(libc::ENXIO | libc::EOPNOTSUPP) => "unsupported",
            _ if error.kind() == std::io::ErrorKind::InvalidData => "invalid",
            _ => "unavailable",
        };
        Self {
            status,
            reason: error.to_string(),
        }
    }
}
impl Reader {
    fn new(path: impl AsRef<Path>) -> Self {
        match File::open(path) {
            Ok(file) => Self {
                file: Some(file),
                error: None,
            },
            Err(error) => Self {
                file: None,
                error: Some(ReadFailure::from_error(error)),
            },
        }
    }
    fn failure(&self) -> (&str, &str) {
        self.error
            .as_ref()
            .map(|e| (e.status, e.reason.as_str()))
            .unwrap_or(("invalid", "Malformed or non-finite source reading"))
    }
    fn read<'a>(&mut self, buffer: &'a mut String) -> Option<&'a str> {
        buffer.clear();
        let file = self.file.as_mut()?;
        let result = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| file.take(1024 * 1024).read_to_string(buffer));
        if let Err(error) = result {
            self.error = Some(ReadFailure::from_error(error));
            self.file = None;
            return None;
        }
        Some(buffer)
    }
}

struct Sensor {
    id: String,
    reader: Reader,
    scale: f64,
    fault: Option<Reader>,
    enabled: Option<Reader>,
}
struct Disk {
    prefix: String,
    reader: Reader,
    previous: Option<(u64, u64)>,
}
struct Network {
    name: String,
    prefix: String,
    previous: Option<(u64, u64)>,
}
struct Filesystem {
    prefix: String,
    path: CString,
    device: libc::dev_t,
}

pub struct Telemetry {
    inventory: Inventory,
    sensors: Vec<Sensor>,
    disks: Vec<Disk>,
    networks: Vec<Network>,
    filesystems: Vec<Filesystem>,
    cpu: Reader,
    memory: Reader,
    network: Reader,
    uptime: Reader,
    buffer: String,
    small_buffer: String,
    previous_cpu: Option<[u64; 8]>,
    previous_time: Option<Instant>,
    refresh_needed: bool,
    next_refresh: Instant,
    gpu: Option<nvml::Nvml>,
    next_gpu_retry: Instant,
    source_keys: BTreeMap<String, String>,
    inventory_version: u64,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl Telemetry {
    pub fn new() -> Self {
        let now = Instant::now();
        let mut collector = Self {
            inventory: Inventory::default(),
            sensors: vec![],
            disks: vec![],
            networks: vec![],
            filesystems: vec![],
            cpu: Reader::new("/proc/stat"),
            memory: Reader::new("/proc/meminfo"),
            network: Reader::new("/proc/net/dev"),
            uptime: Reader::new("/proc/uptime"),
            buffer: String::with_capacity(8192),
            small_buffer: String::with_capacity(64),
            previous_cpu: None,
            previous_time: None,
            refresh_needed: false,
            next_refresh: now + RETRY,
            gpu: None,
            next_gpu_retry: now + RETRY,
            source_keys: BTreeMap::new(),
            inventory_version: 0,
        };
        collector.discover();
        collector.open_gpu();
        collector
    }

    pub fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }

    /// Cheap change token; consumers clone the catalog only when this changes.
    pub fn inventory_version(&self) -> u64 {
        self.inventory_version
    }

    /// Explicit operator-triggered rediscovery; not called by HTTP state reads.
    pub fn refresh(&mut self) {
        self.discover();
        self.gpu = None;
        self.open_gpu();
    }

    pub fn sample(&mut self) -> BTreeMap<String, Option<f64>> {
        let now = Instant::now();
        if self.refresh_needed && now >= self.next_refresh {
            self.discover();
        }
        if self.gpu.is_none() && now >= self.next_gpu_retry {
            self.open_gpu();
        }
        let elapsed = self
            .previous_time
            .map(|old| now.duration_since(old).as_secs_f64())
            .filter(|v| *v > 0.);
        self.previous_time = Some(now);
        let mut values: BTreeMap<_, _> = self
            .inventory
            .metrics
            .iter()
            .map(|m| (m.id.clone(), None))
            .collect();
        self.sample_cpu(&mut values);
        self.sample_memory(&mut values);
        self.sample_network(&mut values, elapsed);
        if let Some(raw) = self.uptime.read(&mut self.buffer) {
            values.insert(
                "system.uptime".into(),
                raw.split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .filter(|v| v.is_finite()),
            );
        } else {
            self.refresh_needed = true;
        }
        if values.get("system.uptime").is_none_or(Option::is_none) {
            set_reader_failure(
                &mut self.inventory.availability,
                &mut self.inventory_version,
                "system.uptime",
                &self.uptime,
            );
        }
        for sensor in &mut self.sensors {
            let fault = sensor
                .fault
                .as_mut()
                .map(|r| read_number(r, &mut self.small_buffer));
            let enabled = sensor
                .enabled
                .as_mut()
                .map(|r| read_number(r, &mut self.small_buffer));
            let reading = read_number(&mut sensor.reader, &mut self.small_buffer)
                .filter(|v| sensor.scale != 1. || *v >= 0.);
            if reading.is_none() || fault == Some(None) || enabled == Some(None) {
                self.refresh_needed = true;
            }
            let healthy = fault.is_none_or(|v| v == Some(0.))
                && enabled.is_none_or(|v| v.is_some_and(|v| v != 0.));
            if reading.is_none() {
                set_reader_failure(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    &sensor.id,
                    &sensor.reader,
                );
            } else if fault == Some(None) {
                set_reader_failure(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    &sensor.id,
                    sensor.fault.as_ref().unwrap(),
                );
            } else if enabled == Some(None) {
                set_reader_failure(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    &sensor.id,
                    sensor.enabled.as_ref().unwrap(),
                );
            } else if !healthy {
                set_availability(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    &sensor.id,
                    "unavailable",
                    Some(if enabled == Some(Some(0.)) {
                        "Sensor disabled by host configuration"
                    } else {
                        "Hardware reports sensor fault"
                    }),
                );
            }
            values.insert(
                sensor.id.clone(),
                reading.filter(|_| healthy).map(|v| v * sensor.scale),
            );
        }
        for disk in &mut self.disks {
            let counters = disk.reader.read(&mut self.buffer).and_then(|raw| {
                let mut fields = raw.split_whitespace();
                let read = fields.nth(2)?.parse::<u64>().ok()?;
                let written = fields.nth(3)?.parse::<u64>().ok()?;
                Some((read, written))
            });
            if counters.is_none() {
                self.refresh_needed = true;
            }
            let (read, written) = rates(disk.previous, counters, elapsed, 512.);
            for (suffix, value) in [("read", read), ("write", written)] {
                if value.is_none() {
                    let id = format!("{}.{suffix}", disk.prefix);
                    if counters.is_none() {
                        set_reader_failure(
                            &mut self.inventory.availability,
                            &mut self.inventory_version,
                            &id,
                            &disk.reader,
                        );
                    } else {
                        set_availability(
                            &mut self.inventory.availability,
                            &mut self.inventory_version,
                            &id,
                            "warming_up",
                            Some("Waiting for two valid counters; initial sample or counter reset"),
                        );
                    }
                }
            }
            disk.previous = counters;
            values.insert(format!("{}.read", disk.prefix), read);
            values.insert(format!("{}.write", disk.prefix), written);
        }
        for filesystem in &self.filesystems {
            let mut identity = std::mem::MaybeUninit::<libc::stat>::uninit();
            // Verify the path still belongs to the discovered mounted device:
            // after unmount statvfs would otherwise report the parent filesystem.
            let stat_result =
                unsafe { libc::stat(filesystem.path.as_ptr(), identity.as_mut_ptr()) };
            let source_error = if stat_result != 0 {
                Some(ReadFailure::from_error(std::io::Error::last_os_error()))
            } else if unsafe { identity.assume_init().st_dev } != filesystem.device {
                Some(ReadFailure {
                    status: "lost",
                    reason: "Mounted filesystem no longer matches discovered device".into(),
                })
            } else {
                None
            };
            if let Some(error) = source_error {
                for suffix in ["total", "used", "available"] {
                    set_availability(
                        &mut self.inventory.availability,
                        &mut self.inventory_version,
                        &format!("{}.{suffix}", filesystem.prefix),
                        error.status,
                        Some(&error.reason),
                    );
                }
                self.refresh_needed = true;
                continue;
            }
            let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            // SAFETY: CString is NUL-terminated, output points to correctly sized
            // storage; initialized fields are read only when statvfs succeeds.
            let result = unsafe { libc::statvfs(filesystem.path.as_ptr(), stats.as_mut_ptr()) };
            if result == 0 {
                let stats = unsafe { stats.assume_init() };
                let block = stats.f_frsize as f64;
                values.insert(
                    format!("{}.total", filesystem.prefix),
                    Some(stats.f_blocks as f64 * block),
                );
                values.insert(
                    format!("{}.used", filesystem.prefix),
                    Some(stats.f_blocks.saturating_sub(stats.f_bfree) as f64 * block),
                );
                values.insert(
                    format!("{}.available", filesystem.prefix),
                    Some(stats.f_bavail as f64 * block),
                );
            } else {
                let error = ReadFailure::from_error(std::io::Error::last_os_error());
                for suffix in ["total", "used", "available"] {
                    set_availability(
                        &mut self.inventory.availability,
                        &mut self.inventory_version,
                        &format!("{}.{suffix}", filesystem.prefix),
                        error.status,
                        Some(&error.reason),
                    );
                }
                self.refresh_needed = true;
            }
        }
        if self.gpu.is_none() {
            for metric in &self.inventory.metrics {
                if metric.group == "gpu"
                    && self
                        .inventory
                        .availability
                        .get(&metric.id)
                        .is_some_and(|a| matches!(a.status.as_str(), "available" | "warming_up"))
                {
                    set_availability(
                        &mut self.inventory.availability,
                        &mut self.inventory_version,
                        &metric.id,
                        "lost",
                        Some("NVIDIA device unavailable; waiting for bounded retry"),
                    );
                }
            }
        }
        let gpu_lost = self.gpu.as_mut().is_some_and(|gpu| {
            let lost = gpu.sample(&mut values);
            gpu.availability(
                &mut self.inventory.availability,
                &mut self.inventory_version,
            );
            lost
        });
        if gpu_lost {
            self.gpu = None;
            self.next_gpu_retry = now + RETRY;
            self.warn("NVIDIA device/driver unavailable; retrying after five minutes");
        }
        // Readers only publish catalogued IDs; future hotplug cannot expand a
        // sample beyond the bounded catalog even when optional devices overflow.
        values.retain(|id, value| {
            if !self.inventory.availability.contains_key(id) {
                return false;
            }
            if value.is_some() {
                set_availability(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    id,
                    "available",
                    None,
                );
            }
            true
        });
        values
    }

    fn sample_cpu(&mut self, values: &mut BTreeMap<String, Option<f64>>) {
        let current = self.cpu.read(&mut self.buffer).and_then(|raw| {
            let mut fields = raw.lines().next()?.split_whitespace();
            if fields.next()? != "cpu" {
                return None;
            }
            let mut counters = [0; 8];
            for counter in &mut counters {
                *counter = fields.next()?.parse::<u64>().ok()?;
            }
            Some(counters)
        });
        let utilization = self.previous_cpu.zip(current).and_then(|(old, new)| {
            // guest/guest_nice are already included in user/nice: only first 8
            // counters count. Any reset invalidates the entire rate sample.
            let mut delta = [0u64; 8];
            for i in 0..8 {
                delta[i] = new[i].checked_sub(old[i])?;
            }
            let total = delta.iter().try_fold(0u64, |a, b| a.checked_add(*b))?;
            let idle = delta[3].checked_add(delta[4])?;
            (total > 0).then(|| (total - idle) as f64 * 100. / total as f64)
        });
        if current.is_none() {
            self.refresh_needed = true;
        }
        if utilization.is_none() {
            if current.is_none() {
                set_reader_failure(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    "cpu.utilization",
                    &self.cpu,
                );
            } else {
                set_availability(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    "cpu.utilization",
                    "warming_up",
                    Some("Waiting for two valid CPU counters; initial sample or counter reset"),
                );
            }
        }
        self.previous_cpu = current;
        values.insert("cpu.utilization".into(), utilization);
    }

    fn sample_memory(&mut self, values: &mut BTreeMap<String, Option<f64>>) {
        let Some(raw) = self.memory.read(&mut self.buffer) else {
            self.refresh_needed = true;
            for id in [
                "memory.total",
                "memory.available",
                "memory.used",
                "memory.swap_total",
                "memory.swap_used",
            ] {
                set_reader_failure(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    id,
                    &self.memory,
                );
            }
            return;
        };
        let mut total = None;
        let mut available = None;
        let mut swap_total = None;
        let mut swap_free = None;
        for line in raw.lines() {
            let Some((key, rest)) = line.split_once(':') else {
                continue;
            };
            let destination = match key {
                "MemTotal" => &mut total,
                "MemAvailable" => &mut available,
                "SwapTotal" => &mut swap_total,
                "SwapFree" => &mut swap_free,
                _ => continue,
            };
            *destination = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .and_then(|v| v.checked_mul(1024));
        }
        for (id, value) in [
            ("memory.total", total),
            ("memory.available", available),
            (
                "memory.used",
                total.zip(available).and_then(|(a, b)| a.checked_sub(b)),
            ),
            ("memory.swap_total", swap_total),
            (
                "memory.swap_used",
                swap_total
                    .zip(swap_free)
                    .and_then(|(a, b)| a.checked_sub(b)),
            ),
        ] {
            if value.is_none() {
                set_availability(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    id,
                    "unavailable",
                    Some("Required memory counters missing or invalid in /proc/meminfo"),
                );
            }
            values.insert(id.into(), value.map(|v| v as f64));
        }
    }

    fn sample_network(&mut self, values: &mut BTreeMap<String, Option<f64>>, elapsed: Option<f64>) {
        let Some(raw) = self.network.read(&mut self.buffer) else {
            self.refresh_needed = true;
            for interface in &mut self.networks {
                interface.previous = None;
                for suffix in ["received", "sent"] {
                    set_reader_failure(
                        &mut self.inventory.availability,
                        &mut self.inventory_version,
                        &format!("{}.{suffix}", interface.prefix),
                        &self.network,
                    );
                }
            }
            return;
        };
        for interface in &mut self.networks {
            let record = raw.lines().find_map(|line| {
                let (name, data) = line.split_once(':')?;
                if name.trim() != interface.name {
                    return None;
                }
                Some(data)
            });
            let counters = record.and_then(|data| {
                let mut fields = data.split_whitespace();
                let received = fields.next()?.parse::<u64>().ok()?;
                let sent = fields.nth(7)?.parse::<u64>().ok()?;
                Some((received, sent))
            });
            if counters.is_none() {
                self.refresh_needed = true;
            }
            let (received, sent) = rates(interface.previous, counters, elapsed, 1.);
            for (suffix, value) in [("received", received), ("sent", sent)] {
                if value.is_none() {
                    let (status, reason) = if record.is_none() {
                        ("lost", "Network interface disappeared")
                    } else if counters.is_none() {
                        ("invalid", "Malformed network counters")
                    } else {
                        (
                            "warming_up",
                            "Waiting for two valid counters; initial sample or counter reset",
                        )
                    };
                    set_availability(
                        &mut self.inventory.availability,
                        &mut self.inventory_version,
                        &format!("{}.{suffix}", interface.prefix),
                        status,
                        Some(reason),
                    );
                }
            }
            interface.previous = counters;
            values.insert(format!("{}.received", interface.prefix), received);
            values.insert(format!("{}.sent", interface.prefix), sent);
        }
    }

    fn discover(&mut self) {
        self.inventory_version = self.inventory_version.wrapping_add(1);
        // Retain previously catalogued identities: a disappearing sensor is
        // still available to history and fan mapping, with null live readings.
        self.sensors.clear();
        self.disks.clear();
        self.networks.clear();
        self.filesystems.clear();
        self.inventory.hostname =
            read_text("/proc/sys/kernel/hostname").unwrap_or_else(|| "Linux server".into());
        self.inventory.system.clear();
        for (label, path) in [
            ("Kernel", "/proc/sys/kernel/osrelease"),
            ("Board", "/sys/class/dmi/id/board_name"),
            ("Manufacturer", "/sys/class/dmi/id/sys_vendor"),
            ("System", "/sys/class/dmi/id/product_name"),
            ("BIOS", "/sys/class/dmi/id/bios_version"),
        ] {
            if let Some(value) = read_text(path) {
                self.inventory.system.push(Spec {
                    label: label.into(),
                    value,
                });
            }
        }
        if let Some(cpu) = read_text("/proc/cpuinfo") {
            if let Some(model) = cpu.lines().find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.trim() == "model name")
                    .map(|(_, v)| v.trim())
            }) {
                self.inventory.system.push(Spec {
                    label: "CPU".into(),
                    value: model.into(),
                });
            }
            self.inventory.system.push(Spec {
                label: "Logical CPUs".into(),
                value: cpu
                    .lines()
                    .filter(|line| line.starts_with("processor\t"))
                    .count()
                    .to_string(),
            });
        }
        if let Some(memory) = read_text("/proc/meminfo") {
            if let Some(ram) = memory
                .lines()
                .find_map(|line| line.strip_prefix("MemTotal:"))
                .map(str::trim)
            {
                self.inventory.system.push(Spec {
                    label: "Installed usable RAM".into(),
                    value: ram.into(),
                });
            }
        }
        self.add(
            "cpu.utilization",
            "CPU utilization",
            "%",
            "cpu",
            "/proc/stat",
        );
        for (id, label) in [
            ("memory.total", "RAM total"),
            ("memory.used", "RAM used"),
            ("memory.available", "RAM available"),
            ("memory.swap_total", "Swap total"),
            ("memory.swap_used", "Swap used"),
        ] {
            self.add(id, label, "bytes", "memory", "/proc/meminfo");
        }
        self.add("system.uptime", "Uptime", "s", "system", "/proc/uptime");
        self.discover_sensors();
        self.discover_disks();
        self.discover_network();
        self.discover_filesystems();
        self.cpu = Reader::new("/proc/stat");
        self.memory = Reader::new("/proc/meminfo");
        self.network = Reader::new("/proc/net/dev");
        self.uptime = Reader::new("/proc/uptime");
        self.previous_cpu = None;
        self.previous_time = None;
        let mut active = BTreeSet::new();
        active.extend(self.sensors.iter().map(|s| s.id.clone()));
        for disk in &self.disks {
            for suffix in ["read", "write"] {
                active.insert(format!("{}.{suffix}", disk.prefix));
            }
        }
        for network in &self.networks {
            for suffix in ["received", "sent"] {
                active.insert(format!("{}.{suffix}", network.prefix));
            }
        }
        for fs in &self.filesystems {
            for suffix in ["total", "used", "available"] {
                active.insert(format!("{}.{suffix}", fs.prefix));
            }
        }
        // Keep bounded retries after a disappeared device is absent at rescan;
        // retaining its catalog entry alone must not stop reappearance recovery.
        self.refresh_needed = self.sensors.is_empty()
            || self.inventory.metrics.iter().any(|m| {
                ["hwmon.", "disk.", "network.", "filesystem."]
                    .iter()
                    .any(|prefix| m.id.starts_with(prefix))
                    && !active.contains(&m.id)
            });
        for metric in &self.inventory.metrics {
            if ["hwmon.", "disk.", "network.", "filesystem."]
                .iter()
                .any(|prefix| metric.id.starts_with(prefix))
                && !active.contains(&metric.id)
            {
                set_availability(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                    &metric.id,
                    "lost",
                    Some("Previously discovered source is no longer present"),
                );
            }
        }
        self.next_refresh = Instant::now() + RETRY;
        self.inventory.metrics.sort_by(|a, b| a.id.cmp(&b.id));
    }

    fn discover_sensors(&mut self) {
        for entry in entries("/sys/class/hwmon") {
            let Ok(canonical) = fs::canonicalize(&entry) else {
                continue;
            };
            let chip = read_text(entry.join("name")).unwrap_or_else(|| "sensor".into());
            let mut device = fs::canonicalize(entry.join("device")).unwrap_or_else(|_| {
                // Typical class target: physical-device/hwmon/hwmonN. Remove
                // both ephemeral hwmon path components, retaining device path.
                canonical
                    .parent()
                    .filter(|p| p.file_name().is_some_and(|n| n == "hwmon"))
                    .and_then(Path::parent)
                    .or_else(|| canonical.parent())
                    .unwrap_or(&canonical)
                    .to_path_buf()
            });
            let serial = read_text(device.join("serial"));
            if let Ok(physical) = fs::canonicalize(device.join("device")) {
                device = physical;
            }
            // NVMe controller numbering may change across boots; its serial is
            // preferable to nvmeN in the class path when the driver exposes it.
            let hardware = serial.unwrap_or_else(|| device.display().to_string());
            if device
                .to_string_lossy()
                .contains("/virtual/thermal/thermal_zone")
            {
                self.warn("Some virtual thermal sources lack physical identity; their channel IDs may change after reboot");
            }
            let identity = format!("{hardware}:{chip}");
            let Some(key) = self.source_key(&identity) else {
                continue;
            };
            for input in entries(&entry) {
                let Some(filename) = input.file_name().and_then(|v| v.to_str()) else {
                    continue;
                };
                let Some(channel) = filename.strip_suffix("_input") else {
                    continue;
                };
                let (kind, unit, scale) = if channel.strip_prefix("temp").is_some_and(is_digits) {
                    ("thermal", "°C", 0.001)
                } else if channel.strip_prefix("fan").is_some_and(is_digits) {
                    ("fan", "RPM", 1.)
                } else {
                    continue;
                };
                let id = format!("hwmon.{key}.{channel}");
                let label = read_text(entry.join(format!("{channel}_label")))
                    .unwrap_or_else(|| channel.into());
                if !self.add(
                    &id,
                    &format!("{chip} · {label}"),
                    unit,
                    kind,
                    &format!("{} · {channel}", device.display()),
                ) {
                    continue;
                }
                let fault_path = entry.join(format!("{channel}_fault"));
                let enabled_path = entry.join(format!("{channel}_enable"));
                self.sensors.push(Sensor {
                    id,
                    reader: Reader::new(&input),
                    scale,
                    fault: fault_path.exists().then(|| Reader::new(fault_path)),
                    enabled: enabled_path.exists().then(|| Reader::new(enabled_path)),
                });
            }
        }
        if self.sensors.is_empty() {
            self.warn("No readable hwmon temperature/fan channels discovered; inspect host driver support");
        }
    }

    fn discover_disks(&mut self) {
        for entry in entries("/sys/class/block") {
            let name = entry.file_name().unwrap_or_default().to_string_lossy();
            if entry.join("partition").exists()
                || name.starts_with("loop")
                || name.starts_with("ram")
                || name.starts_with("zram")
            {
                continue;
            }
            let canonical = fs::canonicalize(&entry).unwrap_or_else(|_| entry.clone());
            let id = read_text(entry.join("wwid"))
                .or_else(|| read_text(entry.join("device/wwid")))
                .or_else(|| read_text(entry.join("device/serial")))
                .or_else(|| read_text(entry.join("dm/uuid")))
                .unwrap_or_else(|| canonical.display().to_string());
            let Some(key) = self.source_key(&id) else {
                continue;
            };
            let prefix = format!("disk.{key}");
            let model = read_text(entry.join("device/model")).unwrap_or_else(|| name.to_string());
            let source = format!("{model} · {id}");
            self.add(
                &format!("{prefix}.read"),
                &format!("{name} read"),
                "bytes/s",
                "storage",
                &source,
            );
            self.add(
                &format!("{prefix}.write"),
                &format!("{name} write"),
                "bytes/s",
                "storage",
                &source,
            );
            if !self
                .inventory
                .availability
                .contains_key(&format!("{prefix}.read"))
                && !self
                    .inventory
                    .availability
                    .contains_key(&format!("{prefix}.write"))
            {
                continue;
            }
            if let Some(size) = read_text(entry.join("size"))
                .and_then(|v| v.parse::<u64>().ok())
                .and_then(|v| v.checked_mul(512))
            {
                self.inventory.system.push(Spec {
                    label: format!("Disk {name}"),
                    value: format!("{model} · {size} bytes · {id}"),
                });
            }
            self.disks.push(Disk {
                prefix,
                reader: Reader::new(canonical.join("stat")),
                previous: None,
            });
        }
    }

    fn discover_network(&mut self) {
        for entry in entries("/sys/class/net") {
            let name = entry
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if name == "lo" || name.starts_with("veth") {
                continue;
            }
            let Ok(path) = fs::canonicalize(&entry) else {
                continue;
            };
            let source = path.display().to_string();
            let Some(key) = self.source_key(&source) else {
                continue;
            };
            let prefix = format!("network.{key}");
            self.add(
                &format!("{prefix}.received"),
                &format!("{name} received"),
                "bytes/s",
                "network",
                &source,
            );
            self.add(
                &format!("{prefix}.sent"),
                &format!("{name} sent"),
                "bytes/s",
                "network",
                &source,
            );
            if !self
                .inventory
                .availability
                .contains_key(&format!("{prefix}.received"))
                && !self
                    .inventory
                    .availability
                    .contains_key(&format!("{prefix}.sent"))
            {
                continue;
            }
            self.networks.push(Network {
                name,
                prefix,
                previous: None,
            });
        }
    }

    fn discover_filesystems(&mut self) {
        let Some(mounts) = read_text("/proc/self/mountinfo") else {
            return;
        };
        let mut seen = BTreeSet::new();
        let uuid_paths: BTreeMap<_, _> = entries("/dev/disk/by-uuid")
            .into_iter()
            .filter_map(|p| {
                Some((
                    fs::canonicalize(&p).ok()?,
                    p.file_name()?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        for line in mounts.lines() {
            let Some((left, right)) = line.split_once(" - ") else {
                continue;
            };
            let left: Vec<_> = left.split_whitespace().collect();
            let mut right = right.split_whitespace();
            let (Some(filesystem), Some(source)) = (right.next(), right.next()) else {
                continue;
            };
            if left.len() < 5
                || !matches!(
                    filesystem,
                    "ext2"
                        | "ext3"
                        | "ext4"
                        | "xfs"
                        | "btrfs"
                        | "f2fs"
                        | "zfs"
                        | "vfat"
                        | "exfat"
                        | "ntfs"
                        | "ntfs3"
                )
            {
                continue;
            }
            if !seen.insert(left[2].to_owned()) {
                continue;
            }
            let mountpoint = unescape_mount(left[4]);
            let source = unescape_mount(source);
            let identity = fs::canonicalize(&source)
                .ok()
                .and_then(|p| uuid_paths.get(&p))
                .cloned()
                .unwrap_or_else(|| source.clone());
            let Some(key) = self.source_key(&identity) else {
                continue;
            };
            let prefix = format!("filesystem.{key}");
            let Ok(path) = CString::new(mountpoint.as_bytes()) else {
                continue;
            };
            // Btrfs subvolumes can report a different st_dev from mountinfo.
            // Cache the actual path identity, then compare like with like.
            let mut device_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: valid NUL-terminated path and appropriately sized output;
            // output is consumed only after a successful stat call.
            if unsafe { libc::stat(path.as_ptr(), device_stat.as_mut_ptr()) } != 0 {
                continue;
            }
            let device = unsafe { device_stat.assume_init().st_dev };
            for (suffix, label) in [
                ("total", "capacity"),
                ("used", "used"),
                ("available", "available"),
            ] {
                self.add(
                    &format!("{prefix}.{suffix}"),
                    &format!("{mountpoint} {label}"),
                    "bytes",
                    "storage",
                    &format!("{filesystem} · {identity} · {mountpoint}"),
                );
            }
            if !["total", "used", "available"].iter().any(|suffix| {
                self.inventory
                    .availability
                    .contains_key(&format!("{prefix}.{suffix}"))
            }) {
                continue;
            }
            self.filesystems.push(Filesystem {
                prefix,
                path,
                device,
            });
        }
    }

    fn open_gpu(&mut self) {
        self.inventory_version = self.inventory_version.wrapping_add(1);
        self.next_gpu_retry = Instant::now() + RETRY;
        self.inventory
            .warnings
            .retain(|w| !w.starts_with("NVIDIA") && !w.starts_with("No readable NVIDIA"));
        match nvml::Nvml::open() {
            Ok((gpu, metrics)) => {
                for metric in metrics {
                    self.add(
                        &metric.id,
                        &metric.label,
                        &metric.unit,
                        &metric.group,
                        &metric.source,
                    );
                }
                gpu.availability(
                    &mut self.inventory.availability,
                    &mut self.inventory_version,
                );
                self.gpu = Some(gpu);
                self.inventory.metrics.sort_by(|a, b| a.id.cmp(&b.id));
            }
            Err(error) => {
                self.warn(&error);
                for metric in &self.inventory.metrics {
                    if metric.group == "gpu" {
                        set_availability(
                            &mut self.inventory.availability,
                            &mut self.inventory_version,
                            &metric.id,
                            "lost",
                            Some(&error),
                        );
                    }
                }
            }
        }
    }

    fn add(&mut self, id: &str, label: &str, unit: &str, group: &str, source: &str) -> bool {
        if let Some(metric) = self.inventory.metrics.iter_mut().find(|m| m.id == id) {
            if metric.label != label
                || metric.unit != unit
                || metric.group != group
                || metric.source != source
            {
                metric.label = label.into();
                metric.unit = unit.into();
                metric.group = group.into();
                metric.source = source.into();
                self.inventory_version = self.inventory_version.wrapping_add(1);
            }
        } else {
            if self.inventory.metrics.len() >= CATALOG_LIMIT {
                self.warn("Metric catalog reached 4096 sources; additional sources omitted to bound memory. Existing identities retained.");
                return false;
            }
            self.inventory_version = self.inventory_version.wrapping_add(1);
            self.inventory.metrics.push(Metric {
                id: id.into(),
                label: label.into(),
                unit: unit.into(),
                group: group.into(),
                source: source.into(),
            });
            self.inventory.availability.insert(
                id.into(),
                Availability {
                    status: "warming_up".into(),
                    reason: Some("Waiting for first collection".into()),
                },
            );
        }
        true
    }
    fn warn(&mut self, message: &str) {
        if !self.inventory.warnings.iter().any(|v| v == message) {
            self.inventory_version = self.inventory_version.wrapping_add(1);
            self.inventory.warnings.push(message.into());
        }
    }
    fn source_key(&mut self, source: &str) -> Option<String> {
        let key = stable_key(source);
        if let Some(existing) = self.source_keys.get(&key) {
            if existing != source {
                self.warn("Hardware identity collision; ambiguous source omitted. Report this commissioning error.");
                return None;
            }
        } else {
            if self.source_keys.len() >= CATALOG_LIMIT {
                self.warn("Source identity limit reached; additional devices omitted to bound discovery memory");
                return None;
            }
            self.source_keys.insert(key.clone(), source.to_owned());
        }
        Some(key)
    }
}

fn rates(
    old: Option<(u64, u64)>,
    new: Option<(u64, u64)>,
    seconds: Option<f64>,
    scale: f64,
) -> (Option<f64>, Option<f64>) {
    match (old, new, seconds) {
        (Some((a, b)), Some((c, d)), Some(seconds)) => (
            c.checked_sub(a).map(|v| v as f64 * scale / seconds),
            d.checked_sub(b).map(|v| v as f64 * scale / seconds),
        ),
        _ => (None, None),
    }
}
fn set_availability(
    map: &mut BTreeMap<String, Availability>,
    version: &mut u64,
    id: &str,
    status: &str,
    reason: Option<&str>,
) {
    let Some(current) = map.get_mut(id) else {
        return;
    };
    if current.status != status || current.reason.as_deref() != reason {
        current.status = status.into();
        current.reason = reason.map(str::to_owned);
        *version = version.wrapping_add(1);
    }
}
fn set_reader_failure(
    map: &mut BTreeMap<String, Availability>,
    version: &mut u64,
    id: &str,
    reader: &Reader,
) {
    let (status, reason) = reader.failure();
    set_availability(map, version, id, status, Some(reason));
}
fn read_number(reader: &mut Reader, buffer: &mut String) -> Option<f64> {
    reader
        .read(buffer)?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}
fn read_text(path: impl AsRef<Path>) -> Option<String> {
    let mut value = String::new();
    File::open(path)
        .ok()?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut value)
        .ok()?;
    if value.len() > 1024 * 1024 {
        return None;
    }
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}
fn entries(path: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .take(CATALOG_LIMIT * 4)
        .map(|e| e.path())
        .collect();
    paths.sort();
    paths
}
fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}
fn stable_key(value: &str) -> String {
    // FNV-1a 128 uses specified constants, not Rust's randomized/default
    // hasher. Identifiers remain compact and stable across process/compiler
    // versions; source_key separately rejects any ambiguous collision.
    let mut hash = 0x6c62272e07bb014262b821756295c58du128;
    for byte in value.bytes() {
        hash = (hash ^ u128::from(byte)).wrapping_mul(0x0000000001000000000000000000013bu128);
    }
    format!("{hash:032x}")
}
fn unescape_mount(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}
