use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

struct Server {
    child: Child,
    dir: PathBuf,
    port: u16,
}
// Keep port selection and listener startup atomic between parallel test cases.
static STARTUP: Mutex<()> = Mutex::new(());
impl Server {
    fn new(samples: Value) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "oyasys-http-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("fixture.json"), json!({"inventory":{"hostname":"fixture", "system":[],"metrics":[{"id":"gpu.p100.temperature","label":"P100 temperature","unit":"°C","group":"gpu","source":"fixture P100"},{"id":"fan.fixture.rpm","label":"Chassis fan","unit":"RPM","group":"fan","source":"fixture fan"}],"warnings":[],"synthetic":true},"samples":samples,"now_ms":2_000_000_000_000i64}).to_string()).unwrap();
        fs::write(dir.join("clock"), "2000000000000").unwrap();
        Self::start(dir)
    }
    fn start(dir: PathBuf) -> Self {
        Self::start_config(dir, None)
    }
    fn start_config(dir: PathBuf, updater: Option<PathBuf>) -> Self {
        let _startup = STARTUP.lock().unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_oyasys"));
        if let Some(path) = updater {
            command.env("OYASYS_UPDATER_DIR", path);
        } else {
            command.env_remove("OYASYS_UPDATER_DIR");
        }
        let child = command
            .args([
                "serve",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "--origin",
                &format!("http://127.0.0.1:{port}"),
                "--data-dir",
                dir.join("data").to_str().unwrap(),
                "--fixture",
                dir.join("fixture.json").to_str().unwrap(),
                "--fixture-clock",
                dir.join("clock").to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut server = Self { child, dir, port };
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "server exited before listening"
            );
            assert!(Instant::now() < deadline, "server failed to listen");
            thread::sleep(Duration::from_millis(20));
        }
        server
    }
    fn updater() -> Self {
        let mut server = Self::new(json!([]));
        server.child.kill().unwrap();
        server.child.wait().unwrap();
        let dir = server.dir.clone();
        server.dir = PathBuf::new();
        drop(server);
        let updater = dir.join("updater");
        fs::create_dir_all(updater.join("inbox")).unwrap();
        Self::start_config(dir, Some(updater))
    }
    fn csrf_headers(&self) -> String {
        let state = self.get("/api/v1/state");
        format!("Origin: http://127.0.0.1:{}\r\nX-OYAsys-CSRF: {}\r\nContent-Type: application/json\r\n",self.port,state["csrf_token"].as_str().unwrap())
    }
    fn request(&self, method: &str, path: &str, headers: &str, body: &str) -> (u16, Value) {
        let mut socket = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(socket,"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",self.port,body.len()).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (
            status,
            serde_json::from_str(body).unwrap_or_else(|_| json!({"raw":body})),
        )
    }
    fn get(&self, path: &str) -> Value {
        let (status, body) = self.request("GET", path, "", "");
        assert_eq!(status, 200, "{body}");
        body
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn current_readings_distinguish_zero_missing_and_default_settings() {
    let server = Server::new(
        json!([{"timestamp_ms":2_000_000_000_000i64,"values":{"gpu.p100.temperature":0,"fan.fixture.rpm":null}}]),
    );
    let state = server.get("/api/v1/state");
    assert_eq!(state["snapshot"]["values"]["gpu.p100.temperature"], 0.0);
    assert!(state["snapshot"]["values"]["fan.fixture.rpm"].is_null());
    assert_eq!(state["settings"]["sampling_seconds"], 10);
    assert_eq!(state["settings"]["retention_days"], 30);
    assert_eq!(state["inventory"]["synthetic"], true);
    assert_eq!(state["storage"]["samples"], 1);
    let history=server.get("/api/v1/history?from=1999999990000&to=2000000000000&metrics=gpu.p100.temperature,fan.fixture.rpm&points=100");
    assert_eq!(
        history["series"][0]["points"][0],
        json!([2_000_000_000_000i64, 0.0])
    );
    assert_eq!(
        history["series"][1]["points"][0],
        json!([2_000_000_000_000i64, null])
    );
}

#[test]
fn settings_are_protected_atomic_durable_and_retention_cannot_resurrect_history() {
    let mut samples: Vec<Value>=(0..1500).map(|i|json!({"timestamp_ms":1_999_827_200_000i64+i*10_000,"values":{"gpu.p100.temperature":45,"fan.fixture.rpm":1200}})).collect();
    samples.push(json!({"timestamp_ms":2_000_000_000_000i64,"values":{"gpu.p100.temperature":55,"fan.fixture.rpm":1500}}));
    let mut server = Server::new(json!(samples));
    let state = server.get("/api/v1/state");
    let headers = format!(
        "Origin: http://127.0.0.1:{}\r\nX-OYAsys-CSRF: {}\r\nContent-Type: application/json\r\n",
        server.port,
        state["csrf_token"].as_str().unwrap()
    );
    let change = json!({"revision":0,"sampling_seconds":5,"retention_days":1,"p100_fan_id":"fan.fixture.rpm","confirm_delete":true});
    assert_eq!(
        server
            .request(
                "PUT",
                "/api/v1/settings",
                "Content-Type: application/json\r\n",
                &change.to_string()
            )
            .0,
        403
    );
    let mut invalid = change.clone();
    invalid["sampling_seconds"] = json!(0);
    assert_eq!(
        server
            .request("PUT", "/api/v1/settings", &headers, &invalid.to_string())
            .0,
        400
    );
    invalid = change.clone();
    invalid["confirm_delete"] = json!(false);
    assert_eq!(
        server
            .request("PUT", "/api/v1/settings", &headers, &invalid.to_string())
            .0,
        409
    );
    assert_eq!(
        server
            .request("PUT", "/api/v1/settings", &headers, &change.to_string())
            .0,
        200
    );
    assert_eq!(
        server
            .request("PUT", "/api/v1/settings", &headers, &change.to_string())
            .0,
        409
    );
    let path="/api/v1/history?from=1999740800000&to=2000000000000&metrics=gpu.p100.temperature&points=100";
    assert_eq!(
        server.get(path)["series"][0]["points"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut longer = change;
    longer["revision"] = json!(1);
    longer["retention_days"] = json!(30);
    assert_eq!(
        server
            .request("PUT", "/api/v1/settings", &headers, &longer.to_string())
            .0,
        200
    );
    assert_eq!(
        server.get(path)["series"][0]["points"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let dir = server.dir.clone();
    server.dir = PathBuf::new();
    drop(server);
    let restarted = Server::start(dir);
    let state = restarted.get("/api/v1/state");
    assert_eq!(state["settings"]["revision"], 2);
    assert_eq!(state["settings"]["sampling_seconds"], 5);
    assert_eq!(state["settings"]["p100_fan_id"], "fan.fixture.rpm");
}

#[test]
fn reduced_history_preserves_peak_missing_reading_and_recovery_timestamp() {
    let samples:Vec<_>=(0..100).map(|i|json!({"timestamp_ms":1_999_999_000_000i64+i*10_000,"values":{"gpu.p100.temperature":match i {20=>json!(99),55=>Value::Null,_=>json!(40)},"fan.fixture.rpm":1200}})).collect();
    let server = Server::new(json!(samples));
    let history=server.get("/api/v1/history?from=1999999000000&to=2000000000000&metrics=gpu.p100.temperature&points=20");
    let points = history["series"][0]["points"].as_array().unwrap();
    assert_eq!(history["reduced"], true);
    assert!(points.len() <= 20);
    assert!(points.contains(&json!([1_999_999_200_000i64, 99.0])));
    assert!(points.contains(&json!([1_999_999_550_000i64, null])));
    assert!(
        points.contains(&json!([1_999_999_560_000i64, 40.0])),
        "recovery must not be hidden by reduction: {points:?}"
    );
}

#[test]
fn collection_continues_without_clients_skips_missed_ticks_and_keeps_sequence_after_expiry() {
    let mut server = Server::new(
        json!([{"timestamp_ms":2_000_000_000_000i64,"values":{"gpu.p100.temperature":42,"fan.fixture.rpm":1000}}]),
    );
    // Deterministic scheduler advances by a minute, but collects only once.
    fs::write(server.dir.join("clock"), "2000000060000").unwrap();
    thread::sleep(Duration::from_millis(250));
    let updates = server.get("/api/v1/updates?after=1&limit=100");
    assert_eq!(updates["snapshots"].as_array().unwrap().len(), 1);
    assert_eq!(
        updates["snapshots"][0]["timestamp_ms"],
        2_000_000_060_000i64
    );
    assert_eq!(updates["next_after"], 2);
    for _ in 0..3 {
        assert_eq!(server.get("/api/v1/state")["snapshot"]["sequence"], 2);
    }
    // Time jumps beyond full-detail retention: old incremental cursor must reset.
    fs::write(server.dir.join("clock"), "2002678400000").unwrap();
    thread::sleep(Duration::from_millis(250));
    assert_eq!(
        server.get("/api/v1/updates?after=1&limit=100")["reset_required"],
        true
    );
    let state = server.get("/api/v1/state");
    let high = state["snapshot"]["sequence"].as_i64().unwrap();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let dir = server.dir.clone();
    server.dir = PathBuf::new();
    drop(server);
    let restarted = Server::start(dir);
    assert_eq!(restarted.get("/api/v1/state")["snapshot"]["sequence"], high);
    fs::write(restarted.dir.join("clock"), "2002678420000").unwrap();
    thread::sleep(Duration::from_millis(250));
    assert_eq!(
        restarted.get("/api/v1/state")["snapshot"]["sequence"],
        high + 1
    );
}

#[test]
fn request_limits_cross_origin_and_discovery_refresh_are_enforced() {
    let server = Server::new(json!([]));
    let state = server.get("/api/v1/state");
    assert_eq!(
        server
            .request(
                "GET",
                "/api/v1/state",
                "Origin: https://foreign.example\r\n",
                ""
            )
            .0,
        403
    );
    assert_eq!(server.request("POST", "/api/v1/refresh", "", "").0, 403);
    let headers = format!(
        "Origin: http://127.0.0.1:{}\r\nX-OYAsys-CSRF: {}\r\n",
        server.port,
        state["csrf_token"].as_str().unwrap()
    );
    assert_eq!(
        server.request("POST", "/api/v1/refresh", &headers, "").0,
        200
    );
    for path in ["/api/v1/history?from=0&to=2000000000000&metrics=gpu.p100.temperature&points=20","/api/v1/history?from=1999999990000&to=2000000000000&metrics=gpu.p100.temperature&points=2001","/api/v1/history?from=1999999990000&to=2000000000000&metrics=unknown&points=20","/api/v1/updates?after=0&limit=1001","/api/v1/updates?after=0&after=1"] {assert_eq!(server.request("GET",path,"","").0,400,"{path}");}
    let (status, root) = server.request("GET", "/", "", "");
    assert_eq!(status, 200);
    assert!(root["raw"].as_str().unwrap().contains("/assets/"));
}

#[test]
fn unavailable_storage_keeps_current_readings_honest_and_rejects_unsaved_settings() {
    let mut server = Server::new(
        json!([{"timestamp_ms":2_000_000_000_000i64,"values":{"gpu.p100.temperature":42,"fan.fixture.rpm":1000}}]),
    );
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let dir = server.dir.clone();
    server.dir = PathBuf::new();
    drop(server);
    fs::rename(dir.join("data"), dir.join("saved-data")).unwrap();
    fs::write(dir.join("data"), "blocked directory").unwrap();
    let blocked = Server::start(dir);
    let state = blocked.get("/api/v1/state");
    assert_eq!(state["storage"]["available"], false);
    assert_eq!(state["snapshot"]["persisted"], false);
    assert_eq!(state["snapshot"]["values"]["gpu.p100.temperature"], 42.0);
    assert_eq!(blocked.request("GET","/api/v1/history?from=1999999990000&to=2000000000000&metrics=gpu.p100.temperature&points=100","","").0,503);
    let headers = format!(
        "Origin: http://127.0.0.1:{}\r\nX-OYAsys-CSRF: {}\r\nContent-Type: application/json\r\n",
        blocked.port,
        state["csrf_token"].as_str().unwrap()
    );
    assert_eq!(
        blocked
            .request(
                "PUT",
                "/api/v1/settings",
                &headers,
                &json!({"revision":0,"sampling_seconds":1,"retention_days":30,"p100_fan_id":null})
                    .to_string()
            )
            .0,
        503
    );
    assert_eq!(
        blocked.get("/api/v1/state")["settings"]["sampling_seconds"],
        10
    );
}

#[test]
fn graceful_stop_preserves_committed_history() {
    let mut server = Server::new(
        json!([{"timestamp_ms":2_000_000_000_000i64,"values":{"gpu.p100.temperature":42,"fan.fixture.rpm":1000}}]),
    );
    unsafe {
        libc::kill(server.child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "graceful stop hung");
        thread::sleep(Duration::from_millis(20));
    }
    let dir = server.dir.clone();
    server.dir = PathBuf::new();
    drop(server);
    let restarted = Server::start(dir);
    assert_eq!(restarted.get("/api/v1/state")["storage"]["samples"], 1);
}

#[test]
fn assets_are_fingerprinted_precompressed_and_conditionally_cached() {
    let server = Server::new(json!([]));
    let (_, root) = server.request("GET", "/", "", "");
    let html = root["raw"].as_str().unwrap();
    let asset = html
        .split('"')
        .find(|part| part.starts_with("/assets/") && part.ends_with(".js"))
        .expect("fingerprinted JavaScript URL");
    let raw = |headers: &str| {
        let mut socket = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            socket,
            "GET {asset} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{headers}\r\n",
            server.port
        )
        .unwrap();
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).unwrap();
        let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        (
            String::from_utf8(bytes[..split].to_vec()).unwrap(),
            bytes[split + 4..].to_vec(),
        )
    };
    let (compressed, body) = raw("Accept-Encoding: gzip\r\n");
    let compressed = compressed.to_lowercase();
    assert!(compressed.starts_with("http/1.1 200"));
    assert!(compressed.contains("content-encoding: gzip"));
    assert!(compressed.contains("immutable"));
    assert!(body.starts_with(&[0x1f, 0x8b]));
    let etag = compressed
        .lines()
        .find_map(|line| line.strip_prefix("etag: "))
        .unwrap();
    let (cached, body) = raw(&format!(
        "Accept-Encoding: gzip\r\nIf-None-Match: {etag}\r\n"
    ));
    assert!(cached.starts_with("HTTP/1.1 304"));
    assert!(body.is_empty());
    let (identity, body) = raw("Accept-Encoding: gzip;q=0\r\n");
    assert!(!identity.to_lowercase().contains("content-encoding: gzip"));
    assert!(!body.starts_with(&[0x1f, 0x8b]));
}

#[test]
fn a_second_process_cannot_write_the_same_data_directory() {
    let server = Server::new(json!([]));
    let second_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut second = Command::new(env!("CARGO_BIN_EXE_oyasys"))
        .args([
            "serve",
            "--bind",
            &format!("127.0.0.1:{second_port}"),
            "--origin",
            &format!("http://127.0.0.1:{second_port}"),
            "--data-dir",
            server.dir.join("data").to_str().unwrap(),
            "--fixture",
            server.dir.join("fixture.json").to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = second.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            second.kill().unwrap();
            second.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.is_some_and(|status| !status.success()),
        "second writer must fail before serving"
    );
    let mut stderr = String::new();
    second
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("already uses this data directory"),
        "{stderr}"
    );
    assert_eq!(server.get("/api/v1/state")["storage"]["available"], true);
}

#[test]
fn wide_catalog_updates_have_a_total_value_budget_and_resumable_cursor() {
    let mut server = Server::new(json!([]));
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let dir = server.dir.clone();
    server.dir = PathBuf::new();
    drop(server);
    fs::remove_dir_all(dir.join("data")).unwrap();
    let metrics:Vec<_>=(0..2048).map(|i|json!({"id":format!("sensor.{i}"),"label":format!("Sensor {i}"),"unit":"°C","group":"thermal","source":"wide fixture"})).collect();
    let values: serde_json::Map<String, Value> = (0..2048)
        .map(|i| (format!("sensor.{i}"), json!(42)))
        .collect();
    let samples: Vec<_> = (0..6)
        .map(|i| json!({"timestamp_ms":1_999_999_950_000i64+i*10_000,"values":values}))
        .collect();
    fs::write(dir.join("fixture.json"),json!({"inventory":{"hostname":"wide","system":[],"metrics":metrics,"warnings":[],"synthetic":true},"samples":samples,"now_ms":2_000_000_000_000i64}).to_string()).unwrap();
    let server = Server::start(dir);
    let first = server.get("/api/v1/updates?after=0&limit=1000");
    let returned: usize = first["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|snapshot| snapshot["values"].as_object().unwrap().len())
        .sum();
    assert!(
        returned <= 8192,
        "wide catalogs must not multiply the update memory budget"
    );
    assert_eq!(first["next_after"], 4);
    let second = server.get("/api/v1/updates?after=4&limit=1000");
    assert_eq!(second["snapshots"].as_array().unwrap().len(), 2);
    assert_eq!(second["next_after"], 6);
    assert_eq!(second["state"]["snapshot"]["sequence"], 6);
}

#[test]
fn software_updater_is_disabled_without_installation_and_has_versioned_health() {
    let server = Server::new(json!([]));
    let status = server.get("/api/v1/software-update");
    assert_eq!(status["enabled"], false);
    assert_eq!(status["state"], "disabled");
    assert_eq!(status["current_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        server.get("/api/v1/health")["version"],
        env!("CARGO_PKG_VERSION")
    );
    let state = server.get("/api/v1/state");
    let headers = format!(
        "Origin: http://127.0.0.1:{}\r\nX-OYAsys-CSRF: {}\r\n",
        server.port,
        state["csrf_token"].as_str().unwrap()
    );
    assert_eq!(
        server
            .request("POST", "/api/v1/software-update/check", &headers, "")
            .0,
        503
    );
}

#[test]
fn software_update_checks_queue_once_and_follow_matching_worker_status() {
    let server = Server::updater();
    let headers = server.csrf_headers();
    assert_eq!(server.get("/api/v1/software-update")["state"], "idle");
    assert_eq!(
        server
            .request("POST", "/api/v1/software-update/check", "", "")
            .0,
        403
    );
    let (code, queued) = server.request("POST", "/api/v1/software-update/check", &headers, "");
    assert_eq!(code, 202);
    assert_eq!(queued["state"], "checking");
    let id = queued["request_id"].as_str().unwrap();
    let request: Value =
        serde_json::from_slice(&fs::read(server.dir.join("updater/inbox/request.json")).unwrap())
            .unwrap();
    assert_eq!(request["action"], "check");
    assert_eq!(request["id"], id);
    assert_eq!(
        server
            .request("POST", "/api/v1/software-update/check", &headers, "")
            .0,
        409
    );
    // Receipt still blocks a second command if the helper dequeued before status arrived.
    fs::remove_file(server.dir.join("updater/inbox/request.json")).unwrap();
    assert_eq!(server.get("/api/v1/software-update")["state"], "checking");
    assert_eq!(
        server
            .request("POST", "/api/v1/software-update/check", &headers, "")
            .0,
        409
    );
    fs::write(server.dir.join("updater/status.json"),json!({"state":"available","latest_version":"9.0.0","message":"Release available","checked_at":100,"request_id":id}).to_string()).unwrap();
    let ready = server.get("/api/v1/software-update");
    assert_eq!(ready["state"], "available");
    assert_eq!(ready["latest_version"], "9.0.0");
}

#[test]
fn software_install_requires_exact_newer_checked_version_and_bounded_json() {
    let server = Server::updater();
    let headers = server.csrf_headers();
    let status_path = server.dir.join("updater/status.json");
    fs::write(&status_path,json!({"state":"available","latest_version":"9.0.0","message":"Ready","checked_at":100,"request_id":"checked"}).to_string()).unwrap();
    for version in ["9.0.0;touch /tmp/no", "v9.0.0", "9.0.0-beta", "09.0.0"] {
        assert_eq!(
            server
                .request(
                    "POST",
                    "/api/v1/software-update/install",
                    &headers,
                    &json!({"version":version}).to_string()
                )
                .0,
            400
        );
    }
    assert_eq!(
        server
            .request(
                "POST",
                "/api/v1/software-update/install",
                &headers,
                &json!({"version":"8.0.0"}).to_string()
            )
            .0,
        409
    );
    assert_eq!(
        server
            .request(
                "POST",
                "/api/v1/software-update/install",
                &headers,
                &json!({"version":"9.0.0","url":"https://wrong.example"}).to_string()
            )
            .0,
        400
    );
    assert_eq!(
        server
            .request(
                "POST",
                "/api/v1/software-update/install",
                &headers,
                &"x".repeat(4097)
            )
            .0,
        413
    );
    let (code, status) = server.request(
        "POST",
        "/api/v1/software-update/install",
        &headers,
        "{\"version\":\"9.0.0\"}",
    );
    assert_eq!(code, 202);
    assert_eq!(status["state"], "installing");
    let queued: Value =
        serde_json::from_slice(&fs::read(server.dir.join("updater/inbox/request.json")).unwrap())
            .unwrap();
    assert_eq!(queued["action"], "install");
    assert_eq!(queued["version"], "9.0.0");
    fs::remove_file(server.dir.join("updater/inbox/request.json")).unwrap();
    fs::write(&status_path,json!({"state":"rolled_back","latest_version":"9.0.0","message":"Health check failed; previous version restored","checked_at":100,"request_id":queued["id"]}).to_string()).unwrap();
    assert_eq!(
        server.get("/api/v1/software-update")["state"],
        "rolled_back"
    );
    assert_eq!(
        server
            .request(
                "POST",
                "/api/v1/software-update/install",
                &headers,
                "{\"version\":\"9.0.0\"}"
            )
            .0,
        409
    );
}

#[test]
fn software_updater_rejects_malformed_oversized_and_nonregular_state_files() {
    use std::os::unix::fs::symlink;
    let server = Server::updater();
    let status_path = server.dir.join("updater/status.json");
    let headers = server.csrf_headers();
    for contents in [
        "{".to_string(),
        "x".repeat(16 * 1024 + 1),
        json!({"state":"available","latest_version":"https://invalid.example","message":"invalid"})
            .to_string(),
    ] {
        fs::write(&status_path, contents).unwrap();
        let status = server.get("/api/v1/software-update");
        assert_eq!(status["state"], "error");
        assert_eq!(
            server
                .request("POST", "/api/v1/software-update/check", &headers, "")
                .0,
            503
        );
    }
    fs::remove_file(&status_path).unwrap();
    let target = server.dir.join("private-status");
    fs::write(
        &target,
        json!({"state":"available","latest_version":"9.0.0","message":"Must not follow symlink"})
            .to_string(),
    )
    .unwrap();
    symlink(&target, &status_path).unwrap();
    assert_eq!(server.get("/api/v1/software-update")["state"], "error");
    fs::remove_file(&status_path).unwrap();
    let name = std::ffi::CString::new(status_path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let started = Instant::now();
    assert_eq!(server.get("/api/v1/software-update")["state"], "error");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn stale_software_jobs_remain_errors_without_ambiguous_install_retries() {
    let server = Server::updater();
    let headers = server.csrf_headers();
    let status_path = server.dir.join("updater/status.json");
    fs::write(&status_path,json!({"state":"available","latest_version":"9.0.0","message":"Ready","checked_at":100,"request_id":"previous"}).to_string()).unwrap();
    let (code, queued) = server.request(
        "POST",
        "/api/v1/software-update/install",
        &headers,
        "{\"version\":\"9.0.0\"}",
    );
    assert_eq!(code, 202);
    let pending_path = server.dir.join("updater/inbox/pending.json");
    let mut pending: Value = serde_json::from_slice(&fs::read(&pending_path).unwrap()).unwrap();
    pending["requested_at"] = json!(1);
    fs::write(&pending_path, pending.to_string()).unwrap();
    assert_eq!(server.get("/api/v1/software-update")["state"], "error");
    assert_eq!(
        server
            .request("POST", "/api/v1/software-update/check", &headers, "")
            .0,
        409
    );
    assert_eq!(
        server
            .request(
                "POST",
                "/api/v1/software-update/install",
                &headers,
                "{\"version\":\"9.0.0\"}"
            )
            .0,
        409
    );
    let request: Value =
        serde_json::from_slice(&fs::read(server.dir.join("updater/inbox/request.json")).unwrap())
            .unwrap();
    assert_eq!(request["id"], queued["request_id"]);
    fs::remove_file(server.dir.join("updater/inbox/request.json")).unwrap();
    fs::write(&status_path,json!({"state":"error","latest_version":"9.0.0","message":"Install failed; inspect service","checked_at":100,"request_id":queued["request_id"]}).to_string()).unwrap();
    assert_eq!(server.get("/api/v1/software-update")["state"], "error");
    assert_eq!(
        server
            .request(
                "POST",
                "/api/v1/software-update/install",
                &headers,
                "{\"version\":\"9.0.0\"}"
            )
            .0,
        409
    );
    assert_eq!(
        server
            .request("POST", "/api/v1/software-update/check", &headers, "")
            .0,
        202
    );
}

#[test]
fn concurrent_update_requests_share_one_durable_queue_across_processes() {
    let first = Server::updater();
    let mut second = Server::new(json!([]));
    second.child.kill().unwrap();
    second.child.wait().unwrap();
    let second_dir = second.dir.clone();
    second.dir = PathBuf::new();
    drop(second);
    let second = Server::start_config(second_dir, Some(first.dir.join("updater")));
    let first_headers = first.csrf_headers();
    let second_headers = second.csrf_headers();
    let barrier = std::sync::Barrier::new(2);
    let mut codes = thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            first
                .request("POST", "/api/v1/software-update/check", &first_headers, "")
                .0
        });
        let b = scope.spawn(|| {
            barrier.wait();
            second
                .request("POST", "/api/v1/software-update/check", &second_headers, "")
                .0
        });
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    codes.sort();
    assert_eq!(codes, vec![202, 409]);
    assert_eq!(
        first.get("/api/v1/software-update")["request_id"],
        second.get("/api/v1/software-update")["request_id"]
    );
}

#[test]
fn a_check_that_recovers_an_interrupted_update_clears_its_pending_receipt() {
    let server = Server::updater();
    let headers = server.csrf_headers();
    for terminal in ["updated", "rolled_back"] {
        let (code, queued) = server.request("POST", "/api/v1/software-update/check", &headers, "");
        assert_eq!(code, 202);
        fs::remove_file(server.dir.join("updater/inbox/request.json")).unwrap();
        fs::write(server.dir.join("updater/status.json"),json!({"state":terminal,"latest_version":"9.0.0","message":"Interrupted installation recovered","checked_at":100,"request_id":queued["request_id"]}).to_string()).unwrap();
        assert_eq!(server.get("/api/v1/software-update")["state"], terminal);
        assert!(!server.dir.join("updater/inbox/pending.json").exists());
    }
}

#[test]
fn settings_are_not_acknowledged_during_update_trial_or_unresolved_recovery() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::updater();
    let headers = server.csrf_headers();
    let updater = server.dir.join("updater");
    let save = |revision: u64, sampling: u64| {
        server.request("PUT","/api/v1/settings",&headers,&json!({"revision":revision,"sampling_seconds":sampling,"retention_days":30,"p100_fan_id":null,"confirm_delete":false}).to_string())
    };
    let (_, check) = server.request("POST", "/api/v1/software-update/check", &headers, "");
    assert_eq!(
        save(0, 11).0,
        200,
        "a release check alone must not lock settings"
    );
    fs::remove_file(updater.join("inbox/request.json")).unwrap();
    fs::write(updater.join("status.json"),json!({"state":"available","latest_version":"9.0.0","message":"Ready","checked_at":100,"request_id":check["request_id"]}).to_string()).unwrap();
    server.get("/api/v1/software-update");
    let (code, install) = server.request(
        "POST",
        "/api/v1/software-update/install",
        &headers,
        "{\"version\":\"9.0.0\"}",
    );
    assert_eq!(code, 202);
    let (code, error) = save(1, 12);
    assert_eq!(code, 409);
    assert!(error["error"].as_str().unwrap().contains("update/recovery"));
    let journal = updater.join("transaction.json");
    fs::write(&journal, "Root-only interrupted transaction journal").unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o000)).unwrap();
    fs::remove_file(updater.join("inbox/request.json")).unwrap();
    for terminal in ["updated", "error", "rolled_back"] {
        fs::write(updater.join("status.json"),json!({"state":terminal,"latest_version":"9.0.0","message":"Worker reached a terminal status","checked_at":100,"request_id":install["request_id"]}).to_string()).unwrap();
        server.get("/api/v1/software-update");
        assert_eq!(
            save(1, 12).0,
            409,
            "journal must override terminal status {terminal}"
        );
        let state = server.get("/api/v1/state");
        assert_eq!(state["settings"]["revision"], 1);
        assert_eq!(state["settings"]["sampling_seconds"], 11);
    }
    fs::remove_file(&journal).unwrap();
    assert_eq!(
        save(1, 12).0,
        200,
        "confirmed recovery completion must unlock settings"
    );
    fs::rename(&updater, server.dir.join("updater-saved")).unwrap();
    fs::write(&updater, "not a directory").unwrap();
    assert_eq!(
        save(2, 13).0,
        409,
        "journal inspection errors must fail closed"
    );
    assert_eq!(
        server.get("/api/v1/state")["settings"]["sampling_seconds"],
        12
    );
}
