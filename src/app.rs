use crate::{
    history::{self, HistoryQuery, QueryError, Reader, StorageStatus, Writer},
    model::{Inventory, Settings, Snapshot},
    telemetry::Telemetry,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    net::SocketAddr,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{oneshot, Semaphore};

type Reply = Response<Full<Bytes>>;
#[derive(Clone)]
struct Clock {
    file: Option<PathBuf>,
    fixture_now: Option<i64>,
    started: Instant,
}
impl Clock {
    fn now(&self) -> i64 {
        self.file
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok()?.trim().parse().ok())
            .or_else(|| {
                self.fixture_now.map(|base| {
                    base.saturating_add(
                        self.started.elapsed().as_millis().min(i64::MAX as u128) as i64
                    )
                })
            })
            .unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(i64::MAX as u128) as i64
            })
    }
}
#[derive(Deserialize)]
struct Fixture {
    inventory: Inventory,
    samples: Vec<FixtureSample>,
    now_ms: Option<i64>,
}
#[derive(Deserialize)]
struct FixtureSample {
    timestamp_ms: i64,
    values: BTreeMap<String, Option<f64>>,
}
#[derive(Clone)]
struct Live {
    inventory: Inventory,
    snapshot: Option<Snapshot>,
    settings: Settings,
    storage: StorageStatus,
}
struct Shared {
    live: Mutex<Live>,
    clock: Clock,
    origin: String,
    authority: String,
    csrf: String,
    commands: mpsc::SyncSender<Command>,
    queries: mpsc::SyncSender<Query>,
    updater: crate::update::Updater,
    storage_scan_busy: Arc<AtomicBool>,
}
enum Command {
    Refresh(oneshot::Sender<Result<Inventory, String>>),
    Settings(Settings, oneshot::Sender<Result<Settings, SettingsError>>),
    Stop,
}
enum SettingsError {
    Stale,
    UpdateRecovery,
    Storage(String),
}
enum Query {
    History(
        HistoryQuery,
        i64,
        oneshot::Sender<Result<QueryResult, QueryError>>,
    ),
    Updates(
        i64,
        usize,
        i64,
        u64,
        oneshot::Sender<Result<QueryResult, QueryError>>,
    ),
}
enum QueryResult {
    History(history::HistoryResult),
    Updates(history::UpdatesResult),
}
#[derive(Serialize)]
struct UpdatesState<'a> {
    #[serde(flatten)]
    updates: &'a history::UpdatesResult,
    state: StatePayload<'a>,
}
#[derive(Serialize)]
struct StatePayload<'a> {
    inventory: &'a Inventory,
    snapshot: &'a Option<Snapshot>,
    settings: &'a Settings,
    csrf_token: &'a str,
    storage: &'a StorageStatus,
    now_ms: i64,
}
impl<'a> StatePayload<'a> {
    fn new(live: &'a Live, shared: &'a Shared) -> Self {
        Self {
            inventory: &live.inventory,
            snapshot: &live.snapshot,
            settings: &live.settings,
            csrf_token: &shared.csrf,
            storage: &live.storage,
            now_ms: shared.clock.now(),
        }
    }
}
struct CollectorInputs {
    writer: Option<Writer>,
    telemetry: Option<Telemetry>,
    fixture: Option<BTreeMap<String, Option<f64>>>,
    path: PathBuf,
    sequence: i64,
    data_lock: Option<fs::File>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsUpdate {
    revision: u64,
    sampling_seconds: u64,
    retention_days: u64,
    p100_fan_id: Option<String>,
    #[serde(default)]
    confirm_delete: bool,
}
struct Options {
    bind: SocketAddr,
    origin: String,
    data: PathBuf,
    fixture: Option<PathBuf>,
    clock: Option<PathBuf>,
    discover: bool,
}
fn options() -> Result<Options, String> {
    let mut result = Options {
        bind: "127.0.0.1:9850".parse().unwrap(),
        origin: "http://127.0.0.1:9850".into(),
        data: PathBuf::from("./data"),
        fixture: None,
        clock: None,
        discover: false,
    };
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("discover") => result.discover = true,
        Some("serve") | None => {}
        Some("--version") => {
            println!("oyasys {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0)
        }
        Some("--help") | Some("-h") => {
            println!("oyasys discover\noyasys serve [--bind IP:PORT] [--origin http[s]://HOST[:PORT]] [--data-dir DIR]\nFixture-only: --fixture JSON [--fixture-clock FILE]");
            std::process::exit(0)
        }
        Some(other) => return Err(format!("Unknown command {other}")),
    };
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("Missing value for {arg}"))?;
        match arg.as_str() {
            "--bind" => result.bind = value.parse().map_err(|_| "Invalid bind address")?,
            "--origin" => result.origin = value,
            "--data-dir" => result.data = value.into(),
            "--fixture" => result.fixture = Some(value.into()),
            "--fixture-clock" => result.clock = Some(value.into()),
            _ => return Err(format!("Unknown option {arg}")),
        }
    }
    if result.clock.is_some() && result.fixture.is_none() {
        return Err("--fixture-clock requires --fixture".into());
    }
    let uri: hyper::Uri = result.origin.parse().map_err(|_| "Invalid origin")?;
    if !matches!(uri.scheme_str(), Some("http") | Some("https"))
        || uri.authority().is_none()
        || uri
            .path_and_query()
            .is_some_and(|p| p.as_str() != "/" && p.as_str() != "")
        || result.origin.ends_with('/')
        || uri.authority().is_some_and(|a| a.as_str().contains('@'))
    {
        return Err(
            "Origin must be http[s]://HOST[:PORT], without a path or trailing slash".into(),
        );
    }
    Ok(result)
}
pub fn run() -> Result<(), String> {
    let options = options()?;
    if options.discover {
        let mut telemetry = Telemetry::new();
        let values = telemetry.sample();
        let mut report = serde_json::to_value(telemetry.inventory()).map_err(|e| e.to_string())?;
        report["readings"] = serde_json::to_value(values).map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let fixture = options
        .fixture
        .as_ref()
        .map(|path| {
            fs::read(path).map_err(|e| e.to_string()).and_then(|bytes| {
                serde_json::from_slice::<Fixture>(&bytes).map_err(|e| e.to_string())
            })
        })
        .transpose()?;
    let clock = Clock {
        file: options.clock,
        fixture_now: fixture.as_ref().and_then(|f| f.now_ms),
        started: Instant::now(),
    };
    let mut telemetry = if fixture.is_none() {
        Some(Telemetry::new())
    } else {
        None
    };
    let mut inventory = fixture
        .as_ref()
        .map(|f| f.inventory.clone())
        .unwrap_or_else(|| telemetry.as_ref().unwrap().inventory());
    inventory.synthetic = fixture.is_some();
    if inventory.metrics.len() > 4096 {
        return Err("Metric catalog exceeds 4096 entries".into());
    }
    let mut unique = BTreeSet::new();
    if inventory
        .metrics
        .iter()
        .any(|m| !unique.insert(m.id.clone()))
    {
        return Err("Metric catalog contains duplicate identities".into());
    }
    let db_path = options.data.join("history.sqlite3");
    let directory_result = fs::create_dir_all(&options.data).map_err(|e| e.to_string());
    let mut data_lock = None;
    let mut writer = match directory_result {
        Ok(()) => match acquire_data_lock(&options.data) {
            Ok(lock) => {
                data_lock = Some(lock);
                Writer::open(&db_path, &inventory)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err("Another OYAsys process already uses this data directory".into())
            }
            Err(error) => Err(error.to_string()),
        },
        Err(error) => Err(error),
    };
    let settings = writer
        .as_ref()
        .ok()
        .map(Writer::settings)
        .transpose()?
        .unwrap_or_default();
    if !(1..=3600).contains(&settings.sampling_seconds)
        || !(1..=365).contains(&settings.retention_days)
    {
        return Err("Invalid persisted settings".into());
    }
    let mut sequence = writer
        .as_ref()
        .ok()
        .and_then(|w| w.high_sequence().ok())
        .unwrap_or(0);
    let mut snapshot = None;
    if let Some(fixture) = &fixture {
        if let Ok(writer) = writer.as_mut() {
            if sequence == 0 && !writer.fixture_imported() {
                for sample in &fixture.samples {
                    sequence += 1;
                    let mut s = Snapshot {
                        sequence,
                        timestamp_ms: sample.timestamp_ms,
                        values: normalize_values(&inventory, &sample.values),
                        persisted: false,
                    };
                    writer.append(&s, settings.sampling_seconds * 1000)?;
                    s.persisted = true;
                    snapshot = Some(s);
                }
                writer.mark_fixture_imported()?;
            }
        }
        if snapshot.is_none() {
            snapshot = writer.as_ref().ok().and_then(|w| w.latest().ok().flatten());
        }
        if snapshot.is_none() && writer.is_err() {
            if let Some(sample) = fixture.samples.last() {
                snapshot = Some(Snapshot {
                    sequence,
                    timestamp_ms: sample.timestamp_ms,
                    values: normalize_values(&inventory, &sample.values),
                    persisted: false,
                });
            }
        }
    } else if let Some(telemetry) = telemetry.as_mut() {
        sequence += 1;
        let mut s = Snapshot {
            sequence,
            timestamp_ms: clock.now(),
            values: telemetry.sample(),
            persisted: false,
        };
        // A first write can fail after the database itself opened successfully.
        // Retain the actual error instead of allowing a readable database to mask it.
        inventory = telemetry.inventory();
        if let Ok(w) = writer.as_mut() {
            match w
                .update_catalog(&inventory)
                .and_then(|_| w.append(&s, settings.sampling_seconds * 1000))
            {
                Ok(()) => s.persisted = true,
                Err(error) => writer = Err(error),
            }
        }
        snapshot = Some(s);
    }
    let status = match &mut writer {
        Ok(w) => w
            .status(history::cutoff(clock.now(), &settings))
            .unwrap_or_else(storage_error),
        Err(e) => storage_error(e.clone()),
    };
    let (command_tx, command_rx) = mpsc::sync_channel(4);
    let (query_tx, query_rx) = mpsc::sync_channel(4);
    let mut random = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .map_err(|e| format!("Cannot initialize CSRF protection: {e}"))?;
    let authority = options
        .origin
        .parse::<hyper::Uri>()
        .unwrap()
        .authority()
        .unwrap()
        .as_str()
        .to_ascii_lowercase();
    let shared = Arc::new(Shared {
        live: Mutex::new(Live {
            inventory: inventory.clone(),
            snapshot,
            settings: settings.clone(),
            storage: status,
        }),
        clock: clock.clone(),
        origin: options.origin,
        authority,
        csrf: random.iter().map(|b| format!("{b:02x}")).collect(),
        commands: command_tx,
        queries: query_tx,
        updater: crate::update::Updater::from_environment(),
        storage_scan_busy: Arc::new(AtomicBool::new(false)),
    });
    let collector_state = shared.clone();
    let collector_path = db_path.clone();
    let collector = thread::Builder::new()
        .name("oyasys-collector".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            collect_loop(
                collector_state,
                command_rx,
                CollectorInputs {
                    writer: writer.ok(),
                    telemetry,
                    fixture: fixture.map(|f| {
                        f.samples
                            .last()
                            .map(|s| s.values.clone())
                            .unwrap_or_default()
                    }),
                    path: collector_path,
                    sequence,
                    data_lock,
                },
            )
        })
        .map_err(|e| e.to_string())?;
    let reader = thread::Builder::new()
        .name("oyasys-history".into())
        .stack_size(256 * 1024)
        .spawn(move || query_loop(query_rx, db_path))
        .map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .map_err(|e| e.to_string())?;
    let runtime_shared = shared.clone();
    let result = runtime.block_on(serve(options.bind, runtime_shared));
    let _ = shared.commands.send(Command::Stop);
    let _ = collector.join();
    drop(runtime);
    drop(shared);
    let _ = reader.join();
    result
}
fn storage_error(error: String) -> StorageStatus {
    StorageStatus {
        available: false,
        error: Some(error),
        ..StorageStatus::default()
    }
}
fn normalize_values(
    inventory: &Inventory,
    values: &BTreeMap<String, Option<f64>>,
) -> BTreeMap<String, Option<f64>> {
    inventory
        .metrics
        .iter()
        .map(|m| {
            (
                m.id.clone(),
                values
                    .get(&m.id)
                    .copied()
                    .flatten()
                    .filter(|v| v.is_finite()),
            )
        })
        .collect()
}
fn collect_loop(shared: Arc<Shared>, rx: mpsc::Receiver<Command>, inputs: CollectorInputs) {
    let CollectorInputs {
        mut writer,
        mut telemetry,
        fixture,
        path,
        mut sequence,
        mut data_lock,
    } = inputs;
    let mut settings = shared.live.lock().unwrap().settings.clone();
    let mut inventory = shared.live.lock().unwrap().inventory.clone();
    let mut next = Instant::now() + Duration::from_secs(settings.sampling_seconds);
    let mut next_maintenance = Instant::now();
    let mut retry = Instant::now() + Duration::from_secs(30);
    let mut last_fixture_time = shared.clock.now();
    let mut inventory_version = telemetry
        .as_ref()
        .map(Telemetry::inventory_version)
        .unwrap_or(0);
    loop {
        let wait = if fixture.is_some() && shared.clock.file.is_some() {
            Duration::from_millis(100)
        } else {
            next.min(next_maintenance)
                .saturating_duration_since(Instant::now())
        };
        match rx.recv_timeout(wait) {
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Command::Refresh(reply)) => {
                if let Some(t) = telemetry.as_mut() {
                    t.refresh();
                    inventory_version = t.inventory_version();
                    let updated = t.inventory();
                    let result = writer
                        .as_mut()
                        .map(|w| w.update_catalog(&updated))
                        .unwrap_or(Ok(()));
                    if let Err(e) = result {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                    inventory = updated;
                    shared.live.lock().unwrap().inventory = inventory.clone();
                }
                let _ = reply.send(Ok(inventory.clone()));
            }
            Ok(Command::Settings(proposed, reply)) => {
                if proposed.revision != settings.revision + 1 {
                    let _ = reply.send(Err(SettingsError::Stale));
                    continue;
                }
                // Recheck after dequeue: the helper may have begun a trial
                // installation while this settings request waited for the writer.
                if !shared.updater.settings_writes_allowed() {
                    let _ = reply.send(Err(SettingsError::UpdateRecovery));
                    continue;
                }
                let result = writer
                    .as_mut()
                    .ok_or_else(|| "History storage unavailable".to_string())
                    .and_then(|w| w.save_settings(&proposed, shared.clock.now()))
                    .map_err(SettingsError::Storage);
                match result {
                    Ok(()) => {
                        settings = proposed;
                        {
                            shared.live.lock().unwrap().settings = settings.clone();
                        }
                        next = Instant::now() + Duration::from_secs(settings.sampling_seconds);
                        next_maintenance = Instant::now();
                        let _ = reply.send(Ok(settings.clone()));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let now = shared.clock.now();
        if writer.is_none() && Instant::now() >= retry {
            let opening = if data_lock.is_none() {
                let directory = path.parent().unwrap_or(Path::new("."));
                fs::create_dir_all(directory)
                    .and_then(|_| acquire_data_lock(directory))
                    .map(|lock| data_lock = Some(lock))
                    .map_err(|e| e.to_string())
                    .and_then(|_| Writer::open(&path, &inventory))
            } else {
                Writer::open(&path, &inventory)
            };
            match opening {
                Ok(w) => {
                    sequence = sequence.max(w.high_sequence().unwrap_or(sequence));
                    if let Ok(saved) = w.settings() {
                        if saved.revision > settings.revision {
                            settings = saved;
                            shared.live.lock().unwrap().settings = settings.clone();
                        }
                    }
                    writer = Some(w);
                    next_maintenance = Instant::now();
                }
                Err(e) => shared.live.lock().unwrap().storage = storage_error(e),
            }
            retry = Instant::now() + Duration::from_secs(30);
        }
        let due = if fixture.is_some() {
            now >= last_fixture_time.saturating_add(settings.sampling_seconds as i64 * 1000)
        } else {
            Instant::now() >= next
        };
        let mut collected = None;
        let mut storage = None;
        if due {
            sequence = sequence.saturating_add(1);
            let values = telemetry
                .as_mut()
                .map(|t| t.sample())
                .unwrap_or_else(|| normalize_values(&inventory, fixture.as_ref().unwrap()));
            if let Some(t) = telemetry
                .as_ref()
                .filter(|t| t.inventory_version() != inventory_version)
            {
                inventory_version = t.inventory_version();
                let updated = t.inventory();
                let changed = updated.metrics.len() != inventory.metrics.len()
                    || updated
                        .metrics
                        .iter()
                        .zip(&inventory.metrics)
                        .any(|(a, b)| {
                            a.id != b.id
                                || a.label != b.label
                                || a.unit != b.unit
                                || a.source != b.source
                        });
                if changed {
                    if let Some(w) = writer.as_mut() {
                        if let Err(e) = w.update_catalog(&updated) {
                            shared.live.lock().unwrap().storage = storage_error(e);
                            writer = None;
                            retry = Instant::now() + Duration::from_secs(30);
                        }
                    }
                }
                if changed
                    || updated.warnings != inventory.warnings
                    || updated.availability != inventory.availability
                {
                    inventory = updated;
                    shared.live.lock().unwrap().inventory = inventory.clone();
                }
            }
            let mut snapshot = Snapshot {
                sequence,
                timestamp_ms: now,
                values,
                persisted: false,
            };
            if let Some(w) = writer.as_mut() {
                match w.append(&snapshot, settings.sampling_seconds * 1000) {
                    Ok(()) => snapshot.persisted = true,
                    Err(e) => {
                        shared.live.lock().unwrap().storage = storage_error(e);
                        writer = None;
                        retry = Instant::now() + Duration::from_secs(30);
                    }
                }
            }
            collected = Some(snapshot);
            next = Instant::now() + Duration::from_secs(settings.sampling_seconds);
            last_fixture_time = now;
        }
        if Instant::now() >= next_maintenance {
            if let Some(w) = writer.as_mut() {
                let cut = history::cutoff(now, &settings);
                let result = w
                    .status(cut)
                    .and_then(|_| w.cleanup(cut))
                    .and_then(|_| w.status(cut));
                storage = Some(match result {
                    Ok(status) => status,
                    Err(error) => storage_error(error),
                });
            }
            next_maintenance = Instant::now() + Duration::from_secs(30);
        }
        // Publish the completed collection and maintenance result together.
        // Otherwise recovery can expose a persisted sample with the previous
        // storage error while checkpoint I/O is still in progress. Keep that
        // I/O outside the lock so HTTP can continue serving the previous state.
        if collected.is_some() || storage.is_some() {
            let mut live = shared.live.lock().unwrap();
            if let Some(snapshot) = collected {
                live.snapshot = Some(snapshot);
            }
            if let Some(status) = storage {
                live.storage = status;
            }
        }
    }
    if let Some(w) = writer {
        w.checkpoint();
    }
}
fn query_loop(rx: mpsc::Receiver<Query>, path: PathBuf) {
    let mut reader = None;
    while let Ok(query) = rx.recv() {
        if reader.is_none() {
            reader = Reader::open(&path).ok();
        }
        match query {
            Query::History(q, cut, reply) => {
                if reply.is_closed() {
                    continue;
                }
                let result = match reader.as_mut() {
                    Some(r) => r
                        .history(q, cut, || reply.is_closed())
                        .map(QueryResult::History),
                    None => Err(QueryError {
                        status: 503,
                        message: "History storage unavailable".into(),
                    }),
                };
                if result.as_ref().is_err_and(|e| e.status == 503) {
                    reader = None;
                }
                let _ = reply.send(result);
            }
            Query::Updates(after, limit, cut, revision, reply) => {
                if reply.is_closed() {
                    continue;
                }
                let result = match reader.as_mut() {
                    Some(r) => r
                        .updates(after, limit, cut, revision, || reply.is_closed())
                        .map(QueryResult::Updates),
                    None => Err(QueryError {
                        status: 503,
                        message: "History storage unavailable".into(),
                    }),
                };
                if result.as_ref().is_err_and(|e| e.status == 503) {
                    reader = None;
                }
                let _ = reply.send(result);
            }
        }
    }
}
async fn serve(bind: SocketAddr, shared: Arc<Shared>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| e.to_string())?;
    eprintln!("OYAsys listening on {bind}");
    let slots = Arc::new(Semaphore::new(32));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    loop {
        tokio::select! { _=tokio::signal::ctrl_c()=>break,_=terminate.recv()=>break,result=listener.accept()=>{let (stream,_)=result.map_err(|e|e.to_string())?;let Ok(permit)=slots.clone().try_acquire_owned()else{drop(stream);continue;};let shared=shared.clone();tokio::spawn(async move {let _permit=permit;let service=service_fn(move|req|{let shared=shared.clone();async move{Ok::<_,std::convert::Infallible>(handle(req,shared).await)}});let mut builder=hyper::server::conn::http1::Builder::new();builder.max_headers(32).max_buf_size(80*1024);let connection=builder.serve_connection(TokioIo::new(stream),service);let _=tokio::time::timeout(Duration::from_secs(20),connection).await;});}}
    }
    Ok(())
}
async fn handle(mut req: Request<Incoming>, shared: Arc<Shared>) -> Reply {
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if host != shared.authority {
        return error(403, "Unrecognized Host");
    }
    if req
        .headers()
        .get("origin")
        .is_some_and(|v| v.to_str().ok() != Some(&shared.origin))
    {
        return error(403, "Origin is not allowed");
    }
    let method = req.method().clone();
    let path = req.uri().path();
    if method == hyper::Method::GET {
        match path {
            "/api/v1/software-update" => return response(200, &shared.updater.status()),
            "/api/v1/state" => {
                let live = shared.live.lock().unwrap();
                return response(200, &StatePayload::new(&live, &shared));
            }
            "/api/v1/health" => {
                let live = shared.live.lock().unwrap();
                return response(
                    200,
                    &json!({"version":env!("CARGO_PKG_VERSION"),"status":if live.storage.available{"ok"}else{"degraded"},"storage":live.storage,"last_collection_ms":live.snapshot.as_ref().map(|s|s.timestamp_ms),"now_ms":shared.clock.now()}),
                );
            }
            "/api/v1/history" | "/api/v1/updates" => {
                let params = match query_parameters(req.uri().query().unwrap_or("")) {
                    Ok(p) => p,
                    Err(e) => return error(400, &e),
                };
                let settings = shared.live.lock().unwrap().settings.clone();
                let cut = history::cutoff(shared.clock.now(), &settings);
                let (tx, rx) = oneshot::channel();
                let command = if path.ends_with("history") {
                    let parsed = parse_history(params, &shared.live.lock().unwrap().inventory);
                    match parsed {
                        Ok(q) => Query::History(q, cut, tx),
                        Err(e) => return error(400, &e),
                    }
                } else {
                    if params.keys().any(|k| k != "after" && k != "limit") {
                        return error(400, "Unknown updates parameter");
                    }
                    let after = match params
                        .get("after")
                        .and_then(|v| v.parse::<i64>().ok())
                        .filter(|v| *v >= 0)
                    {
                        Some(v) => v,
                        None => return error(400, "after must be a nonnegative sequence"),
                    };
                    let limit = match params.get("limit").map(|v| v.parse::<usize>()).transpose() {
                        Ok(v) => v.unwrap_or(100),
                        Err(_) => return error(400, "Invalid limit"),
                    };
                    if !(1..=1000).contains(&limit) {
                        return error(400, "limit must be 1..1000");
                    }
                    Query::Updates(after, limit, cut, settings.revision, tx)
                };
                if shared.queries.try_send(command).is_err() {
                    return error(429, "Historical worker busy; retry shortly");
                }
                return match tokio::time::timeout(Duration::from_secs(10), rx).await {
                    Ok(Ok(Ok(QueryResult::History(value)))) => {
                        let live = shared.live.lock().unwrap();
                        if live.settings.revision != settings.revision {
                            return error(
                                409,
                                "Settings changed during query; reload retained history",
                            );
                        }
                        response(200, &value)
                    }
                    Ok(Ok(Ok(QueryResult::Updates(value)))) => {
                        let live = shared.live.lock().unwrap();
                        if live.settings.revision != settings.revision {
                            return error(
                                409,
                                "Settings changed during query; reload retained history",
                            );
                        }
                        response(
                            200,
                            &UpdatesState {
                                updates: &value,
                                state: StatePayload::new(&live, &shared),
                            },
                        )
                    }
                    Ok(Ok(Err(e))) => error(e.status, &e.message),
                    _ => error(503, "History request timed out"),
                };
            }
            _ => return asset(path, &req),
        }
    }
    if method == hyper::Method::POST && path == "/api/v1/storage/analyze" {
        if req.headers().get("origin").and_then(|v| v.to_str().ok()) != Some(&shared.origin)
            || req
                .headers()
                .get("x-oyasys-csrf")
                .and_then(|v| v.to_str().ok())
                != Some(&shared.csrf)
        {
            return error(403, "Folder analysis requires same-origin CSRF protection");
        }
        if req
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            != Some("application/json")
        {
            return error(415, "Folder analysis requires application/json");
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct AnalyzeRequest {
            metric_id: String,
        }
        let body = match bounded_body(&mut req, 4096).await {
            Ok(body) => body,
            Err((code, message)) => return error(code, message),
        };
        let request: AnalyzeRequest = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return error(400, "Choose a discovered filesystem"),
        };
        let mount = {
            let live = shared.live.lock().unwrap();
            let metric = live
                .inventory
                .metrics
                .iter()
                .find(|m| {
                    m.id == request.metric_id
                        && m.group == "storage"
                        && m.id.starts_with("filesystem.")
                        && m.id.ends_with(".total")
                })
                .and_then(|m| m.label.strip_suffix(" capacity"))
                .map(PathBuf::from);
            if metric.is_some()
                && !live
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.values.get(&request.metric_id))
                    .and_then(|v| *v)
                    .is_some_and(|v| v.is_finite() && v > 0.0)
            {
                return error(409, "Filesystem is currently unavailable");
            }
            metric
        };
        let Some(mount) = mount else {
            return error(400, "Choose a discovered filesystem");
        };
        let Some(lease) = crate::storage_scan::Lease::acquire(&shared.storage_scan_busy) else {
            return error(409, "A folder analysis is already running");
        };
        let (tx, rx) = oneshot::channel();
        if let Err(error) = thread::Builder::new()
            .name("oyasys-storage-scan".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let result = crate::storage_scan::analyze(&mount, || tx.is_closed());
                drop(lease);
                let _ = tx.send(result);
            })
        {
            return self::error(503, &format!("Cannot start folder analysis: {error}"));
        }
        return match tokio::time::timeout(Duration::from_secs(12), rx).await {
            Ok(Ok(Ok(result))) => response(200, &result),
            Ok(Ok(Err(message))) => error(503, &message),
            _ => error(503, "Folder analysis timed out"),
        };
    }
    if method == hyper::Method::POST
        && matches!(
            path,
            "/api/v1/software-update/check" | "/api/v1/software-update/install"
        )
    {
        if req.headers().get("origin").and_then(|v| v.to_str().ok()) != Some(&shared.origin)
            || req
                .headers()
                .get("x-oyasys-csrf")
                .and_then(|v| v.to_str().ok())
                != Some(&shared.csrf)
        {
            return error(403, "Software updates require same-origin CSRF protection");
        }
        let install = path.ends_with("/install");
        if install
            && req
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(';').next())
                != Some("application/json")
        {
            return error(415, "Installation requires application/json");
        }
        let body = match bounded_body(&mut req, if install { 4096 } else { 0 }).await {
            Ok(body) => body,
            Err((code, message)) => return error(code, message),
        };
        let result = if install {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct InstallRequest {
                version: String,
            }
            let request: InstallRequest = match serde_json::from_slice(&body) {
                Ok(value) => value,
                Err(e) => return error(400, &e.to_string()),
            };
            shared.updater.install(request.version)
        } else {
            shared.updater.check()
        };
        return match result {
            Ok(status) => response(202, &status),
            Err(e) => error(e.code, &e.message),
        };
    }
    if method == hyper::Method::POST && path == "/api/v1/refresh" {
        if req.headers().get("origin").and_then(|v| v.to_str().ok()) != Some(&shared.origin)
            || req
                .headers()
                .get("x-oyasys-csrf")
                .and_then(|v| v.to_str().ok())
                != Some(&shared.csrf)
        {
            return error(403, "Refresh requires same-origin CSRF protection");
        }
        let (tx, rx) = oneshot::channel();
        if shared.commands.try_send(Command::Refresh(tx)).is_err() {
            return error(429, "Collector busy");
        }
        return match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(Ok(inventory))) => response(200, &inventory),
            Ok(Ok(Err(e))) => error(503, &e),
            _ => error(503, "Refresh timed out"),
        };
    }
    if method == hyper::Method::PUT && path == "/api/v1/settings" {
        if req.headers().get("origin").and_then(|v| v.to_str().ok()) != Some(&shared.origin)
            || req
                .headers()
                .get("x-oyasys-csrf")
                .and_then(|v| v.to_str().ok())
                != Some(&shared.csrf)
        {
            return error(403, "Settings require same-origin CSRF protection");
        }
        if req
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            != Some("application/json")
        {
            return error(415, "Settings require application/json");
        }
        let body = match bounded_body(&mut req, 4096).await {
            Ok(body) => body,
            Err((code, message)) => return error(code, message),
        };
        let update: SettingsUpdate = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(e) => return error(400, &e.to_string()),
        };
        {
            let live = shared.live.lock().unwrap();
            if update.revision != live.settings.revision {
                return error(409, "Settings changed; reload before saving");
            }
            if !(1..=3600).contains(&update.sampling_seconds)
                || !(1..=365).contains(&update.retention_days)
            {
                return error(
                    400,
                    "Sampling must be 1..3600 seconds; retention 1..365 days",
                );
            }
            if update.retention_days < live.settings.retention_days && !update.confirm_delete {
                return error(409, "Confirm deletion before reducing retention");
            }
            if update.p100_fan_id != live.settings.p100_fan_id
                && update.p100_fan_id.as_ref().is_some_and(|id| {
                    !live
                        .inventory
                        .metrics
                        .iter()
                        .any(|m| &m.id == id && m.unit == "RPM")
                })
            {
                return error(400, "Fan mapping must select a discovered RPM source");
            }
        }
        let settings = Settings {
            revision: match update.revision.checked_add(1) {
                Some(v) => v,
                None => return error(409, "Settings revision exhausted"),
            },
            sampling_seconds: update.sampling_seconds,
            retention_days: update.retention_days,
            p100_fan_id: update.p100_fan_id,
        };
        if !shared.updater.settings_writes_allowed() {
            return error(409, crate::update::SETTINGS_LOCK_REASON);
        }
        let (tx, rx) = oneshot::channel();
        if shared
            .commands
            .try_send(Command::Settings(settings, tx))
            .is_err()
        {
            return error(429, "Settings writer busy");
        }
        return match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(Ok(settings))) => response(200, &settings),
            Ok(Ok(Err(SettingsError::Stale))) => {
                error(409, "Settings changed; reload before saving")
            }
            Ok(Ok(Err(SettingsError::UpdateRecovery))) => {
                error(409, crate::update::SETTINGS_LOCK_REASON)
            }
            Ok(Ok(Err(SettingsError::Storage(e)))) => error(503, &e),
            _ => error(503, "Settings save timed out; reload before retrying"),
        };
    }
    error(404, "Not found")
}
fn parse_history(
    params: BTreeMap<String, String>,
    inventory: &Inventory,
) -> Result<HistoryQuery, String> {
    if params
        .keys()
        .any(|k| !["from", "to", "metrics", "points"].contains(&k.as_str()))
    {
        return Err("Unknown history parameter".into());
    }
    let from = params
        .get("from")
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or("Invalid from timestamp")?;
    let to = params
        .get("to")
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or("Invalid to timestamp")?;
    if from < 0 || to < from || to - from > 365 * history::DAY_MS {
        return Err("Range must be ordered, nonnegative, and at most 365 days".into());
    }
    let points = params
        .get("points")
        .and_then(|v| v.parse::<usize>().ok())
        .ok_or("Invalid points")?;
    if !(8..=2000).contains(&points) {
        return Err("points must be 8..2000".into());
    }
    let ids: Vec<_> = params
        .get("metrics")
        .ok_or("Missing metrics")?
        .split(',')
        .map(str::to_owned)
        .collect();
    let unique: BTreeSet<_> = ids.iter().collect();
    if ids.is_empty()
        || ids.len() > 64
        || unique.len() != ids.len()
        || ids
            .iter()
            .any(|id| !inventory.metrics.iter().any(|m| &m.id == id))
    {
        return Err("Select 1..64 unique discovered metric IDs".into());
    }
    Ok(HistoryQuery {
        from,
        to,
        ids,
        points,
    })
}
fn query_parameters(query: &str) -> Result<BTreeMap<String, String>, String> {
    if query.len() > 65536 {
        return Err("Query too long".into());
    }
    let mut result = BTreeMap::new();
    for field in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = field.split_once('=').ok_or("Invalid query parameter")?;
        let key = decode(key)?;
        if result.insert(key, decode(value)?).is_some() {
            return Err("Duplicate query parameter".into());
        }
    }
    Ok(result)
}
fn decode(input: &str) -> Result<String, String> {
    let mut output = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err("Invalid URL encoding".into());
                }
                let value = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3])
                        .map_err(|_| "Invalid URL encoding")?,
                    16,
                )
                .map_err(|_| "Invalid URL encoding")?;
                output.push(value);
                i += 3;
            }
            b'+' => {
                output.push(b' ');
                i += 1
            }
            other => {
                output.push(other);
                i += 1
            }
        }
    }
    String::from_utf8(output).map_err(|_| "Invalid URL encoding".into())
}
fn response(status: u16, value: &impl Serialize) -> Reply {
    let mut response = Response::new(Full::new(Bytes::from(
        serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
    )));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(
        "content-type",
        "application/json; charset=utf-8".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    security_headers(&mut response);
    response
}
fn error(status: u16, message: &str) -> Reply {
    response(status, &json!({"error":message}))
}
fn security_headers(response: &mut Reply) {
    for (name,value) in [("x-content-type-options","nosniff"),("referrer-policy","same-origin"),("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")]{response.headers_mut().insert(name,value.parse().unwrap());}
}
fn asset(path: &str, req: &Request<Incoming>) -> Reply {
    match crate::assets::serve(path, req) {
        Some(mut response) => {
            security_headers(&mut response);
            response
        }
        None => error(404, "Not found"),
    }
}

// The held file descriptor owns the advisory lock until the collector exits.
// Never unlink this file: that could let two processes lock different inodes.
fn acquire_data_lock(directory: &Path) -> std::io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("oyasys.lock"))?;
    // SAFETY: flock only uses the borrowed descriptor; file keeps it alive.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

async fn bounded_body(
    req: &mut Request<Incoming>,
    limit: usize,
) -> Result<Vec<u8>, (u16, &'static str)> {
    if req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > limit as u64)
    {
        return Err((413, "Request body exceeds endpoint limit"));
    }
    let read = async {
        let mut body = Vec::new();
        while let Some(frame) = req.body_mut().frame().await {
            let frame = frame.map_err(|_| (400, "Invalid request body"))?;
            if let Some(bytes) = frame.data_ref() {
                if body.len() + bytes.len() > limit {
                    return Err((413, "Request body exceeds endpoint limit"));
                }
                body.extend_from_slice(bytes);
            }
        }
        Ok(body)
    };
    match tokio::time::timeout(Duration::from_secs(5), read).await {
        Ok(result) => result,
        Err(_) => Err((408, "Body timeout")),
    }
}
