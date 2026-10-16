//! SQLite history: versioned dense f64 gauges; missing values use canonical NaN.
use crate::model::{Inventory, Settings, Snapshot};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const DAY_MS: i64 = 86_400_000;
pub fn cutoff(now: i64, settings: &Settings) -> i64 {
    now.saturating_sub(settings.retention_days as i64 * DAY_MS)
}
#[derive(Clone, Serialize)]
pub struct StorageStatus {
    pub available: bool,
    pub error: Option<String>,
    pub bytes: u64,
    pub samples: u64,
    pub oldest_ms: Option<i64>,
    pub newest_ms: Option<i64>,
    pub estimated_daily_bytes: Option<u64>,
}
impl Default for StorageStatus {
    fn default() -> Self {
        Self {
            available: false,
            error: Some("History starting".into()),
            bytes: 0,
            samples: 0,
            oldest_ms: None,
            newest_ms: None,
            estimated_daily_bytes: None,
        }
    }
}
pub struct Writer {
    conn: Connection,
    path: PathBuf,
    catalog: i64,
    ids: Vec<String>,
    visible_count: Option<(i64, u64)>,
}
fn configure(conn: &Connection, writer: bool) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_millis(50))?;
    conn.execute_batch("PRAGMA cache_size=-128; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")?;
    if writer {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=2097152;")?;
    }
    Ok(())
}
impl Writer {
    pub fn open(path: &Path, inventory: &Inventory) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")
            .map_err(|e| e.to_string())?;
        configure(&conn, true).map_err(|e| e.to_string())?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if version > 1 {
            return Err(format!(
                "Unsupported history schema {version}; upgrade OYAsys"
            ));
        }
        conn.execute_batch("BEGIN; CREATE TABLE IF NOT EXISTS catalog(id INTEGER PRIMARY KEY, inventory TEXT NOT NULL UNIQUE); CREATE TABLE IF NOT EXISTS samples(sequence INTEGER PRIMARY KEY, timestamp_ms INTEGER NOT NULL, catalog INTEGER NOT NULL, cadence_ms INTEGER NOT NULL, values_blob BLOB NOT NULL); CREATE INDEX IF NOT EXISTS sample_time ON samples(timestamp_ms); CREATE TABLE IF NOT EXISTS settings(singleton INTEGER PRIMARY KEY CHECK(singleton=1), json TEXT NOT NULL); CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); PRAGMA user_version=1; COMMIT;").map_err(|e|e.to_string())?;
        let (catalog, ids) = register_catalog(&conn, inventory)?;
        Ok(Self {
            conn,
            path: path.to_owned(),
            catalog,
            ids,
            visible_count: None,
        })
    }
    pub fn settings(&self) -> Result<Settings, String> {
        let mut statement = self
            .conn
            .prepare("SELECT json FROM settings WHERE singleton=1")
            .map_err(|e| e.to_string())?;
        let mut rows = statement.query([]).map_err(|e| e.to_string())?;
        match rows.next().map_err(|e| e.to_string())? {
            Some(row) => serde_json::from_str(&row.get::<_, String>(0).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string()),
            None => Ok(Settings::default()),
        }
    }
    pub fn update_catalog(&mut self, inventory: &Inventory) -> Result<(), String> {
        let (catalog, ids) = register_catalog(&self.conn, inventory)?;
        self.catalog = catalog;
        self.ids = ids;
        Ok(())
    }
    pub fn save_settings(&mut self, settings: &Settings, now: i64) -> Result<(), String> {
        let old = self.settings()?;
        let cut = cutoff(now, &old).max(cutoff(now, settings));
        self.conn
            .execute_batch("PRAGMA synchronous=FULL;")
            .map_err(|e| e.to_string())?;
        let result = (|| {
            let tx = self.conn.transaction().map_err(|e| e.to_string())?;
            tx.execute("INSERT INTO settings VALUES(1,?) ON CONFLICT(singleton) DO UPDATE SET json=excluded.json",[serde_json::to_string(settings).map_err(|e|e.to_string())?]).map_err(|e|e.to_string())?;
            tx.execute("INSERT INTO metadata VALUES('expires_before',?) ON CONFLICT(key) DO UPDATE SET value=max(CAST(value AS INTEGER),CAST(excluded.value AS INTEGER))",[cut]).map_err(|e|e.to_string())?;
            tx.commit().map_err(|e| e.to_string())
        })();
        let _ = self.conn.execute_batch("PRAGMA synchronous=NORMAL;");
        result
    }
    pub fn high_sequence(&self) -> Result<i64, String> {
        self.conn.query_row("SELECT coalesce((SELECT CAST(value AS INTEGER) FROM metadata WHERE key='high_sequence'),(SELECT max(sequence) FROM samples),0)",[],|r|r.get(0)).map_err(|e|e.to_string())
    }
    pub fn latest(&self) -> Result<Option<Snapshot>, String> {
        let row:Option<(i64,i64,String,Vec<u8>)>=self.conn.query_row("SELECT s.sequence,s.timestamp_ms,c.inventory,s.values_blob FROM samples s JOIN catalog c ON c.id=s.catalog ORDER BY sequence DESC LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|e|e.to_string())?;
        row.map(|(sequence, timestamp_ms, encoded, bytes)| {
            let inventory: Inventory = serde_json::from_str(&encoded).map_err(|e| e.to_string())?;
            validate_blob(&bytes, inventory.metrics.len()).map_err(|e| e.message)?;
            Ok(Snapshot {
                sequence,
                timestamp_ms,
                values: inventory
                    .metrics
                    .into_iter()
                    .enumerate()
                    .map(|(i, m)| (m.id, value_at(&bytes, i)))
                    .collect(),
                persisted: true,
            })
        })
        .transpose()
    }
    pub fn fixture_imported(&self) -> bool {
        self.conn
            .query_row(
                "SELECT value FROM metadata WHERE key='fixture_imported'",
                [],
                |r| r.get::<_, String>(0),
            )
            .is_ok()
    }
    pub fn mark_fixture_imported(&self) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO metadata VALUES('fixture_imported','1')",
                [],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn append(&mut self, snapshot: &Snapshot, cadence_ms: u64) -> Result<(), String> {
        let mut bytes = Vec::with_capacity(1 + self.ids.len() * 8);
        bytes.push(1);
        for id in &self.ids {
            let v = snapshot
                .values
                .get(id)
                .copied()
                .flatten()
                .filter(|v| v.is_finite())
                .unwrap_or(f64::NAN);
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        tx.prepare_cached("INSERT INTO samples(sequence,timestamp_ms,catalog,cadence_ms,values_blob) VALUES(?,?,?,?,?)").map_err(|e|e.to_string())?.execute(params![snapshot.sequence,snapshot.timestamp_ms,self.catalog,cadence_ms as i64,bytes]).map_err(|e|e.to_string())?;
        tx.execute("INSERT INTO metadata VALUES('high_sequence',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[snapshot.sequence]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        if let Some((cut, count)) = &mut self.visible_count {
            if snapshot.timestamp_ms >= *cut {
                *count += 1;
            }
        }
        Ok(())
    }
    pub fn cleanup(&mut self, cut: i64) -> Result<(), String> {
        self.conn.execute("INSERT INTO metadata VALUES('expires_before',?) ON CONFLICT(key) DO UPDATE SET value=max(CAST(value AS INTEGER),CAST(excluded.value AS INTEGER))",[cut]).map_err(|e|e.to_string())?;
        let cut = effective_cut(&self.conn, cut).map_err(|e| e.to_string())?;
        self.conn.execute("DELETE FROM samples WHERE sequence IN (SELECT sequence FROM samples WHERE timestamp_ms < ? ORDER BY timestamp_ms LIMIT 1000)",[cut]).map_err(|e|e.to_string())?;
        self.conn
            .execute_batch("PRAGMA incremental_vacuum(32); PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn checkpoint(&self) {
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    pub fn status(&mut self, cut: i64) -> Result<StorageStatus, String> {
        let cut = effective_cut(&self.conn, cut).map_err(|e| e.to_string())?;
        // Cached count avoids scanning retained history during routine collection.
        let samples = match self.visible_count {
            Some((previous, count)) if cut >= previous => {
                let expired: i64 = self
                    .conn
                    .query_row(
                        "SELECT count(*) FROM samples WHERE timestamp_ms>=? AND timestamp_ms<?",
                        params![previous, cut],
                        |r| r.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                count.saturating_sub(expired as u64)
            }
            _ => self
                .conn
                .query_row(
                    "SELECT count(*) FROM samples WHERE timestamp_ms>=?",
                    [cut],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(|e| e.to_string())? as u64,
        };
        self.visible_count = Some((cut, samples));
        let coverage = coverage(&self.conn, cut).map_err(|e| e.to_string())?;
        let oldest = coverage.oldest_ms;
        let newest = coverage.newest_ms;
        let bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(format!("{}-wal", self.path.display()))
                .map(|m| m.len())
                .unwrap_or(0);
        let used_pages: i64 = self
            .conn
            .query_row(
                "SELECT page_count-freelist_count FROM pragma_page_count(),pragma_freelist_count()",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let page_size: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        // Observed occupied database pages; includes fixed schema/catalog overhead.
        let estimate = oldest.zip(newest).filter(|(a, b)| b > a).map(|(a, b)| {
            (used_pages as u64)
                .saturating_mul(page_size as u64)
                .saturating_mul(DAY_MS as u64)
                / (b - a) as u64
        });
        Ok(StorageStatus {
            available: true,
            error: None,
            bytes,
            samples: samples as u64,
            oldest_ms: oldest,
            newest_ms: newest,
            estimated_daily_bytes: estimate,
        })
    }
}

pub struct Reader {
    conn: Connection,
    catalogs: BTreeMap<i64, Vec<String>>,
}
#[derive(Debug)]
pub struct QueryError {
    pub status: u16,
    pub message: String,
}
impl From<rusqlite::Error> for QueryError {
    fn from(e: rusqlite::Error) -> Self {
        Self {
            status: 503,
            message: e.to_string(),
        }
    }
}
#[derive(Clone)]
pub struct HistoryQuery {
    pub from: i64,
    pub to: i64,
    pub ids: Vec<String>,
    pub points: usize,
}
#[derive(Serialize)]
pub struct Series {
    pub id: String,
    pub points: Vec<(i64, Option<f64>)>,
}
#[derive(Serialize)]
pub struct Coverage {
    pub oldest_ms: Option<i64>,
    pub newest_ms: Option<i64>,
}
#[derive(Serialize)]
pub struct HistoryResult {
    pub from_ms: i64,
    pub to_ms: i64,
    pub reduced: bool,
    pub series: Vec<Series>,
    pub coverage: Coverage,
    pub next_after: Option<i64>,
}
#[derive(Serialize)]
pub struct UpdatesResult {
    pub snapshots: Vec<Snapshot>,
    pub next_after: i64,
    pub reset_required: bool,
    pub settings_revision: u64,
}
impl Reader {
    pub fn open(path: &Path) -> Result<Self, QueryError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure(&conn, false)?;
        Ok(Self {
            conn,
            catalogs: BTreeMap::new(),
        })
    }
    fn coverage(&self, cut: i64) -> Result<Coverage, QueryError> {
        Ok(coverage(&self.conn, cut)?)
    }
    pub fn updates(
        &mut self,
        after: i64,
        limit: usize,
        cut: i64,
        revision: u64,
        cancel: impl Fn() -> bool,
    ) -> Result<UpdatesResult, QueryError> {
        let cut = effective_cut(&self.conn, cut)?;
        let high:i64=self.conn.query_row("SELECT coalesce((SELECT CAST(value AS INTEGER) FROM metadata WHERE key='high_sequence'),0)",[],|r|r.get(0))?;
        let cursor_timestamp: Option<i64> = self
            .conn
            .query_row(
                "SELECT timestamp_ms FROM samples WHERE sequence=?",
                [after],
                |r| r.get(0),
            )
            .optional()?;
        let reset = after != 0
            && (after > high || cursor_timestamp.is_none_or(|timestamp| timestamp < cut));
        let mut result = UpdatesResult {
            snapshots: Vec::new(),
            next_after: after,
            reset_required: reset,
            settings_revision: revision,
        };
        if reset {
            return Ok(result);
        }
        let mut stmt=self.conn.prepare("SELECT sequence,timestamp_ms,catalog,values_blob FROM samples WHERE sequence>? AND timestamp_ms>=? ORDER BY sequence LIMIT ?")?;
        let mut rows = stmt.query(params![after, cut, limit as i64])?;
        let mut total_values = 0usize;
        while let Some(row) = rows.next()? {
            if cancel() {
                return Err(QueryError {
                    status: 499,
                    message: "Request cancelled".into(),
                });
            }
            let seq = row.get(0)?;
            let catalog: i64 = row.get(2)?;
            let bytes: Vec<u8> = row.get(3)?;
            let ids = catalog_ids(&self.conn, &mut self.catalogs, catalog)?;
            validate_blob(&bytes, ids.len())?;
            if total_values + ids.len() > 8192 {
                break;
            }
            total_values += ids.len();
            let values = ids
                .iter()
                .enumerate()
                .map(|(i, id)| (id.clone(), value_at(&bytes, i)))
                .collect();
            result.snapshots.push(Snapshot {
                sequence: seq,
                timestamp_ms: row.get(1)?,
                values,
                persisted: true,
            });
            result.next_after = seq;
        }
        Ok(result)
    }
    pub fn history(
        &mut self,
        q: HistoryQuery,
        cut: i64,
        cancel: impl Fn() -> bool,
    ) -> Result<HistoryResult, QueryError> {
        let cut = effective_cut(&self.conn, cut)?;
        let from = q.from.max(cut);
        let coverage = self.coverage(cut)?;
        let count:i64=self.conn.query_row("SELECT count(*) FROM (SELECT 1 FROM samples WHERE timestamp_ms>=? AND timestamp_ms<=? LIMIT 3200001)",params![from,q.to],|r|r.get(0))?;
        if count > 3_200_000 {
            return Err(QueryError {
                status: 422,
                message: "Range exceeds 3,200,000 collections; select a narrower range".into(),
            });
        }
        let reduced = count as usize > q.points;
        let bucket_count = if reduced {
            (q.points / 5).max(1)
        } else {
            q.points
        };
        let mut reducers: Vec<Reducer> = q
            .ids
            .iter()
            .map(|_| Reducer::new(bucket_count, q.points, from, q.to, reduced))
            .collect();
        let mut positions: BTreeMap<i64, Vec<Option<usize>>> = BTreeMap::new();
        let started = Instant::now();
        let mut stmt=self.conn.prepare("SELECT timestamp_ms,catalog,cadence_ms,values_blob FROM samples WHERE timestamp_ms>=? AND timestamp_ms<=? ORDER BY timestamp_ms,sequence")?;
        let mut rows = stmt.query(params![from, q.to])?;
        let mut previous: Option<(i64, i64)> = None;
        let mut seen = 0;
        while let Some(row) = rows.next()? {
            seen += 1;
            if seen % 256 == 0 && (cancel() || started.elapsed() > Duration::from_secs(8)) {
                return Err(QueryError {
                    status: 422,
                    message: "History work limit reached; select a narrower range".into(),
                });
            }
            let timestamp: i64 = row.get(0)?;
            let catalog: i64 = row.get(1)?;
            let cadence: i64 = row.get(2)?;
            let bytes: Vec<u8> = row.get(3)?;
            let ids = catalog_ids(&self.conn, &mut self.catalogs, catalog)?;
            validate_blob(&bytes, ids.len())?;
            if let Some((prev, prev_cadence)) = previous {
                if timestamp - prev > prev_cadence.max(cadence).saturating_mul(3) / 2 {
                    for reducer in &mut reducers {
                        reducer.push(prev.saturating_add(1), None)?;
                    }
                }
            }
            if positions.len() >= 8 && !positions.contains_key(&catalog) {
                positions.clear();
            }
            let indices = positions.entry(catalog).or_insert_with(|| {
                q.ids
                    .iter()
                    .map(|wanted| ids.iter().position(|id| id == wanted))
                    .collect()
            });
            for (reducer, index) in reducers.iter_mut().zip(indices.iter()) {
                reducer.push(timestamp, index.and_then(|i| value_at(&bytes, i)))?;
            }
            previous = Some((timestamp, cadence));
        }
        let series = q
            .ids
            .into_iter()
            .zip(reducers)
            .map(|(id, r)| r.finish().map(|points| Series { id, points }))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(HistoryResult {
            from_ms: from,
            to_ms: q.to,
            reduced,
            series,
            coverage,
            next_after: None,
        })
    }
}
fn validate_blob(bytes: &[u8], n: usize) -> Result<(), QueryError> {
    if bytes.len() != 1 + n * 8 || bytes[0] != 1 {
        Err(QueryError {
            status: 503,
            message: "Unsupported or corrupt sample encoding".into(),
        })
    } else {
        Ok(())
    }
}
fn value_at(bytes: &[u8], index: usize) -> Option<f64> {
    let start = 1 + index * 8;
    let value = f64::from_le_bytes(bytes.get(start..start + 8)?.try_into().ok()?);
    value.is_finite().then_some(value)
}
#[derive(Default)]
struct Bucket {
    first: Option<(i64, Option<f64>)>,
    last: Option<(i64, Option<f64>)>,
    min: Option<(i64, Option<f64>)>,
    max: Option<(i64, Option<f64>)>,
    gaps: Vec<(i64, Option<f64>)>,
}
struct Reducer {
    buckets: Vec<Bucket>,
    raw: Vec<(i64, Option<f64>)>,
    limit: usize,
    from: i64,
    to: i64,
    reduced: bool,
    last_missing: bool,
    last_point: Option<(i64, Option<f64>)>,
    gap_count: usize,
}
impl Reducer {
    fn new(count: usize, limit: usize, from: i64, to: i64, reduced: bool) -> Self {
        Self {
            buckets: (0..count).map(|_| Bucket::default()).collect(),
            raw: Vec::new(),
            limit,
            from,
            to,
            reduced,
            last_missing: false,
            last_point: None,
            gap_count: 0,
        }
    }
    fn push(&mut self, t: i64, v: Option<f64>) -> Result<(), QueryError> {
        if !self.reduced {
            self.raw.push((t, v));
            if self.raw.len() > self.limit {
                return Err(QueryError {
                    status: 422,
                    message: "Too many gaps for point budget; increase points or narrow range"
                        .into(),
                });
            }
            return Ok(());
        }
        let count = self.buckets.len();
        let index = (((t - self.from) as i128 * count as i128)
            / ((self.to - self.from) as i128 + 1))
            .clamp(0, count as i128 - 1) as usize;
        let bucket = &mut self.buckets[index];
        bucket.first.get_or_insert((t, v));
        bucket.last = Some((t, v));
        if let Some(value) = v {
            if bucket
                .min
                .is_none_or(|(_, v)| value < v.unwrap_or(f64::INFINITY))
            {
                bucket.min = Some((t, v));
            }
            if bucket
                .max
                .is_none_or(|(_, v)| value > v.unwrap_or(f64::NEG_INFINITY))
            {
                bucket.max = Some((t, v));
            }
        }
        if v.is_none() != self.last_missing {
            if v.is_none() {
                if let Some(previous) = self.last_point {
                    bucket.gaps.push(previous);
                    self.gap_count += 1;
                }
            }
            bucket.gaps.push((t, v));
            self.gap_count += 1;
            if self.gap_count > self.limit {
                return Err(QueryError {
                    status: 422,
                    message: "Too many gaps for point budget; narrow range".into(),
                });
            }
        }
        self.last_missing = v.is_none();
        self.last_point = Some((t, v));
        Ok(())
    }
    fn finish(self) -> Result<Vec<(i64, Option<f64>)>, QueryError> {
        if !self.reduced {
            return Ok(self.raw);
        }
        let mut points = Vec::new();
        for b in self.buckets {
            points.extend([b.first, b.min, b.max, b.last].into_iter().flatten());
            points.extend(b.gaps);
        }
        points.sort_by_key(|p| p.0);
        points.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
        if points.len() > self.limit {
            return Err(QueryError {
                status: 422,
                message: "Too many gaps for point budget; narrow range".into(),
            });
        }
        Ok(points)
    }
}

fn effective_cut(conn: &Connection, cut: i64) -> rusqlite::Result<i64> {
    conn.query_row("SELECT max(?,coalesce((SELECT CAST(value AS INTEGER) FROM metadata WHERE key='expires_before'),0))",[cut],|r|r.get(0))
}
fn coverage(conn: &Connection, cut: i64) -> rusqlite::Result<Coverage> {
    conn.query_row("SELECT (SELECT timestamp_ms FROM samples WHERE timestamp_ms>=?1 ORDER BY timestamp_ms LIMIT 1),(SELECT timestamp_ms FROM samples WHERE timestamp_ms>=?1 ORDER BY timestamp_ms DESC LIMIT 1)",[cut],|r|Ok(Coverage{oldest_ms:r.get(0)?,newest_ms:r.get(1)?}))
}

fn catalog_ids<'a>(
    conn: &Connection,
    cache: &'a mut BTreeMap<i64, Vec<String>>,
    id: i64,
) -> Result<&'a Vec<String>, QueryError> {
    if !cache.contains_key(&id) {
        let encoded: String =
            conn.query_row("SELECT inventory FROM catalog WHERE id=?", [id], |r| {
                r.get(0)
            })?;
        let inventory: Inventory = serde_json::from_str(&encoded).map_err(|e| QueryError {
            status: 503,
            message: e.to_string(),
        })?;
        if inventory.metrics.len() > 4096 {
            return Err(QueryError {
                status: 503,
                message: "Metric catalog exceeds supported size".into(),
            });
        }
        if cache.len() >= 8 {
            cache.clear();
        }
        cache.insert(id, inventory.metrics.into_iter().map(|m| m.id).collect());
    }
    Ok(&cache[&id])
}

fn prune_catalogs(conn: &Connection, current: i64) -> rusqlite::Result<()> {
    let count: i64 = conn.query_row("SELECT count(*) FROM catalog", [], |r| r.get(0))?;
    if count >= 16 {
        conn.execute("DELETE FROM catalog WHERE id IN (SELECT id FROM catalog WHERE id<>? AND id<>(SELECT max(id) FROM catalog) AND id NOT IN (SELECT DISTINCT catalog FROM samples) LIMIT 16)",[current])?;
    }
    Ok(())
}

fn register_catalog(
    conn: &Connection,
    inventory: &Inventory,
) -> Result<(i64, Vec<String>), String> {
    let mut stable_inventory = inventory.clone();
    stable_inventory.warnings.clear();
    stable_inventory.availability.clear();
    let encoded = serde_json::to_string(&stable_inventory).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT OR IGNORE INTO catalog(inventory) VALUES (?)",
        [&encoded],
    )
    .map_err(|e| e.to_string())?;
    let catalog = conn
        .query_row(
            "SELECT id FROM catalog WHERE inventory=?",
            [&encoded],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    prune_catalogs(conn, catalog).map_err(|e| e.to_string())?;
    Ok((
        catalog,
        inventory.metrics.iter().map(|m| m.id.clone()).collect(),
    ))
}
