//! Optional read-only NVML binding. ABI definitions are the stable v1 structures
//! from NVIDIA's nvml.h; no CUDA toolkit or driver is linked into the executable.
use crate::model::{Availability, Metric};
use libloading::Library;
use std::{
    collections::BTreeMap,
    ffi::{c_char, c_uint, c_void},
};

type Status = c_uint;
type Handle = *mut c_void;
type Init = unsafe extern "C" fn() -> Status;
type Count = unsafe extern "C" fn(*mut c_uint) -> Status;
type ByIndex = unsafe extern "C" fn(c_uint, *mut Handle) -> Status;
type Text = unsafe extern "C" fn(Handle, *mut c_char, c_uint) -> Status;
type Scalar = unsafe extern "C" fn(Handle, *mut c_uint) -> Status;
type SelectedScalar = unsafe extern "C" fn(Handle, c_uint, *mut c_uint) -> Status;
type MemoryFn = unsafe extern "C" fn(Handle, *mut Memory) -> Status;
type UtilizationFn = unsafe extern "C" fn(Handle, *mut Utilization) -> Status;

#[repr(C)]
#[derive(Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}
#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

struct Device {
    // NVML handles are opaque tokens, never dereferenced by Rust. Storing the
    // address also permits moving the owner to the single collector worker.
    handle: usize,
    prefix: String,
    enabled: u8,
    statuses: [Option<Status>; 5],
}

pub(super) struct Nvml {
    devices: Vec<Device>,
    shutdown: Init,
    temperature: Option<SelectedScalar>,
    utilization: Option<UtilizationFn>,
    memory: Option<MemoryFn>,
    power: Option<Scalar>,
    clock: Option<SelectedScalar>,
    // Library outlives all function pointers and handles, including shutdown.
    _library: Library,
}

impl Nvml {
    pub(super) fn open() -> Result<(Self, Vec<Metric>), String> {
        // SAFETY: only the installed NVML library is loaded, and symbol types
        // match the published C ABI. All symbols remain owned by this object.
        unsafe {
            let library = Library::new("libnvidia-ml.so.1")
                .map_err(|_| "NVIDIA NVML unavailable; GPU metrics unavailable".to_owned())?;
            let init: Init = *library.get(b"nvmlInit_v2\0").map_err(|e| e.to_string())?;
            let shutdown: Init = *library.get(b"nvmlShutdown\0").map_err(|e| e.to_string())?;
            let count: Count = *library
                .get(b"nvmlDeviceGetCount_v2\0")
                .map_err(|e| e.to_string())?;
            let by_index: ByIndex = *library
                .get(b"nvmlDeviceGetHandleByIndex_v2\0")
                .map_err(|e| e.to_string())?;
            let name: Text = *library
                .get(b"nvmlDeviceGetName\0")
                .map_err(|e| e.to_string())?;
            let uuid: Text = *library
                .get(b"nvmlDeviceGetUUID\0")
                .map_err(|e| e.to_string())?;
            let temperature = library
                .get::<SelectedScalar>(b"nvmlDeviceGetTemperature\0")
                .ok()
                .map(|f| *f);
            let utilization = library
                .get::<UtilizationFn>(b"nvmlDeviceGetUtilizationRates\0")
                .ok()
                .map(|f| *f);
            let memory = library
                .get::<MemoryFn>(b"nvmlDeviceGetMemoryInfo\0")
                .ok()
                .map(|f| *f);
            let power = library
                .get::<Scalar>(b"nvmlDeviceGetPowerUsage\0")
                .ok()
                .map(|f| *f);
            let clock = library
                .get::<SelectedScalar>(b"nvmlDeviceGetClockInfo\0")
                .ok()
                .map(|f| *f);
            let status = init();
            if status != 0 {
                return Err(format!(
                    "NVIDIA NVML initialization failed ({status}); GPU metrics unavailable"
                ));
            }
            // From here onward the owner guarantees exactly one shutdown.
            let mut nvml = Self {
                devices: vec![],
                shutdown,
                temperature,
                utilization,
                memory,
                power,
                clock,
                _library: library,
            };
            let mut device_count = 0;
            let status = count(&mut device_count);
            if status != 0 {
                return Err(format!("NVIDIA GPU enumeration failed ({status})"));
            }
            let mut metrics = Vec::new();
            for index in 0..device_count.min(64) {
                let mut handle = std::ptr::null_mut();
                if by_index(index, &mut handle) != 0 || handle.is_null() {
                    continue;
                }
                let mut uuid_buffer = [0u8; 128];
                if uuid(
                    handle,
                    uuid_buffer.as_mut_ptr().cast(),
                    uuid_buffer.len() as c_uint,
                ) != 0
                {
                    continue;
                }
                let uuid = decode(&uuid_buffer);
                if uuid.is_empty() {
                    continue;
                }
                let mut name_buffer = [0u8; 128];
                let label = if name(
                    handle,
                    name_buffer.as_mut_ptr().cast(),
                    name_buffer.len() as c_uint,
                ) == 0
                {
                    decode(&name_buffer)
                } else {
                    "NVIDIA GPU".to_owned()
                };
                let prefix = format!("gpu.{uuid}");
                for (suffix, title, unit) in [
                    ("temperature", "temperature", "°C"),
                    ("utilization", "utilization", "%"),
                    ("memory_used", "VRAM used", "bytes"),
                    ("memory_total", "VRAM total", "bytes"),
                    ("power", "power", "W"),
                    ("clock", "graphics clock", "MHz"),
                ] {
                    metrics.push(Metric {
                        id: format!("{prefix}.{suffix}"),
                        label: format!("{label} {title}"),
                        unit: unit.into(),
                        group: "gpu".into(),
                        source: format!("{label} · {uuid} · NVML"),
                    });
                }
                nvml.devices.push(Device {
                    handle: handle as usize,
                    prefix,
                    enabled: 0xff,
                    statuses: [
                        temperature.map(|_| None).unwrap_or(Some(3)),
                        utilization.map(|_| None).unwrap_or(Some(3)),
                        memory.map(|_| None).unwrap_or(Some(3)),
                        power.map(|_| None).unwrap_or(Some(3)),
                        clock.map(|_| None).unwrap_or(Some(3)),
                    ],
                });
            }
            if nvml.devices.is_empty() {
                return Err("No readable NVIDIA devices; GPU metrics unavailable".into());
            }
            Ok((nvml, metrics))
        }
    }

