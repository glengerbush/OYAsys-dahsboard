//! Opt-in filesystem bridge to the separately installed privileged updater.
//! No network, process launch, permanent worker, or periodic checks run here.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SETTINGS_LOCK_REASON: &str =
    "Software update/recovery in progress or unresolved; settings are locked until it completes.";
const MAX_FILE_BYTES: u64 = 16 * 1024;
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
#[derive(Serialize)]
pub struct SoftwareStatus {
    pub enabled: bool,
    pub current_version: &'static str,
    pub state: String,
    pub latest_version: Option<String>,
    pub message: String,
    pub checked_at: Option<u64>,
    pub request_id: Option<String>,
}
impl SoftwareStatus {
    fn base(enabled: bool, state: &str, message: &str) -> Self {
        Self {
            enabled,
            current_version: env!("CARGO_PKG_VERSION"),
            state: state.into(),
            latest_version: None,
            message: message.into(),
            checked_at: None,
            request_id: None,
        }
    }
    fn error(message: impl Into<String>) -> Self {
        let mut status = Self::base(true, "error", "");
        status.message = message.into();
        status
    }
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Action {
    Check,
    Install,
}
impl Action {
    fn state(self) -> &'static str {
        match self {
            Self::Check => "checking",
            Self::Install => "installing",
        }
    }
    fn timeout(self) -> u64 {
        match self {
            Self::Check => 120,
            Self::Install => 600,
        }
    }
    fn terminal(self, state: &str) -> bool {
        // The trusted helper may recover an interrupted installation before
        // processing either command, including an explicit release check.
        matches!(state, "error" | "updated" | "rolled_back")
            || self == Self::Check && matches!(state, "available" | "current")
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    job: Job,
    requested_at: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerStatus {
    state: String,
    #[serde(default)]
    latest_version: Option<String>,
    message: String,
    #[serde(default)]
    checked_at: Option<u64>,
    #[serde(default)]
    request_id: Option<String>,
}
pub struct UpdateError {
    pub code: u16,
    pub message: String,
}
impl UpdateError {
    fn new(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
pub struct Updater {
    root: Option<PathBuf>,
}
struct Inspection {
    status: SoftwareStatus,
    busy: bool,
    blocked: bool,
}
impl Updater {
    pub fn from_environment() -> Self {
        Self {
            root: std::env::var_os("OYASYS_UPDATER_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        }
    }
    pub fn status(&self) -> SoftwareStatus {
        self.inspect().status
    }
    /// Checked only on an attempted settings write, never on a timer. The
    /// privileged journal is intentionally unreadable here; existence alone
    /// fences writes until the helper can no longer restore the old database.
    pub fn settings_writes_allowed(&self) -> bool {
        self.settings_write_guard().unwrap_or(false)
    }
    fn settings_write_guard(&self) -> Result<bool, String> {
        let Some(root) = &self.root else {
            return Ok(true);
        };
        match fs::symlink_metadata(root.join("transaction.json")) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        if !root.is_dir() {
            return Err("Updater directory cannot be inspected".into());
        }
        let receipt: Option<Receipt> = read_json(&root.join("inbox/pending.json"))?;
        let queued: Option<Job> = read_json(&root.join("inbox/request.json"))?;
        if let Some(job) = &queued {
            validate_job(job)?;
            if job.action == Action::Install {
                return Ok(false);
            }
        }
        let worker: Option<WorkerStatus> = read_json(&root.join("status.json"))?;
        if let Some(worker) = &worker {
            validate_worker(worker)?;
            if worker.state == "installing" {
                return Ok(false);
            }
        }
        if let Some(receipt) = receipt {
            validate_job(&receipt.job)?;
            if receipt.job.action == Action::Install {
                // A completed helper may leave the HTTP receipt until the next
                // status read. Do not mutate it from this read-only guard.
                return Ok(queued.is_none()
                    && worker.as_ref().is_some_and(|status| {
                        status.request_id.as_deref() == Some(&receipt.job.id)
                            && Action::Install.terminal(&status.state)
                    }));
            }
        }
        Ok(true)
    }
    fn inspect(&self) -> Inspection {
        match self.inspect_inner(){Ok(value)=>value,Err(error)=>Inspection{status:SoftwareStatus::error(format!("Updater state unavailable: {error}. Inspect the updater service before retrying.")),busy:false,blocked:true}}
    }
    fn inspect_inner(&self) -> Result<Inspection, String> {
        let Some(root) = &self.root else {
            return Ok(Inspection {
                status: SoftwareStatus::base(
                    false,
                    "disabled",
                    "Software updater is not installed or enabled",
                ),
                busy: false,
                blocked: false,
            });
        };
        if !root.is_dir() || !root.join("inbox").is_dir() {
            return Err("installation directory is missing".into());
        }
        let worker: Option<WorkerStatus> = read_json(&root.join("status.json"))?;
        if let Some(worker) = &worker {
            validate_worker(worker)?;
        }
        let receipt: Option<Receipt> = read_json(&root.join("inbox/pending.json"))?;
        let queued: Option<Job> = read_json(&root.join("inbox/request.json"))?;
        if let Some(receipt) = &receipt {
            validate_job(&receipt.job)?;
            if receipt.requested_at > now().saturating_add(60) {
                return Err("pending request has an invalid timestamp".into());
            }
        }
        if let Some(queued) = &queued {
            validate_job(queued)?;
        }
        if let (Some(receipt), Some(queued)) = (&receipt, &queued) {
            if receipt.job.id != queued.id {
                return Err("pending and queued request identities disagree".into());
            }
        }
        let mut status = match worker.as_ref() {
            Some(worker) => SoftwareStatus {
                enabled: true,
                current_version: env!("CARGO_PKG_VERSION"),
                state: worker.state.clone(),
                latest_version: worker.latest_version.clone(),
                message: worker.message.clone(),
                checked_at: worker.checked_at,
                request_id: worker.request_id.clone(),
            },
            None => SoftwareStatus::base(true, "idle", "Check for a newer release when needed"),
        };
        let pending = receipt
            .as_ref()
            .map(|r| (&r.job, r.requested_at))
            .or_else(|| {
                queued
                    .as_ref()
                    .map(|job| (job, file_time(&root.join("inbox/request.json"))))
            });
        if let Some((job, requested_at)) = pending {
            if status.request_id.as_deref() == Some(&job.id)
                && job.action.terminal(&status.state)
                && queued.is_none()
            {
                if receipt.is_some() {
                    fs::remove_file(root.join("inbox/pending.json")).map_err(|e| e.to_string())?;
                }
                return Ok(Inspection {
                    status,
                    busy: false,
                    blocked: false,
                });
            }
            if now().saturating_sub(requested_at) > job.action.timeout() {
                let mut error=SoftwareStatus::error("The updater request timed out. Inspect the updater service before retrying; installation will not be retried automatically.");
                error.request_id = Some(job.id.clone());
                error.latest_version = job.version.clone();
                return Ok(Inspection {
                    status: error,
                    busy: true,
                    blocked: true,
                });
            }
            status.state = job.action.state().into();
            status.request_id = Some(job.id.clone());
            if job.version.is_some() {
                status.latest_version = job.version.clone();
            }
            status.message = if job.action == Action::Check {
                "Release check queued or running"
            } else {
                "Installation queued or running; the dashboard may disconnect briefly"
            }
            .into();
            return Ok(Inspection {
                status,
                busy: true,
                blocked: false,
            });
        }
        if matches!(status.state.as_str(), "checking" | "installing") {
            let timeout = if status.state == "checking" { 120 } else { 600 };
            if now().saturating_sub(file_time(&root.join("status.json"))) > timeout {
                status.state = "error".into();
                status.message="The updater stopped reporting progress. Inspect the updater service before retrying.".into();
                return Ok(Inspection {
                    status,
                    busy: true,
                    blocked: true,
                });
            }
            return Ok(Inspection {
                status,
                busy: true,
                blocked: false,
            });
        }
        Ok(Inspection {
            status,
            busy: false,
            blocked: false,
        })
    }
    pub fn check(&self) -> Result<SoftwareStatus, UpdateError> {
        self.queue(Action::Check, None)
    }
    pub fn install(&self, version: String) -> Result<SoftwareStatus, UpdateError> {
        self.queue(Action::Install, Some(version))
    }
    fn queue(
        &self,
        action: Action,
        version: Option<String>,
    ) -> Result<SoftwareStatus, UpdateError> {
        let Some(root) = &self.root else {
            return Err(UpdateError::new(
                503,
                "Software updater is not installed or enabled",
            ));
        };
        let current = self.inspect();
        if current.busy {
            return Err(UpdateError::new(409, current.status.message));
        }
        if current.blocked {
            return Err(UpdateError::new(503, current.status.message));
        }
        if action == Action::Install {
            let target = version
                .as_deref()
                .ok_or_else(|| UpdateError::new(400, "A release version is required"))?;
            let parsed = stable_version(target)
                .ok_or_else(|| UpdateError::new(400, "Version must be stable X.Y.Z"))?;
            let installed = stable_version(env!("CARGO_PKG_VERSION"))
                .ok_or_else(|| UpdateError::new(503, "Installed version cannot be compared"))?;
            if current.status.state != "available"
                || current.status.latest_version.as_deref() != Some(target)
                || parsed <= installed
            {
                return Err(UpdateError::new(409,"Check for an available release newer than the installed version before installing"));
            }
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let job = Job {
            action,
            version,
            id: format!(
                "{nanos}-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ),
        };
        let receipt = Receipt {
            job,
            requested_at: now(),
        };
        // Receipt is the durable compare-and-create lock. Keep it after any ambiguous
        // queue failure so a caller cannot accidentally request a second install.
        publish_new(&root.join("inbox/pending.json"), &receipt, &receipt.job.id).map_err(|e| {
            UpdateError::new(
                if e.kind() == io::ErrorKind::AlreadyExists {
                    409
                } else {
                    503
                },
                format!("Cannot reserve updater request: {e}"),
            )
        })?;
        publish_new(&root.join("inbox/request.json"),&receipt.job,&receipt.job.id).map_err(|e|UpdateError::new(503,format!("Updater request could not be queued: {e}. Inspect updater state before retrying.")))?;
        Ok(self.status())
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn file_time(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("state path must be a regular file".into());
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("state file exceeds 16 KiB".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("state file exceeds 16 KiB".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("malformed state: {e}"))
}
fn stable_version(version: &str) -> Option<[u64; 3]> {
    if version.len() > 64 {
        return None;
    }
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut result = [0; 3];
    for (i, part) in parts.into_iter().enumerate() {
        if part.is_empty()
            || !part.bytes().all(|c| c.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        result[i] = part.parse().ok()?;
    }
    Some(result)
}
fn validate_job(job: &Job) -> Result<(), String> {
    if job.id.is_empty()
        || job.id.len() > 128
        || !job
            .id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        return Err("invalid request identity".into());
    }
    match job.action {
        Action::Check if job.version.is_some() => {
            Err("check request cannot select a version".into())
        }
        Action::Install if job.version.as_deref().and_then(stable_version).is_none() => {
            Err("invalid installation version".into())
        }
        _ => Ok(()),
    }
}
fn validate_worker(status: &WorkerStatus) -> Result<(), String> {
    if !matches!(
        status.state.as_str(),
        "idle"
            | "checking"
            | "available"
            | "current"
            | "installing"
            | "updated"
            | "rolled_back"
            | "error"
    ) || status.message.len() > 2048
        || status.request_id.as_ref().is_some_and(|id| id.len() > 128)
        || status
            .latest_version
            .as_deref()
            .is_some_and(|v| stable_version(v).is_none())
        || status.state == "available" && status.latest_version.is_none()
    {
        return Err("invalid worker status".into());
    }
    Ok(())
}
fn publish_new(path: &Path, value: &impl Serialize, id: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing updater inbox"))?;
    let temporary = parent.join(format!(
        ".{}-{id}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::hard_link(&temporary, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    let _ = fs::remove_file(temporary);
    result
}