    /// Returns true when initialization/driver/device loss warrants bounded retry.
    pub(super) fn sample(&mut self, output: &mut BTreeMap<String, Option<f64>>) -> bool {
        let mut lost = false;
        for device in &mut self.devices {
            let handle = device.handle as Handle;
            // SAFETY: pointers are writable, correctly sized C-layout outputs;
            // handles are returned by this live NVML instance, never dereferenced.
            unsafe {
                if let Some(f) = self.temperature.filter(|_| device.enabled & 1 != 0) {
                    let mut value = 0;
                    let code = f(handle, 0, &mut value); // NVML_TEMPERATURE_GPU
                    record(
                        output,
                        device,
                        "temperature",
                        code,
                        value as f64,
                        1,
                        &mut lost,
                    );
                }
                if let Some(f) = self.utilization.filter(|_| device.enabled & 2 != 0) {
                    let mut value = Utilization::default();
                    let code = f(handle, &mut value);
                    record(
                        output,
                        device,
                        "utilization",
                        code,
                        value.gpu as f64,
                        2,
                        &mut lost,
                    );
                }
                if let Some(f) = self.memory.filter(|_| device.enabled & 4 != 0) {
                    let mut value = Memory::default();
                    let code = f(handle, &mut value);
                    record(
                        output,
                        device,
                        "memory_used",
                        code,
                        value.used as f64,
                        4,
                        &mut lost,
                    );
                    record(
                        output,
                        device,
                        "memory_total",
                        code,
                        value.total as f64,
                        4,
                        &mut lost,
                    );
                }
                if let Some(f) = self.power.filter(|_| device.enabled & 8 != 0) {
                    let mut value = 0;
                    let code = f(handle, &mut value);
                    record(
                        output,
                        device,
                        "power",
                        code,
                        value as f64 / 1000.,
                        8,
                        &mut lost,
                    );
                }
                if let Some(f) = self.clock.filter(|_| device.enabled & 16 != 0) {
                    let mut value = 0;
                    let code = f(handle, 0, &mut value); // NVML_CLOCK_GRAPHICS
                    record(output, device, "clock", code, value as f64, 16, &mut lost);
                }
            }
        }
        lost
    }

    pub(super) fn availability(
        &self,
        output: &mut BTreeMap<String, Availability>,
        version: &mut u64,
    ) {
        for device in &self.devices {
            for (suffix, slot) in [
                ("temperature", 0),
                ("utilization", 1),
                ("memory_used", 2),
                ("memory_total", 2),
                ("power", 3),
                ("clock", 4),
            ] {
                let (status, reason) = match device.statuses[slot] {
                    None => ("warming_up", None),
                    Some(0) => ("available", None),
                    Some(3) => (
                        "unsupported",
                        Some("NVML query unsupported by this GPU or installed driver (3)"),
                    ),
                    Some(4) => (
                        "permission_denied",
                        Some("NVML query denied by driver permissions (4)"),
                    ),
                    Some(15) => ("lost", Some("NVIDIA GPU lost from bus (15)")),
                    Some(6) => ("lost", Some("NVIDIA source not found (6)")),
                    Some(1) => ("unavailable", Some("NVML uninitialized (1)")),
                    Some(9) => ("unavailable", Some("NVIDIA driver not loaded (9)")),
                    Some(2) => ("invalid", Some("NVML rejected query arguments (2)")),
                    Some(999) => ("unavailable", Some("NVML unknown driver error (999)")),
                    Some(code) => {
                        super::set_availability(
                            output,
                            version,
                            &format!("{}.{suffix}", device.prefix),
                            "unavailable",
                            Some(&format!("NVML query failed ({code})")),
                        );
                        continue;
                    }
                };
                super::set_availability(
                    output,
                    version,
                    &format!("{}.{suffix}", device.prefix),
                    status,
                    reason,
                );
            }
        }
    }
}

fn record(
    output: &mut BTreeMap<String, Option<f64>>,
    device: &mut Device,
    suffix: &str,
    status: Status,
    value: f64,
    mask: u8,
    lost: &mut bool,
) {
    device.statuses[mask.trailing_zeros() as usize] = Some(status);
    output.insert(
        format!("{}.{suffix}", device.prefix),
        (status == 0).then_some(value),
    );
    if matches!(status, 3 | 4) {
        device.enabled &= !mask;
    } // Unsupported/permission denied: retry on explicit refresh.
      // A failed optional query must not discard otherwise healthy GPU readings.
    if matches!(status, 1 | 9 | 15) {
        *lost = true;
    }
}

fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.split(|b| *b == 0).next().unwrap_or_default()).into_owned()
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: initialization succeeded and the owning library is still live.
        unsafe {
            (self.shutdown)();
        }
    }
}
