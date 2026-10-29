//! SQLite storage for persons, programs and rides. Synchronous; async callers
//! use `tokio::task::spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use thiserror::Error;

use crate::elevation::{ElevationParams, ElevationPreview, heights, preview};
use crate::program::{FlatSegment, Program, templates};
use crate::protocol::{DeviceLimits, Status};
use crate::session::Sample;

const SCHEMA_VERSION: i64 = 6;

/// Version 1. Never change it: later versions migrate from it.
const SCHEMA_V1: &str = "
CREATE TABLE persons (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);
CREATE TABLE programs (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    blocks TEXT NOT NULL
);
CREATE TABLE rides (
    id INTEGER PRIMARY KEY,
    person_id INTEGER NOT NULL REFERENCES persons(id),
    program_name TEXT,
    program_json TEXT,
    started_at INTEGER NOT NULL,
    finished INTEGER NOT NULL DEFAULT 0,
    ride_secs INTEGER NOT NULL DEFAULT 0,
    avg_watts INTEGER NOT NULL DEFAULT 0,
    max_watts INTEGER NOT NULL DEFAULT 0,
    avg_pulse INTEGER,
    max_pulse INTEGER,
    distance_m REAL NOT NULL DEFAULT 0,
    energy_kj REAL NOT NULL DEFAULT 0
);
CREATE INDEX rides_person ON rides(person_id, started_at);
CREATE TABLE samples (
    ride_id INTEGER NOT NULL REFERENCES rides(id),
    ride_secs INTEGER NOT NULL,
    pulse INTEGER NOT NULL,
    rpm INTEGER NOT NULL,
    speed_x10 INTEGER NOT NULL,
    distance INTEGER NOT NULL,
    status_target_watts INTEGER NOT NULL,
    energy INTEGER NOT NULL,
    elapsed_secs INTEGER NOT NULL,
    actual_watts INTEGER NOT NULL,
    target_watts INTEGER NOT NULL,
    UNIQUE (ride_id, ride_secs)
);
";

/// Version 2: ride end time (unix seconds) and the wall clock of each sample (unix ms).
const MIGRATE_V2: &str = "
ALTER TABLE rides ADD COLUMN ended_at INTEGER;
ALTER TABLE samples ADD COLUMN unix_ms INTEGER;
";

/// Version 3: cadence summary per ride. Existing rides are backfilled from their samples
/// in the same transaction: only samples with rpm > 0 count, the average is rounded half
/// up exactly like `summarize` (integer arithmetic), rides without pedalling stay NULL.
const MIGRATE_V3: &str = "
ALTER TABLE rides ADD COLUMN avg_rpm INTEGER;
ALTER TABLE rides ADD COLUMN max_rpm INTEGER;
UPDATE rides SET
    avg_rpm = (SELECT (SUM(rpm) + COUNT(*) / 2) / COUNT(*) FROM samples
               WHERE samples.ride_id = rides.id AND samples.rpm > 0),
    max_rpm = (SELECT MAX(rpm) FROM samples
               WHERE samples.ride_id = rides.id AND samples.rpm > 0);
";

/// Version 4: results of the cockpit's recovery measurement, at the ride time they
/// belong to (during the ride, or its end for a measurement right after it).
const MIGRATE_V4: &str = "
CREATE TABLE recoveries (
    ride_id INTEGER NOT NULL REFERENCES rides(id),
    ride_secs INTEGER NOT NULL,
    pulse_start INTEGER NOT NULL,
    difference INTEGER NOT NULL,
    grade_x10 INTEGER NOT NULL,
    unix_ms INTEGER NOT NULL
);
CREATE INDEX recoveries_ride ON recoveries(ride_id);
";

/// Version 5: rider weight and flat power per person, their snapshot per ride and the
/// ride's elevation gain. The gain of existing rides is backfilled in the migration step
/// (it needs `crate::elevation`, so it is not plain SQL).
const MIGRATE_V5: &str = "
ALTER TABLE persons ADD COLUMN weight_kg REAL NOT NULL DEFAULT 75;
ALTER TABLE persons ADD COLUMN flat_watts INTEGER NOT NULL DEFAULT 70;
ALTER TABLE rides ADD COLUMN weight_kg REAL NOT NULL DEFAULT 75;
ALTER TABLE rides ADD COLUMN flat_watts INTEGER NOT NULL DEFAULT 70;
ALTER TABLE rides ADD COLUMN elevation_gain_m REAL NOT NULL DEFAULT 0;
";

/// Version 6: chest straps, a strap per person, the strap's pulse per sample and the
/// source of each recovery measurement (existing ones came from the cockpit).
const MIGRATE_V6: &str = "
CREATE TABLE straps (
    id INTEGER PRIMARY KEY,
    address TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL
);
ALTER TABLE persons ADD COLUMN strap_id INTEGER REFERENCES straps(id);
ALTER TABLE samples ADD COLUMN strap_pulse INTEGER;
ALTER TABLE recoveries ADD COLUMN source TEXT NOT NULL DEFAULT 'cockpit';
";

const RIDE_COLUMNS: &str = "id, person_id, program_name, started_at, ride_secs, avg_watts, \
     max_watts, avg_pulse, max_pulse, distance_m, energy_kj, finished, ended_at, \
     avg_rpm, max_rpm, weight_kg, flat_watts, elevation_gain_m";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Person {
    pub id: i64,
    pub name: String,
    /// Without the bike (see `crate::elevation::BIKE_KG`).
    pub weight_kg: f64,
    /// Power that counts as flat ground.
    pub flat_watts: u16,
    pub strap_id: Option<i64>,
}

/// Changes to a person; `None` keeps the stored value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersonUpdate {
    pub name: Option<String>,
    pub weight_kg: Option<f64>,
    pub flat_watts: Option<u16>,
    /// `Some(None)` clears the assignment.
    pub strap_id: Option<Option<i64>>,
}

/// A rejected person value; the message is shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PersonError {
    #[error("Name darf nicht leer sein")]
    EmptyName,
    #[error("Gewicht muss zwischen 30 und 250 kg liegen")]
    Weight,
    #[error("Ebene muss zwischen 0 und 300 W liegen")]
    Flat,
}

/// A registered chest strap; the server connects to exactly these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Strap {
    pub id: i64,
    /// Bluetooth address, upper case.
    pub address: String,
    pub name: String,
}

/// A rejected strap value; the message is shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StrapError {
    #[error("Dieser Gurt ist schon angemeldet")]
    Duplicate,
    #[error("Name darf nicht leer sein")]
    EmptyName,
    #[error("Unbekannter Gurt")]
    Unknown,
}

const WEIGHT_KG: std::ops::RangeInclusive<f64> = 30.0..=250.0;
const MAX_FLAT_WATTS: u16 = 300;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RideSummary {
    pub id: i64,
    pub person_id: i64,
    pub program_name: Option<String>,
    pub started_at: i64,
    pub ride_secs: u32,
    pub avg_watts: u16,
    pub max_watts: u16,
    pub avg_pulse: Option<u16>,
    pub max_pulse: Option<u16>,
    pub distance_m: f64,
    pub energy_kj: f64,
    pub finished: bool,
    /// Unix seconds; `None` while running and for rides from schema version 1.
    pub ended_at: Option<i64>,
    /// Cadence over the samples with rpm > 0; `None` if there was no pedalling.
    pub avg_rpm: Option<u16>,
    pub max_rpm: Option<u16>,
    /// Snapshot of the person's profile at the start of the ride.
    pub weight_kg: f64,
    pub flat_watts: u16,
    pub elevation_gain_m: f64,
}

impl RideSummary {
    fn elevation_params(&self) -> ElevationParams {
        ElevationParams { weight_kg: self.weight_kg, flat_watts: self.flat_watts }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RideDetail {
    pub summary: RideSummary,
    pub program_segments: Option<Vec<FlatSegment>>,
    pub samples: Vec<Sample>,
    pub recoveries: Vec<Recovery>,
    /// Height in metres after each sample, aligned with `samples`.
    pub elevation_m: Vec<f32>,
    /// The program's elevation if ridden on target; `None` without a program.
    pub elevation_preview: Option<ElevationPreview>,
}

/// A recovery measurement stored with a ride (see `crate::recovery`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recovery {
    pub ride_secs: u32,
    /// P1; the recovery pulse P2 is `pulse_start - difference`.
    pub pulse_start: u16,
    pub difference: u16,
    /// Fitness grade × 10 (10 = F1,0 … 60 = F6,0).
    pub grade_x10: u16,
    pub at_unix_ms: i64,
    pub source: RecoverySource,
}

/// Who measured a recovery: the cockpit itself, or the server with a chest strap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoverySource {
    #[default]
    Cockpit,
    Strap,
}

impl RecoverySource {
    fn as_str(self) -> &'static str {
        match self {
            RecoverySource::Cockpit => "cockpit",
            RecoverySource::Strap => "strap",
        }
    }

    /// Unknown text counts as the cockpit, the column's default.
    fn parse(text: &str) -> Self {
        if text == "strap" { RecoverySource::Strap } else { RecoverySource::Cockpit }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RideStats {
    pub ride_secs: u32,
    pub avg_watts: u16,
    pub max_watts: u16,
    pub avg_pulse: Option<u16>,
    pub max_pulse: Option<u16>,
    pub avg_rpm: Option<u16>,
    pub max_rpm: Option<u16>,
    pub distance_m: f64,
    pub energy_kj: f64,
    pub elevation_gain_m: f64,
}

/// Actual watts in ride time order (the slice may be unordered).
fn watts_in_order(samples: &[Sample]) -> Vec<f64> {
    let mut timed: Vec<(u32, u16)> = samples.iter().map(|s| (s.ride_secs, s.status.actual_watts)).collect();
    timed.sort_by_key(|&(secs, _)| secs);
    timed.into_iter().map(|(_, w)| f64::from(w)).collect()
}

/// Aggregates samples (one per second). The pulse is the effective one (strap, else
/// cockpit; 0 means "no reading" and is excluded), rpm 0 (not pedalling) likewise; distance and energy are derived
/// from speed and actual watts, the elevation gain from actual watts with `params`.
pub fn summarize(samples: &[Sample], params: &ElevationParams) -> RideStats {
    let n = samples.len() as u64;
    let watts: u64 = samples.iter().map(|s| u64::from(s.status.actual_watts)).sum();
    let pulses: Vec<u64> = samples.iter().filter_map(Sample::effective_pulse).map(u64::from).collect();
    let rpms: Vec<u64> = samples
        .iter()
        .map(|s| u64::from(s.status.rpm))
        .filter(|&r| r > 0)
        .collect();
    let speed: u64 = samples.iter().map(|s| u64::from(s.status.speed_x10)).sum();
    RideStats {
        ride_secs: samples.iter().map(|s| s.ride_secs).max().unwrap_or(0),
        avg_watts: (watts + n / 2).checked_div(n).map_or(0, |avg| avg as u16),
        max_watts: samples.iter().map(|s| s.status.actual_watts).max().unwrap_or(0),
        avg_pulse: (!pulses.is_empty()).then(|| {
            let len = pulses.len() as u64;
            ((pulses.iter().sum::<u64>() + len / 2) / len) as u16
        }),
        max_pulse: pulses.iter().max().map(|&p| p as u16),
        avg_rpm: (!rpms.is_empty()).then(|| {
            let len = rpms.len() as u64;
            ((rpms.iter().sum::<u64>() + len / 2) / len) as u16
        }),
        max_rpm: rpms.iter().max().map(|&r| r as u16),
        distance_m: speed as f64 / 36.0,
        energy_kj: watts as f64 / 1000.0,
        elevation_gain_m: heights(params, watts_in_order(samples)).1,
    }
}

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

pub fn open(path: &Path) -> Result<Db> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!("database journal mode is {mode}, expected wal");
    }
    let db = Db::init(conn)?;
    db.close_unfinished()?;
    Ok(db)
}

#[cfg(test)]
pub fn open_in_memory() -> Result<Db> {
    Db::init(Connection::open_in_memory()?)
}

/// What a backup file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BackupInfo {
    pub schema_version: i64,
    pub persons: u32,
    pub programs: u32,
    pub rides: u32,
}

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("Keine gültige Kettler-Datenbank")]
    Invalid,
    #[error("Backup stammt von einer neueren Version")]
    TooNew,
    #[error("Während einer Fahrt nicht möglich")]
    RideRunning,
    #[error("Speicherfehler: {0}")]
    Storage(String),
}

/// Tables every schema version has.
const BACKUP_TABLES: [&str; 4] = ["persons", "programs", "rides", "samples"];

fn count_backup(conn: &Connection, schema_version: i64) -> rusqlite::Result<BackupInfo> {
    let count = |table: &str| conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0));
    Ok(BackupInfo { schema_version, persons: count("persons")?, programs: count("programs")?, rides: count("rides")? })
}

/// Every schema object with its full definition, for comparing a migrated backup with a fresh
/// database. Only the counters SQLite keeps by itself are left out.
fn schema_signature(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    conn.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_master \
         WHERE name <> 'sqlite_sequence' AND name NOT GLOB 'sqlite_stat[1-4]' \
         ORDER BY type, name",
    )?
    .query_map([], |r| {
        let sql: Option<String> = r.get(3)?;
        Ok(format!("{} {} {} {sql:?}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?
    .collect()
}

/// Opens the scratch copy of an upload for writing. Never creates it: if it vanished, an
/// empty database must not take the place of the backup.
fn open_scratch(src: &Path) -> Result<Connection, RestoreError> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
    Connection::open_with_flags(src, flags).map_err(|_| RestoreError::Invalid)
}

/// Checks that `src` is an intact Kettler database this version can read, without changing it.
pub fn inspect_backup(src: &Path) -> Result<BackupInfo, RestoreError> {
    let invalid = |_| RestoreError::Invalid;
    let conn = Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(invalid)?;
    let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).map_err(invalid)?;
    if check != "ok" {
        return Err(RestoreError::Invalid);
    }
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).map_err(invalid)?;
    if version > SCHEMA_VERSION {
        return Err(RestoreError::TooNew);
    }
    if version < 1 {
        return Err(RestoreError::Invalid);
    }
    for table in BACKUP_TABLES {
        let found: bool = conn
            .query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)", [table], |r| {
                r.get(0)
            })
            .map_err(invalid)?;
        if !found {
            return Err(RestoreError::Invalid);
        }
    }
    count_backup(&conn, version).map_err(invalid)
}

fn clean_name(name: &str) -> Result<&str, PersonError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(PersonError::EmptyName);
    }
    Ok(name)
}

fn ride_from_row(row: &Row) -> rusqlite::Result<RideSummary> {
    Ok(RideSummary {
        id: row.get(0)?,
        person_id: row.get(1)?,
        program_name: row.get(2)?,
        started_at: row.get(3)?,
        ride_secs: row.get(4)?,
        avg_watts: row.get(5)?,
        max_watts: row.get(6)?,
        avg_pulse: row.get(7)?,
        max_pulse: row.get(8)?,
        distance_m: row.get(9)?,
        energy_kj: row.get(10)?,
        finished: row.get(11)?,
        ended_at: row.get(12)?,
        avg_rpm: row.get(13)?,
        max_rpm: row.get(14)?,
        weight_kg: row.get(15)?,
        flat_watts: row.get(16)?,
        elevation_gain_m: row.get(17)?,
    })
}

/// Gain over a ride's stored samples with `params`.
/// Reads only the watts, so the migration to version 5 can use it before later columns exist.
fn stored_gain(conn: &Connection, ride_id: i64, params: &ElevationParams) -> Result<f64> {
    let mut stmt = conn.prepare("SELECT actual_watts FROM samples WHERE ride_id = ?1 ORDER BY ride_secs, rowid")?;
    let watts = stmt.query_map([ride_id], |r| r.get::<_, u16>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(heights(params, watts.into_iter().map(f64::from)).1)
}

fn program_from_row(row: &Row) -> Result<Program> {
    let blocks: String = row.get(2)?;
    Ok(Program {
        id: Some(row.get(0)?),
        name: row.get(1)?,
        blocks: serde_json::from_str(&blocks)?,
    })
}

fn recoveries_of(conn: &Connection, ride_id: i64) -> Result<Vec<Recovery>> {
    let mut stmt = conn.prepare(
        "SELECT ride_secs, pulse_start, difference, grade_x10, unix_ms, source FROM recoveries          WHERE ride_id = ?1 ORDER BY unix_ms",
    )?;
    let rows = stmt.query_map([ride_id], |r| {
        Ok(Recovery {
            ride_secs: r.get(0)?,
            pulse_start: r.get(1)?,
            difference: r.get(2)?,
            grade_x10: r.get(3)?,
            at_unix_ms: r.get(4)?,
            source: RecoverySource::parse(&r.get::<_, String>(5)?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn samples_of(conn: &Connection, ride_id: i64) -> Result<Vec<Sample>> {
    let mut stmt = conn.prepare(
        "SELECT ride_secs, pulse, rpm, speed_x10, distance, status_target_watts, energy, \
         elapsed_secs, actual_watts, target_watts, unix_ms, strap_pulse FROM samples WHERE ride_id = ?1 \
         ORDER BY ride_secs, rowid",
    )?;
    let rows = stmt.query_map([ride_id], |r| {
        Ok(Sample {
            ride_secs: r.get(0)?,
            status: Status {
                pulse: r.get(1)?,
                rpm: r.get(2)?,
                speed_x10: r.get(3)?,
                distance: r.get(4)?,
                target_watts: r.get(5)?,
                energy: r.get(6)?,
                elapsed_secs: r.get(7)?,
                actual_watts: r.get(8)?,
                recovery: false,
            },
            target_watts: r.get(9)?,
            at_unix_ms: r.get(10)?,
            strap_pulse: r.get(11)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

impl Db {
    fn init(mut conn: Connection) -> Result<Db> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let tx = conn.transaction()?;
        let mut version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            bail!("unsupported database schema version {version}");
        }
        // Step by step from the stored version; all in one transaction.
        while version < SCHEMA_VERSION {
            match version {
                0 => {
                    tx.execute_batch(SCHEMA_V1)?;
                    for program in templates() {
                        tx.execute(
                            "INSERT INTO programs (name, blocks) VALUES (?1, ?2)",
                            params![program.name, serde_json::to_string(&program.blocks)?],
                        )?;
                    }
                }
                1 => tx.execute_batch(MIGRATE_V2)?,
                2 => tx.execute_batch(MIGRATE_V3)?,
                3 => tx.execute_batch(MIGRATE_V4)?,
                4 => {
                    tx.execute_batch(MIGRATE_V5)?;
                    // Old rides got the default profile from the column defaults.
                    let ids: Vec<i64> = tx
                        .prepare("SELECT id FROM rides")?
                        .query_map([], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?;
                    for id in ids {
                        let gain = stored_gain(&tx, id, &ElevationParams::DEFAULT)?;
                        tx.execute("UPDATE rides SET elevation_gain_m = ?2 WHERE id = ?1", params![id, gain])?;
                    }
                }
                5 => tx.execute_batch(MIGRATE_V6)?,
                v => unreachable!("no migration from version {v}"),
            }
            version += 1;
            tx.pragma_update(None, "user_version", version)?;
        }
        tx.commit()?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Schema version and counts of the live database.
    pub fn stats(&self) -> Result<BackupInfo> {
        let conn = self.conn();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        Ok(count_backup(&conn, version)?)
    }

    /// Writes a consistent copy of the database to `dest` (a new file).
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        self.conn().backup(rusqlite::MAIN_DB, dest, None::<fn(rusqlite::backup::Progress)>)?;
        Ok(())
    }

    /// Replaces all data with the backup at `src`, all or nothing. `src` is migrated and
    /// its unfinished rides are closed in place, so it must be a scratch copy.
    pub fn restore_from(&self, src: &Path) -> Result<BackupInfo, RestoreError> {
        inspect_backup(src)?;
        let storage = |e: anyhow::Error| RestoreError::Storage(e.to_string());
        let copy = Db::init(open_scratch(src)?).map_err(storage)?;
        copy.close_unfinished().map_err(storage)?;
        let source = copy.conn();
        // Only data comes from the file: no triggers, views or other changes to the schema.
        let reference = Connection::open_in_memory().map_err(|e| storage(e.into()))?;
        let reference = Db::init(reference).map_err(storage)?;
        let expected = schema_signature(&reference.conn()).map_err(|e| storage(e.into()))?;
        if schema_signature(&source).map_err(|_| RestoreError::Invalid)? != expected {
            return Err(RestoreError::Invalid);
        }
        let broken: bool = source
            .query_row("SELECT EXISTS (SELECT 1 FROM pragma_foreign_key_check)", [], |r| r.get(0))
            .map_err(|_| RestoreError::Invalid)?;
        if broken {
            return Err(RestoreError::Invalid);
        }
        let mut live = self.conn();
        let backup = rusqlite::backup::Backup::new(&source, &mut live).map_err(|e| storage(e.into()))?;
        // A single step copies everything in one transaction.
        match backup.step(-1).map_err(|e| storage(e.into()))? {
            rusqlite::backup::StepResult::Done => {}
            other => return Err(RestoreError::Storage(format!("Kopie unvollständig: {other:?}"))),
        }
        drop(backup);
        count_backup(&live, SCHEMA_VERSION).map_err(|e| storage(e.into()))
    }

    pub fn list_persons(&self) -> Result<Vec<Person>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, name, weight_kg, flat_watts, strap_id FROM persons ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Person {
                id: r.get(0)?,
                name: r.get(1)?,
                weight_kg: r.get(2)?,
                flat_watts: r.get(3)?,
                strap_id: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_person(&self, name: &str) -> Result<i64> {
        let name = clean_name(name)?;
        let conn = self.conn();
        conn.execute("INSERT INTO persons (name) VALUES (?1)", [name])?;
        Ok(conn.last_insert_rowid())
    }

    /// Changes the given fields of a person; `Ok(false)` if the id is unknown. An invalid
    /// value fails with a `PersonError` and changes nothing. Past rides keep their snapshot.
    pub fn update_person(&self, id: i64, u: &PersonUpdate) -> Result<bool> {
        let name = u.name.as_deref().map(clean_name).transpose()?;
        if u.weight_kg.is_some_and(|w| !WEIGHT_KG.contains(&w)) {
            return Err(PersonError::Weight.into());
        }
        if u.flat_watts.is_some_and(|f| f > MAX_FLAT_WATTS) {
            return Err(PersonError::Flat.into());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if let Some(Some(strap)) = u.strap_id {
            let known: bool =
                tx.query_row("SELECT EXISTS (SELECT 1 FROM straps WHERE id = ?1)", [strap], |r| r.get(0))?;
            if !known {
                return Err(StrapError::Unknown.into());
            }
        }
        let n = tx.execute(
            "UPDATE persons SET name = COALESCE(?2, name), weight_kg = COALESCE(?3, weight_kg), \
             flat_watts = COALESCE(?4, flat_watts), strap_id = CASE WHEN ?5 THEN ?6 ELSE strap_id END \
             WHERE id = ?1",
            params![id, name, u.weight_kg, u.flat_watts, u.strap_id.is_some(), u.strap_id.flatten()],
        )?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn list_straps(&self) -> Result<Vec<Strap>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, address, name FROM straps ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok(Strap { id: r.get(0)?, address: r.get(1)?, name: r.get(2)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The address is stored in upper case; a second strap with the same address fails.
    pub fn add_strap(&self, address: &str, name: &str) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            return Err(StrapError::EmptyName.into());
        }
        let address = address.trim().to_ascii_uppercase();
        let conn = self.conn();
        match conn.execute("INSERT INTO straps (address, name) VALUES (?1, ?2)", params![address, name]) {
            Ok(_) => Ok(conn.last_insert_rowid()),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                Err(StrapError::Duplicate.into())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `Ok(false)` if the id is unknown.
    pub fn rename_strap(&self, id: i64, name: &str) -> Result<bool> {
        let name = name.trim();
        if name.is_empty() {
            return Err(StrapError::EmptyName.into());
        }
        Ok(self.conn().execute("UPDATE straps SET name = ?2 WHERE id = ?1", params![id, name])? > 0)
    }

    /// Clears the persons' assignment first; samples keep their values. `Ok(false)` if the
    /// id is unknown.
    pub fn delete_strap(&self, id: i64) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("UPDATE persons SET strap_id = NULL WHERE strap_id = ?1", [id])?;
        let n = tx.execute("DELETE FROM straps WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn list_programs(&self) -> Result<Vec<Program>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, name, blocks FROM programs ORDER BY id")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            match program_from_row(row) {
                Ok(p) => out.push(p),
                Err(e) => tracing::warn!("skipping stored program with invalid data: {e}"),
            }
        }
        Ok(out)
    }

    pub fn get_program(&self, id: i64) -> Result<Option<Program>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, name, blocks FROM programs WHERE id = ?1")?;
        let mut rows = stmt.query([id])?;
        rows.next()?.map(program_from_row).transpose()
    }

    /// Inserts when `id` is `None`, otherwise replaces the stored program.
    pub fn save_program(&self, program: &Program) -> Result<i64> {
        program.validate(&DeviceLimits::default())?;
        let blocks = serde_json::to_string(&program.blocks)?;
        let conn = self.conn();
        match program.id {
            None => {
                conn.execute(
                    "INSERT INTO programs (name, blocks) VALUES (?1, ?2)",
                    params![program.name, blocks],
                )?;
                Ok(conn.last_insert_rowid())
            }
            Some(id) => {
                let n = conn.execute(
                    "UPDATE programs SET name = ?2, blocks = ?3 WHERE id = ?1",
                    params![id, program.name, blocks],
                )?;
                if n == 0 {
                    bail!("program {id} not found");
                }
                Ok(id)
            }
        }
    }

    pub fn delete_program(&self, id: i64) -> Result<()> {
        self.conn().execute("DELETE FROM programs WHERE id = ?1", [id])?;
        Ok(())
    }

    /// The program (as JSON) and the person's weight and flat power are stored as a
    /// snapshot so later edits don't alter old rides. Returns the ride id and exactly the
    /// snapshot that was stored.
    pub fn start_ride(
        &self,
        person_id: i64,
        program: Option<&Program>,
        started_at: i64,
    ) -> Result<(i64, ElevationParams)> {
        if let Some(p) = program {
            p.validate(&DeviceLimits::default())?;
        }
        let json = program.map(serde_json::to_string).transpose()?;
        let name = program.map(|p| p.name.as_str());
        let row = self
            .conn()
            .query_row(
                "INSERT INTO rides (person_id, program_name, program_json, started_at, weight_kg,                  flat_watts) SELECT id, ?2, ?3, ?4, weight_kg, flat_watts FROM persons WHERE id = ?1                  RETURNING id, weight_kg, flat_watts",
                params![person_id, name, json, started_at],
                |r| Ok((r.get(0)?, ElevationParams { weight_kg: r.get(1)?, flat_watts: r.get(2)? })),
            )
            .optional()?;
        row.with_context(|| format!("person {person_id} not found"))
    }

    pub fn add_sample(&self, ride_id: i64, sample: &Sample) -> Result<()> {
        let s = &sample.status;
        self.conn().execute(
            "INSERT INTO samples (ride_id, ride_secs, pulse, rpm, speed_x10, distance, \
             status_target_watts, energy, elapsed_secs, actual_watts, target_watts, unix_ms, strap_pulse) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                ride_id,
                sample.ride_secs,
                s.pulse,
                s.rpm,
                s.speed_x10,
                s.distance,
                s.target_watts,
                s.energy,
                s.elapsed_secs,
                s.actual_watts,
                sample.target_watts,
                sample.at_unix_ms,
                sample.strap_pulse
            ],
        )?;
        Ok(())
    }

    pub fn add_recovery(&self, ride_id: i64, r: &Recovery) -> Result<()> {
        self.conn().execute(
            "INSERT INTO recoveries (ride_id, ride_secs, pulse_start, difference, grade_x10, unix_ms, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ride_id, r.ride_secs, r.pulse_start, r.difference, r.grade_x10, r.at_unix_ms, r.source.as_str()],
        )?;
        Ok(())
    }

    /// The ride really started (first pedalling) at `started_at`.
    pub fn set_ride_started(&self, ride_id: i64, started_at: i64) -> Result<()> {
        let n = self
            .conn()
            .execute("UPDATE rides SET started_at = ?2 WHERE id = ?1", params![ride_id, started_at])?;
        if n == 0 {
            bail!("ride {ride_id} not found");
        }
        Ok(())
    }

    /// Removes a ride with its samples; `false` if there was no such ride.
    pub fn delete_ride(&self, ride_id: i64) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM samples WHERE ride_id = ?1", [ride_id])?;
        tx.execute("DELETE FROM recoveries WHERE ride_id = ?1", [ride_id])?;
        let n = tx.execute("DELETE FROM rides WHERE id = ?1", [ride_id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Removes a person with all their rides and the samples of those rides, in one
    /// transaction; `false` if there was no such person. Programs are not touched.
    pub fn delete_person(&self, id: i64) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM samples WHERE ride_id IN (SELECT id FROM rides WHERE person_id = ?1)",
            [id],
        )?;
        tx.execute(
            "DELETE FROM recoveries WHERE ride_id IN (SELECT id FROM rides WHERE person_id = ?1)",
            [id],
        )?;
        tx.execute("DELETE FROM rides WHERE person_id = ?1", [id])?;
        let n = tx.execute("DELETE FROM persons WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn count_rides(&self, person_id: i64) -> Result<u32> {
        Ok(self.conn().query_row("SELECT COUNT(*) FROM rides WHERE person_id = ?1", [person_id], |r| r.get(0))?)
    }

    /// Stores the summary. The elevation gain is computed here from the stored samples
    /// with the ride's snapshot; `summary.weight_kg` and `flat_watts` are ignored (they may
    /// be placeholders) and the snapshot is never overwritten. `summary.elevation_gain_m`
    /// is only used if the samples cannot be read.
    pub fn finish_ride(&self, ride_id: i64, summary: &RideSummary) -> Result<()> {
        let conn = self.conn();
        let Some(params) = conn
            .query_row("SELECT weight_kg, flat_watts FROM rides WHERE id = ?1", [ride_id], |r| {
                Ok(ElevationParams { weight_kg: r.get(0)?, flat_watts: r.get(1)? })
            })
            .optional()?
        else {
            bail!("ride {ride_id} not found");
        };
        // The summary matters more than the gain: keep it even if the samples are unreadable.
        let gain = stored_gain(&conn, ride_id, &params).unwrap_or_else(|e| {
            tracing::warn!(ride_id, "elevation gain not computed: {e}");
            summary.elevation_gain_m
        });
        conn.execute(
            "UPDATE rides SET finished = 1, ride_secs = ?2, avg_watts = ?3, max_watts = ?4, \
             avg_pulse = ?5, max_pulse = ?6, distance_m = ?7, energy_kj = ?8, ended_at = ?9, \
             avg_rpm = ?10, max_rpm = ?11, elevation_gain_m = ?12 WHERE id = ?1",
            params![
                ride_id,
                summary.ride_secs,
                summary.avg_watts,
                summary.max_watts,
                summary.avg_pulse,
                summary.max_pulse,
                summary.distance_m,
                summary.energy_kj,
                summary.ended_at,
                summary.avg_rpm,
                summary.max_rpm,
                gain
            ],
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn list_rides(&self, person_id: i64) -> Result<Vec<RideSummary>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RIDE_COLUMNS} FROM rides WHERE person_id = ?1 \
             ORDER BY started_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map([person_id], ride_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_ride(&self, id: i64) -> Result<Option<RideDetail>> {
        let conn = self.conn();
        let found = conn
            .query_row(
                &format!("SELECT {RIDE_COLUMNS}, program_json FROM rides WHERE id = ?1"),
                [id],
                |r| Ok((ride_from_row(r)?, r.get::<_, Option<String>>(18)?)),
            )
            .optional()?;
        let Some((summary, json)) = found else {
            return Ok(None);
        };
        let program_segments = json.and_then(|j| match serde_json::from_str::<Program>(&j) {
            Ok(p) => match p.validate(&DeviceLimits::default()) {
                Ok(()) => Some(p.expand()),
                Err(e) => {
                    tracing::warn!("ride {id} has an unusable program snapshot: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("ride {id} has an invalid program snapshot: {e}");
                None
            }
        });
        let samples = samples_of(&conn, id)?;
        let recoveries = recoveries_of(&conn, id)?;
        let params = summary.elevation_params();
        // `samples_of` returns them in ride time order, so the heights align with them.
        let watts = samples.iter().map(|s| f64::from(s.status.actual_watts));
        let elevation_m = heights(&params, watts).0.into_iter().map(|h| h as f32).collect();
        let elevation_preview = program_segments.as_deref().map(|s| preview(&params, s));
        Ok(Some(RideDetail { summary, program_segments, samples, recoveries, elevation_m, elevation_preview }))
    }

    /// Rides without an end (e.g. after a crash) get a summary computed from their samples
    /// and end at the last sample's time; rides without samples are removed.
    pub fn close_unfinished(&self) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let rides: Vec<(i64, i64, ElevationParams)> = {
            let mut stmt =
                tx.prepare("SELECT id, started_at, weight_kg, flat_watts FROM rides WHERE finished = 0")?;
            stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, ElevationParams { weight_kg: r.get(2)?, flat_watts: r.get(3)? }))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        for (id, started_at, elevation) in rides {
            let samples = samples_of(&tx, id)?;
            let Some(last) = samples.last() else {
                tx.execute("DELETE FROM recoveries WHERE ride_id = ?1", [id])?;
                tx.execute("DELETE FROM rides WHERE id = ?1", [id])?;
                continue;
            };
            let ended_at = last
                .at_unix_ms
                .map_or(started_at + i64::from(last.ride_secs), |ms| ms / 1000);
            let stats = summarize(&samples, &elevation);
            tx.execute(
                "UPDATE rides SET finished = 1, ride_secs = ?2, avg_watts = ?3, max_watts = ?4, \
                 avg_pulse = ?5, max_pulse = ?6, distance_m = ?7, energy_kj = ?8, ended_at = ?9, \
                 avg_rpm = ?10, max_rpm = ?11, elevation_gain_m = ?12 WHERE id = ?1",
                params![
                    id,
                    stats.ride_secs,
                    stats.avg_watts,
                    stats.max_watts,
                    stats.avg_pulse,
                    stats.max_pulse,
                    stats.distance_m,
                    stats.energy_kj,
                    ended_at,
                    stats.avg_rpm,
                    stats.max_rpm,
                    stats.elevation_gain_m
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elevation::heights;
    use crate::program::{Block, Step};
    use crate::protocol::Status;

    fn program() -> Program {
        Program {
            id: None,
            name: "Test".into(),
            blocks: vec![Block {
                repeat: 2,
                steps: vec![Step {
                    name: "Step".into(),
                    duration_secs: 60,
                    start_watts: 100,
                    end_watts: 150,
                }],
            }],
        }
    }

    fn sample(secs: u32, pulse: u16, watts: u16) -> Sample {
        Sample {
            ride_secs: secs,
            status: Status {
                pulse,
                rpm: 80,
                speed_x10: 360,
                distance: 0,
                target_watts: watts,
                energy: 0,
                elapsed_secs: secs,
                actual_watts: watts,
                recovery: false,
            },
            target_watts: watts,
            at_unix_ms: None,
            strap_pulse: None,
        }
    }

    fn sample_count(db: &Db) -> i64 {
        db.conn().query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0)).unwrap()
    }

    fn ride_with_samples(db: &Db, person: i64, n: u32) -> i64 {
        let id = db.start_ride(person, None, 1000).unwrap().0;
        for secs in 1..=n {
            db.add_sample(id, &sample(secs, 100, 90)).unwrap();
        }
        id
    }

    fn recovery(ride_secs: u32) -> Recovery {
        Recovery { ride_secs, pulse_start: 106, difference: 6, grade_x10: 57, at_unix_ms: 1_000 + i64::from(ride_secs), source: RecoverySource::Cockpit }
    }

    #[test]
    fn recoveries_roundtrip_and_go_with_their_ride() {
        let db = open_in_memory().unwrap();
        let person = db.add_person("Bob").unwrap();
        let a = ride_with_samples(&db, person, 3);
        let b = ride_with_samples(&db, person, 3);
        db.add_recovery(a, &recovery(3)).unwrap();
        db.add_recovery(a, &recovery(1)).unwrap();
        db.add_recovery(b, &recovery(2)).unwrap();
        // In the order they were measured.
        assert_eq!(db.get_ride(a).unwrap().unwrap().recoveries, vec![recovery(1), recovery(3)]);
        assert!(db.add_recovery(9999, &recovery(1)).is_err());
        assert!(db.delete_ride(a).unwrap());
        let left = db.conn().query_row("SELECT count(*) FROM recoveries", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(left, 1);
        assert!(db.delete_person(person).unwrap());
        let left = db.conn().query_row("SELECT count(*) FROM recoveries", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn close_unfinished_removes_recoveries_of_empty_rides() {
        let db = open_in_memory().unwrap();
        let person = db.add_person("Alice").unwrap();
        let r = db.start_ride(person, None, 100).unwrap().0;
        db.add_recovery(r, &recovery(0)).unwrap();
        db.close_unfinished().unwrap();
        assert!(db.get_ride(r).unwrap().is_none());
    }

    #[test]
    fn delete_ride_removes_its_samples_only() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("A").unwrap();
        let a = ride_with_samples(&db, p, 3);
        let b = ride_with_samples(&db, p, 2);
        assert!(db.delete_ride(a).unwrap());
        assert!(db.get_ride(a).unwrap().is_none());
        assert_eq!(db.get_ride(b).unwrap().unwrap().samples.len(), 2);
        assert_eq!(sample_count(&db), 2);
        assert!(!db.delete_ride(a).unwrap());
    }

    #[test]
    fn delete_person_cascades_and_spares_others_and_programs() {
        let db = open_in_memory().unwrap();
        let alice = db.add_person("Alice").unwrap();
        let bob = db.add_person("Bob").unwrap();
        ride_with_samples(&db, alice, 3);
        ride_with_samples(&db, alice, 4);
        let bobs = ride_with_samples(&db, bob, 2);
        let programs = db.list_programs().unwrap().len();
        assert_eq!(db.count_rides(alice).unwrap(), 2);
        assert_eq!(db.count_rides(bob).unwrap(), 1);
        assert!(db.delete_person(alice).unwrap());
        assert_eq!(db.list_persons().unwrap().len(), 1);
        assert!(db.list_rides(alice).unwrap().is_empty());
        assert_eq!(db.count_rides(alice).unwrap(), 0);
        // No orphaned samples: only Bob's two remain.
        assert_eq!(sample_count(&db), 2);
        assert_eq!(db.get_ride(bobs).unwrap().unwrap().samples.len(), 2);
        assert_eq!(db.list_programs().unwrap().len(), programs);
        assert!(!db.delete_person(alice).unwrap());
    }

    #[test]
    fn new_db_has_templates() {
        let db = open_in_memory().unwrap();
        let names: Vec<_> = db.list_programs().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Grundlage 30", "Pyramide", "4 × 4", "Rampentest"]);
    }

    #[test]
    fn templates_not_reseeded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        let db = open(&path).unwrap();
        let first = db.list_programs().unwrap()[0].id.unwrap();
        db.delete_program(first).unwrap();
        drop(db);
        let db = open(&path).unwrap();
        assert_eq!(db.list_programs().unwrap().len(), 3);
    }

    #[test]
    fn persons_roundtrip() {
        let db = open_in_memory().unwrap();
        let id = db.add_person(" Ich ").unwrap();
        let rename = |n: &str| PersonUpdate { name: Some(n.into()), ..PersonUpdate::default() };
        assert!(db.update_person(id, &rename(" Ich2 ")).unwrap());
        let list = db.list_persons().unwrap();
        assert_eq!(list, vec![Person { id, name: "Ich2".into(), weight_kg: 75.0, flat_watts: 70, strap_id: None }]);
        assert!(db.add_person("  ").is_err());
        assert!(db.update_person(id, &rename("")).is_err());
        assert!(!db.update_person(id + 1, &rename("X")).unwrap());
    }

    #[test]
    fn programs_roundtrip_and_delete() {
        let db = open_in_memory().unwrap();
        let id = db.save_program(&program()).unwrap();
        let mut expected = program();
        expected.id = Some(id);
        assert_eq!(db.get_program(id).unwrap(), Some(expected.clone()));
        expected.name = "Neu".into();
        assert_eq!(db.save_program(&expected).unwrap(), id);
        assert_eq!(db.list_programs().unwrap().last(), Some(&expected));
        db.delete_program(id).unwrap();
        assert_eq!(db.get_program(id).unwrap(), None);
        assert_eq!(db.list_programs().unwrap().len(), 4);
    }

    #[test]
    fn ride_with_samples_roundtrip() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let prog = program();
        let r = db.start_ride(p, Some(&prog), 1000).unwrap().0;
        for i in 1..=3 {
            db.add_sample(r, &sample(i, 120, 100)).unwrap();
        }
        let mut s = db.get_ride(r).unwrap().unwrap().summary;
        assert!(!s.finished);
        s.ride_secs = 3;
        s.distance_m = 30.0;
        db.finish_ride(r, &s).unwrap();
        let d = db.get_ride(r).unwrap().unwrap();
        assert_eq!(d.samples.len(), 3);
        assert_eq!(d.samples[1], sample(2, 120, 100));
        assert!(d.summary.finished);
        assert_eq!(d.summary.distance_m, 30.0);
        assert_eq!(d.summary.program_name.as_deref(), Some("Test"));
        assert_eq!(d.program_segments.unwrap().len(), 2);
        assert_eq!(db.list_rides(p).unwrap().len(), 1);
    }

    #[test]
    fn free_ride_has_no_segments_and_list_is_newest_first() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let a = db.start_ride(p, None, 10).unwrap().0;
        let b = db.start_ride(p, None, 20).unwrap().0;
        assert!(db.get_ride(a).unwrap().unwrap().program_segments.is_none());
        let ids: Vec<i64> = db.list_rides(p).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![b, a]);
    }

    #[test]
    fn unfinished_ride_closed_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        let db = open(&path).unwrap();
        let p = db.add_person("Ich").unwrap();
        let r = db.start_ride(p, None, 5).unwrap().0;
        for i in 1..=5 {
            db.add_sample(r, &sample(i, 100, 200)).unwrap();
        }
        drop(db);
        let db = open(&path).unwrap();
        let rides = db.list_rides(p).unwrap();
        assert_eq!(rides.len(), 1);
        assert!(rides[0].finished);
        assert_eq!(rides[0].ride_secs, 5);
        assert_eq!(rides[0].avg_watts, 200);
        assert_eq!(rides[0].max_watts, 200);
        assert_eq!(rides[0].avg_pulse, Some(100));
        assert_eq!(rides[0].max_pulse, Some(100));
        assert_eq!(rides[0].distance_m, 50.0);
        assert!((rides[0].energy_kj - 1.0).abs() < 1e-9);
        assert_eq!(db.get_ride(r).unwrap().unwrap().samples.len(), 5);
    }

    #[test]
    fn pulse_zero_not_counted() {
        let stats = summarize(&[sample(1, 0, 100), sample(2, 0, 100)], &ElevationParams::DEFAULT);
        assert_eq!(stats.avg_pulse, None);
        assert_eq!(stats.max_pulse, None);
        let stats = summarize(&[sample(1, 0, 100), sample(2, 120, 100), sample(3, 140, 100)], &ElevationParams::DEFAULT);
        assert_eq!(stats.avg_pulse, Some(130));
        assert_eq!(stats.max_pulse, Some(140));
    }

    fn sample_rpm(secs: u32, rpm: u16) -> Sample {
        let mut s = sample(secs, 0, 100);
        s.status.rpm = rpm;
        s
    }

    #[test]
    fn rpm_zero_not_counted() {
        let stats = summarize(&[sample_rpm(1, 0), sample_rpm(2, 0)], &ElevationParams::DEFAULT);
        assert_eq!((stats.avg_rpm, stats.max_rpm), (None, None));
        let stats = summarize(&[], &ElevationParams::DEFAULT);
        assert_eq!((stats.avg_rpm, stats.max_rpm), (None, None));
        // Zeros are ignored by the average: (60 + 91) / 2 = 75.5, rounded up.
        let stats = summarize(&[
            sample_rpm(1, 0),
            sample_rpm(2, 60),
            sample_rpm(3, 0),
            sample_rpm(4, 91),
            sample_rpm(5, 0),
        ], &ElevationParams::DEFAULT);
        assert_eq!(stats.avg_rpm, Some(76));
        assert_eq!(stats.max_rpm, Some(91));
    }

    #[test]
    fn rpm_summary_stored_by_finish_and_close_unfinished() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let r = db.start_ride(p, None, 10).unwrap().0;
        db.add_sample(r, &sample_rpm(1, 70)).unwrap();
        let mut s = db.get_ride(r).unwrap().unwrap().summary;
        assert_eq!((s.avg_rpm, s.max_rpm), (None, None));
        s.avg_rpm = Some(70);
        s.max_rpm = Some(75);
        db.finish_ride(r, &s).unwrap();
        let got = db.list_rides(p).unwrap();
        assert_eq!((got[0].avg_rpm, got[0].max_rpm), (Some(70), Some(75)));

        let a = db.start_ride(p, None, 20).unwrap().0;
        for (i, rpm) in [0, 60, 91].into_iter().enumerate() {
            db.add_sample(a, &sample_rpm(i as u32 + 1, rpm)).unwrap();
        }
        let b = db.start_ride(p, None, 30).unwrap().0;
        db.add_sample(b, &sample_rpm(1, 0)).unwrap();
        db.close_unfinished().unwrap();
        let a = db.get_ride(a).unwrap().unwrap().summary;
        assert_eq!((a.avg_rpm, a.max_rpm), (Some(76), Some(91)));
        let b = db.get_ride(b).unwrap().unwrap().summary;
        assert_eq!((b.avg_rpm, b.max_rpm), (None, None));
    }

    #[test]
    fn v2_database_migrates_and_backfills_cadence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        {
            // The schema as it was at version 2.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(MIGRATE_V2).unwrap();
            conn.execute_batch(
                "INSERT INTO persons (name) VALUES ('Alt');
                 INSERT INTO programs (name, blocks) VALUES ('Eigenes', '[]');
                 -- 1: mixed zeros and values, average 75.5 rounds up to 76
                 INSERT INTO rides (person_id, started_at, finished, ride_secs, avg_watts, max_watts, ended_at)
                     VALUES (1, 100, 1, 5, 100, 100, 105);
                 -- 2: only zero rpm
                 INSERT INTO rides (person_id, started_at, finished, ride_secs, avg_watts, max_watts, ended_at)
                     VALUES (1, 200, 1, 2, 100, 100, 202);
                 -- 3: no samples at all
                 INSERT INTO rides (person_id, started_at, finished, ride_secs, avg_watts, max_watts, ended_at)
                     VALUES (1, 300, 1, 0, 0, 0, 300);
                 -- 4: unfinished ride with pedalling (closed on open, keeps its backfill)
                 INSERT INTO rides (person_id, started_at) VALUES (1, 400);
                 INSERT INTO samples (ride_id, ride_secs, pulse, rpm, speed_x10, distance,
                     status_target_watts, energy, elapsed_secs, actual_watts, target_watts, unix_ms)
                 VALUES
                     (1, 1, 0,  0, 360, 0, 100, 0, 1, 100, 100, 101000),
                     (1, 2, 0, 60, 360, 0, 100, 0, 2, 100, 100, 102000),
                     (1, 3, 0,  0, 360, 0, 100, 0, 3, 100, 100, NULL),
                     (1, 4, 0, 91, 360, 0, 100, 0, 4, 100, 100, 104000),
                     (1, 5, 0,  0, 360, 0, 100, 0, 5, 100, 100, 105000),
                     (2, 1, 0,  0, 360, 0, 100, 0, 1, 100, 100, NULL),
                     (2, 2, 0,  0, 360, 0, 100, 0, 2, 100, 100, NULL),
                     (4, 1, 0, 50, 360, 0, 100, 0, 1, 100, 100, NULL),
                     (4, 2, 0, 51, 360, 0, 100, 0, 2, 100, 100, NULL);
                 -- 5: 200 finished seconds with varied rpm (about every 7th second not pedalling)
                 INSERT INTO rides (person_id, started_at, finished, ride_secs, avg_watts, max_watts, ended_at)
                     VALUES (1, 500, 1, 200, 100, 100, 700);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200)
                 INSERT INTO samples (ride_id, ride_secs, pulse, rpm, speed_x10, distance,
                     status_target_watts, energy, elapsed_secs, actual_watts, target_watts, unix_ms)
                 SELECT 5, i, 0, CASE WHEN i % 7 = 0 THEN 0 ELSE 55 + (i * 37) % 61 END,
                     360, 0, 100, 0, i, 100, 100, NULL FROM n;
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let db = open(&path).unwrap();
        let version: i64 = db.conn().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let cadence = |id: i64| {
            let s = db.get_ride(id).unwrap().unwrap().summary;
            (s.avg_rpm, s.max_rpm)
        };
        assert_eq!(cadence(1), (Some(76), Some(91)));
        assert_eq!(cadence(2), (None, None));
        assert_eq!(cadence(3), (None, None));
        assert_eq!(cadence(4), (Some(51), Some(51)));
        // The backfill agrees with summarize over the stored samples, for every ride.
        for id in 1..=5 {
            let d = db.get_ride(id).unwrap().unwrap();
            let stats = summarize(&d.samples, &ElevationParams::DEFAULT);
            assert_eq!((d.summary.avg_rpm, d.summary.max_rpm), (stats.avg_rpm, stats.max_rpm), "ride {id}");
        }
        // Nothing else was touched.
        assert_eq!(db.list_persons().unwrap().len(), 1);
        assert_eq!(sample_count(&db), 209);
        let first = db.get_ride(1).unwrap().unwrap();
        assert_eq!(first.samples.len(), 5);
        assert_eq!(first.samples[3].status.rpm, 91);
        assert_eq!(first.samples[1].at_unix_ms, Some(102_000));
        assert_eq!(
            (first.summary.started_at, first.summary.ride_secs, first.summary.ended_at),
            (100, 5, Some(105))
        );
        assert_eq!(db.count_rides(1).unwrap(), 5);
        let varied = db.get_ride(5).unwrap().unwrap().summary;
        assert!(varied.avg_rpm.is_some() && varied.max_rpm > varied.avg_rpm);
        // Reopening is a no-op and new rides work.
        drop(db);
        let db = open(&path).unwrap();
        assert_eq!(db.get_ride(1).unwrap().unwrap().summary.avg_rpm, Some(76));
    }

    #[test]
    fn failed_migration_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(MIGRATE_V2).unwrap();
            // A column of that name already exists, so the v3 ALTER fails.
            conn.execute_batch("ALTER TABLE rides ADD COLUMN max_rpm INTEGER; PRAGMA user_version = 2;")
                .unwrap();
        }
        assert!(open(&path).is_err());
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        let has_avg: i64 = conn
            .query_row("SELECT count(*) FROM pragma_table_info('rides') WHERE name = 'avg_rpm'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(has_avg, 0);
    }

    #[test]
    fn ride_snapshot_survives_program_edit_and_delete() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let pid = db.save_program(&program()).unwrap();
        let original = db.get_program(pid).unwrap().unwrap();
        let r = db.start_ride(p, Some(&original), 1).unwrap().0;
        let mut edited = original.clone();
        edited.name = "Anders".into();
        edited.blocks[0].repeat = 5;
        db.save_program(&edited).unwrap();
        db.delete_program(pid).unwrap();
        let d = db.get_ride(r).unwrap().unwrap();
        assert_eq!(d.program_segments, Some(original.expand()));
        assert_eq!(d.summary.program_name.as_deref(), Some("Test"));
    }

    #[test]
    fn unsupported_schema_version_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        assert!(open(&path).is_err());
    }

    #[test]
    fn file_db_uses_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("k.db")).unwrap();
        let mode: String = db
            .conn()
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn duplicate_sample_second_rejected() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let r = db.start_ride(p, None, 1).unwrap().0;
        db.add_sample(r, &sample(1, 100, 100)).unwrap();
        assert!(db.add_sample(r, &sample(1, 100, 100)).is_err());
    }

    #[test]
    fn foreign_keys_enforced() {
        let db = open_in_memory().unwrap();
        assert!(db.start_ride(42, None, 1).is_err());
        assert!(db.add_sample(42, &sample(1, 100, 100)).is_err());
    }

    #[test]
    fn corrupt_snapshot_and_program_degrade() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let r = db.start_ride(p, Some(&program()), 1).unwrap().0;
        db.add_sample(r, &sample(1, 100, 100)).unwrap();
        let good = db.save_program(&program()).unwrap();
        let bad = db.save_program(&program()).unwrap();
        {
            let conn = db.conn();
            conn.execute("UPDATE rides SET program_json = 'x'", []).unwrap();
            conn.execute("UPDATE programs SET blocks = 'x' WHERE id = ?1", [bad]).unwrap();
        }
        let d = db.get_ride(r).unwrap().unwrap();
        assert!(d.program_segments.is_none());
        assert_eq!(d.samples.len(), 1);
        let ids: Vec<_> = db.list_programs().unwrap().iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![Some(1), Some(2), Some(3), Some(4), Some(good)]);
    }

    #[test]
    fn invalid_programs_rejected_and_oversized_snapshot_not_expanded() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let mut bomb = program();
        bomb.blocks[0].repeat = 1000;
        assert!(db.save_program(&bomb).is_err());
        assert!(db.start_ride(p, Some(&bomb), 1).is_err());
        let r = db.start_ride(p, Some(&program()), 1).unwrap().0;
        let json = serde_json::to_string(&bomb).unwrap();
        db.conn()
            .execute("UPDATE rides SET program_json = ?1", [json])
            .unwrap();
        assert!(db.get_ride(r).unwrap().unwrap().program_segments.is_none());
    }

    #[test]
    fn save_program_unknown_id_fails() {
        let db = open_in_memory().unwrap();
        let mut p = program();
        p.id = Some(7);
        assert!(db.save_program(&p).is_err());
    }

    #[test]
    fn close_unfinished_handles_empty_and_leaves_finished_alone() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let done = db.start_ride(p, None, 1).unwrap().0;
        db.add_sample(done, &sample(1, 100, 100)).unwrap();
        let mut s = db.get_ride(done).unwrap().unwrap().summary;
        s.ride_secs = 99;
        s.avg_watts = 77;
        db.finish_ride(done, &s).unwrap();
        let empty = db.start_ride(p, None, 2).unwrap().0;
        db.close_unfinished().unwrap();
        let d = db.get_ride(done).unwrap().unwrap().summary;
        assert_eq!((d.ride_secs, d.avg_watts), (99, 77));
        // A ride without samples is not kept.
        assert_eq!(db.get_ride(empty).unwrap(), None);
    }

    #[test]
    fn ended_at_set_by_finish_and_close_unfinished() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let r = db.start_ride(p, None, 1000).unwrap().0;
        db.add_sample(r, &sample(1, 0, 100)).unwrap();
        let mut s = db.get_ride(r).unwrap().unwrap().summary;
        assert_eq!(s.ended_at, None);
        s.ended_at = Some(1100);
        db.finish_ride(r, &s).unwrap();
        assert_eq!(db.get_ride(r).unwrap().unwrap().summary.ended_at, Some(1100));

        // Unfinished: the last sample's wall clock, else start + ride time.
        let a = db.start_ride(p, None, 2000).unwrap().0;
        db.add_sample(a, &Sample { at_unix_ms: Some(2_004_400), ..sample(4, 0, 100) }).unwrap();
        db.add_sample(a, &Sample { at_unix_ms: Some(2_005_600), ..sample(5, 0, 100) }).unwrap();
        let b = db.start_ride(p, None, 3000).unwrap().0;
        db.add_sample(b, &sample(7, 0, 100)).unwrap();
        db.close_unfinished().unwrap();
        assert_eq!(db.get_ride(a).unwrap().unwrap().summary.ended_at, Some(2005));
        assert_eq!(db.get_ride(b).unwrap().unwrap().summary.ended_at, Some(3007));
    }

    #[test]
    fn sample_wall_clock_roundtrip() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let r = db.start_ride(p, None, 1).unwrap().0;
        let s = Sample { at_unix_ms: Some(1_761_400_000_123), ..sample(1, 0, 100) };
        db.add_sample(r, &s).unwrap();
        db.add_sample(r, &sample(2, 0, 100)).unwrap();
        let got = db.get_ride(r).unwrap().unwrap().samples;
        assert_eq!(got, vec![s, sample(2, 0, 100)]);
    }

    #[test]
    fn delete_ride_and_set_started() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Ich").unwrap();
        let r = db.start_ride(p, None, 1).unwrap().0;
        db.set_ride_started(r, 42).unwrap();
        assert_eq!(db.get_ride(r).unwrap().unwrap().summary.started_at, 42);
        db.delete_ride(r).unwrap();
        assert_eq!(db.get_ride(r).unwrap(), None);
        assert!(db.list_rides(p).unwrap().is_empty());
    }

    fn person_with_weight(db: &Db, weight_kg: f64) -> i64 {
        let p = db.add_person("Bob").unwrap();
        let u = PersonUpdate { weight_kg: Some(weight_kg), ..PersonUpdate::default() };
        assert!(db.update_person(p, &u).unwrap());
        p
    }

    fn person_error(r: Result<bool>) -> PersonError {
        r.unwrap_err().downcast::<PersonError>().unwrap()
    }

    #[test]
    fn persons_have_default_profile() {
        let db = open_in_memory().unwrap();
        let id = db.add_person("Alice").unwrap();
        let p = &db.list_persons().unwrap()[0];
        assert_eq!((p.id, p.weight_kg, p.flat_watts), (id, 75.0, 70));
    }

    #[test]
    fn update_person_validates_and_keeps_missing_fields() {
        let db = open_in_memory().unwrap();
        let id = db.add_person("Bob").unwrap();
        let full = PersonUpdate { name: None, weight_kg: Some(82.5), flat_watts: Some(90), strap_id: None };
        assert!(db.update_person(id, &full).unwrap());
        let name_only = PersonUpdate { name: Some(" Alice ".into()), ..PersonUpdate::default() };
        assert!(db.update_person(id, &name_only).unwrap());
        assert_eq!(
            db.list_persons().unwrap(),
            vec![Person { id, name: "Alice".into(), weight_kg: 82.5, flat_watts: 90, strap_id: None }]
        );
        let weight = |w| PersonUpdate { weight_kg: Some(w), ..PersonUpdate::default() };
        assert_eq!(person_error(db.update_person(id, &weight(29.0))), PersonError::Weight);
        assert_eq!(person_error(db.update_person(id, &weight(251.0))), PersonError::Weight);
        assert_eq!(person_error(db.update_person(id, &weight(f64::NAN))), PersonError::Weight);
        let flat = PersonUpdate { flat_watts: Some(301), ..PersonUpdate::default() };
        assert_eq!(person_error(db.update_person(id, &flat)), PersonError::Flat);
        let empty = PersonUpdate { name: Some("  ".into()), ..PersonUpdate::default() };
        assert_eq!(person_error(db.update_person(id, &empty)), PersonError::EmptyName);
        assert_eq!(PersonError::Weight.to_string(), "Gewicht muss zwischen 30 und 250 kg liegen");
        // Nothing was changed by the rejected updates.
        assert_eq!(db.list_persons().unwrap()[0].weight_kg, 82.5);
        // The limits themselves are allowed.
        let edge = PersonUpdate { name: None, weight_kg: Some(30.0), flat_watts: Some(300), strap_id: None };
        assert!(db.update_person(id, &edge).unwrap());
        assert!(db.update_person(id, &weight(250.0)).unwrap());
        assert!(!db.update_person(id + 1, &full).unwrap());
    }

    #[test]
    fn ride_snapshot_and_gain_are_stored() {
        let db = open_in_memory().unwrap();
        let p = person_with_weight(&db, 85.0);
        let r = db.start_ride(p, None, 1000).unwrap().0;
        for secs in 1..=240 {
            db.add_sample(r, &sample(secs, 100, 170)).unwrap();
        }
        // finish_ride computes the gain itself and keeps the snapshot.
        let placeholder = RideSummary {
            weight_kg: 1.0,
            flat_watts: 1,
            elevation_gain_m: 0.0,
            ..db.get_ride(r).unwrap().unwrap().summary
        };
        db.finish_ride(r, &placeholder).unwrap();
        let d = db.get_ride(r).unwrap().unwrap();
        assert_eq!((d.summary.weight_kg, d.summary.flat_watts), (85.0, 70));
        assert!((d.summary.elevation_gain_m - 25.8).abs() <= 0.5, "{}", d.summary.elevation_gain_m);
        assert_eq!(db.list_rides(p).unwrap()[0], d.summary);
        assert_eq!(d.elevation_m.len(), d.samples.len());
        let last = f64::from(*d.elevation_m.last().unwrap());
        assert!((last - 25.8).abs() <= 0.5, "{last}");
        assert!((last - d.summary.elevation_gain_m).abs() < 1e-3);
        assert!(d.elevation_preview.is_none());
    }

    #[test]
    fn finish_ride_keeps_summary_when_samples_are_unreadable() {
        let db = open_in_memory().unwrap();
        let p = db.add_person("Bob").unwrap();
        let r = db.start_ride(p, None, 1000).unwrap().0;
        db.conn().execute_batch("DROP TABLE samples").unwrap();
        let s = RideSummary { avg_watts: 120, elevation_gain_m: 3.5, ..db.list_rides(p).unwrap()[0].clone() };
        db.finish_ride(r, &s).unwrap();
        let got = &db.list_rides(p).unwrap()[0];
        assert_eq!((got.finished, got.avg_watts, got.elevation_gain_m), (true, 120, 3.5));
        assert!(db.finish_ride(r + 1, &s).is_err());
    }

    #[test]
    fn profile_change_keeps_old_ride_elevation() {
        let db = open_in_memory().unwrap();
        let p = person_with_weight(&db, 85.0);
        let r = db.start_ride(p, None, 1000).unwrap().0;
        for secs in 1..=120 {
            db.add_sample(r, &sample(secs, 100, 170)).unwrap();
        }
        db.finish_ride(r, &db.get_ride(r).unwrap().unwrap().summary).unwrap();
        let before = db.get_ride(r).unwrap().unwrap();
        let lighter = PersonUpdate { weight_kg: Some(60.0), flat_watts: Some(50), name: None, strap_id: None };
        assert!(db.update_person(p, &lighter).unwrap());
        let after = db.get_ride(r).unwrap().unwrap();
        assert_eq!(after.summary.elevation_gain_m, before.summary.elevation_gain_m);
        assert_eq!(after.elevation_m, before.elevation_m);
        assert_eq!((after.summary.weight_kg, after.summary.flat_watts), (85.0, 70));
        // New rides take the new profile.
        let next = db.start_ride(p, None, 2000).unwrap().0;
        let s = db.get_ride(next).unwrap().unwrap().summary;
        assert_eq!((s.weight_kg, s.flat_watts), (60.0, 50));
    }

    #[test]
    fn close_unfinished_uses_the_ride_snapshot() {
        let db = open_in_memory().unwrap();
        let p = person_with_weight(&db, 85.0);
        let r = db.start_ride(p, None, 1000).unwrap().0;
        for secs in 1..=240 {
            db.add_sample(r, &sample(secs, 100, 170)).unwrap();
        }
        let lighter = PersonUpdate { weight_kg: Some(60.0), ..PersonUpdate::default() };
        assert!(db.update_person(p, &lighter).unwrap());
        db.close_unfinished().unwrap();
        let d = db.get_ride(r).unwrap().unwrap();
        let p85 = ElevationParams { weight_kg: 85.0, flat_watts: 70 };
        assert_eq!(d.summary.elevation_gain_m, summarize(&d.samples, &p85).elevation_gain_m);
        assert!((d.summary.elevation_gain_m - 25.8).abs() <= 0.5);
    }

    #[test]
    fn start_ride_returns_the_stored_snapshot() {
        let db = open_in_memory().unwrap();
        let p = person_with_weight(&db, 77.5);
        let u = PersonUpdate { flat_watts: Some(55), ..PersonUpdate::default() };
        assert!(db.update_person(p, &u).unwrap());
        let (id, params) = db.start_ride(p, None, 1).unwrap();
        assert_eq!(params, ElevationParams { weight_kg: 77.5, flat_watts: 55 });
        let stored: (f64, u16) = db
            .conn()
            .query_row("SELECT weight_kg, flat_watts FROM rides WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(stored, (params.weight_kg, params.flat_watts));
    }

    #[test]
    fn summarize_gain_follows_ride_time_order() {
        let p85 = ElevationParams { weight_kg: 85.0, flat_watts: 70 };
        let climb: Vec<Sample> = (1..=60).map(|i| sample(i, 0, 170)).chain((61..=120).map(|i| sample(i, 0, 0))).collect();
        let gain = summarize(&climb, &p85).elevation_gain_m;
        assert_eq!(gain, heights(&p85, climb.iter().map(|s| f64::from(s.status.actual_watts))).1);
        let mut shuffled = climb.clone();
        shuffled.reverse();
        assert_eq!(summarize(&shuffled, &p85).elevation_gain_m, gain);
        assert_eq!(summarize(&[], &p85).elevation_gain_m, 0.0);
    }

    #[test]
    fn migrates_v4_rides_with_default_elevation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        let watts: Vec<u16> = (0..300).map(|i| 60 + (i * 37 % 150) as u16).collect();
        {
            // The schema as it was at version 4.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(MIGRATE_V2).unwrap();
            conn.execute_batch(MIGRATE_V3).unwrap();
            conn.execute_batch(MIGRATE_V4).unwrap();
            conn.execute_batch(
                "INSERT INTO persons (name) VALUES ('Alt');
                 INSERT INTO rides (person_id, started_at, finished, ride_secs, avg_watts, max_watts, ended_at)
                     VALUES (1, 100, 1, 300, 100, 200, 400);
                 INSERT INTO rides (person_id, started_at, finished, ride_secs, avg_watts, max_watts, ended_at)
                     VALUES (1, 500, 1, 0, 0, 0, 500);
                 PRAGMA user_version = 4;",
            )
            .unwrap();
            // Inserted backwards to check the backfill follows the ride time.
            for (i, w) in watts.iter().enumerate().rev() {
                conn.execute(
                    "INSERT INTO samples (ride_id, ride_secs, pulse, rpm, speed_x10, distance,
                         status_target_watts, energy, elapsed_secs, actual_watts, target_watts, unix_ms)
                     VALUES (1, ?1, 0, 80, 360, 0, 100, 0, ?1, ?2, 100, NULL)",
                    params![i as u32 + 1, w],
                )
                .unwrap();
            }
        }
        let db = open(&path).unwrap();
        let version: i64 = db.conn().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let p = &db.list_persons().unwrap()[0];
        assert_eq!((p.weight_kg, p.flat_watts), (75.0, 70));
        let expected = heights(&ElevationParams::DEFAULT, watts.iter().map(|&w| f64::from(w))).1;
        assert!(expected > 1.0, "{expected}");
        let d = db.get_ride(1).unwrap().unwrap();
        assert_eq!((d.summary.weight_kg, d.summary.flat_watts), (75.0, 70));
        assert_eq!(d.summary.elevation_gain_m, expected);
        assert_eq!(d.elevation_m.len(), 300);
        let empty = db.get_ride(2).unwrap().unwrap();
        assert_eq!(empty.summary.elevation_gain_m, 0.0);
        assert!(empty.elevation_m.is_empty());
        // Nothing else was touched.
        assert_eq!((d.summary.ride_secs, d.summary.avg_watts, d.summary.max_watts), (300, 100, 200));
        drop(db);
        let db = open(&path).unwrap();
        assert_eq!(db.get_ride(1).unwrap().unwrap().summary.elevation_gain_m, expected);
    }

    #[test]
    fn migrates_v5_to_v6() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        {
            // The schema as it was at version 5.
            let conn = Connection::open(&path).unwrap();
            for sql in [SCHEMA_V1, MIGRATE_V2, MIGRATE_V3, MIGRATE_V4, MIGRATE_V5] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute_batch(
                "INSERT INTO persons (name) VALUES ('Bob');
                 INSERT INTO rides (person_id, started_at, finished, ride_secs, avg_watts, max_watts, ended_at)
                     VALUES (1, 100, 1, 1, 100, 100, 101);
                 INSERT INTO samples (ride_id, ride_secs, pulse, rpm, speed_x10, distance,
                     status_target_watts, energy, elapsed_secs, actual_watts, target_watts, unix_ms)
                     VALUES (1, 1, 90, 80, 360, 0, 100, 0, 1, 100, 100, NULL);
                 INSERT INTO recoveries (ride_id, ride_secs, pulse_start, difference, grade_x10, unix_ms)
                     VALUES (1, 1, 106, 6, 57, 5);
                 PRAGMA user_version = 5;",
            )
            .unwrap();
        }
        let db = open(&path).unwrap();
        let version: i64 = db.conn().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 6);
        assert_eq!(db.list_persons().unwrap()[0].strap_id, None);
        let d = db.get_ride(1).unwrap().unwrap();
        assert_eq!(d.samples[0].strap_pulse, None);
        assert_eq!(d.samples[0].status.pulse, 90);
        assert_eq!(d.recoveries[0].source, RecoverySource::Cockpit);
        assert!(db.list_straps().unwrap().is_empty());
    }

    fn strap_sample(secs: u32, cockpit: u16, strap: Option<u16>) -> Sample {
        Sample { strap_pulse: strap, ..sample(secs, cockpit, 100) }
    }

    #[test]
    fn stores_strap_pulse_and_source() {
        let db = open_in_memory().unwrap();
        let person = db.add_person("Bob").unwrap();
        let ride = db.start_ride(person, None, 1000).unwrap().0;
        db.add_sample(ride, &strap_sample(1, 0, Some(90))).unwrap();
        let strap = Recovery { source: RecoverySource::Strap, ..recovery(1) };
        db.add_recovery(ride, &strap).unwrap();
        let d = db.get_ride(ride).unwrap().unwrap();
        assert_eq!(d.samples[0].strap_pulse, Some(90));
        assert_eq!(d.recoveries, vec![strap]);
    }

    #[test]
    fn summary_uses_effective_pulse() {
        let samples = [strap_sample(1, 0, Some(100)), strap_sample(2, 80, None), strap_sample(3, 0, None)];
        let stats = summarize(&samples, &ElevationParams::DEFAULT);
        assert_eq!((stats.avg_pulse, stats.max_pulse), (Some(90), Some(100)));
        // The strap wins over the cockpit when both have a value.
        assert_eq!(strap_sample(1, 70, Some(95)).effective_pulse(), Some(95));
        assert_eq!(strap_sample(1, 70, Some(0)).effective_pulse(), Some(70));
    }

    #[test]
    fn strap_crud() {
        let db = open_in_memory().unwrap();
        let id = db.add_strap("aa:bb:cc:dd:ee:ff", "HRM-Dual").unwrap();
        assert_eq!(
            db.list_straps().unwrap(),
            vec![Strap { id, address: "AA:BB:CC:DD:EE:FF".into(), name: "HRM-Dual".into() }]
        );
        let dup = db.add_strap("AA:BB:CC:DD:EE:FF", "Noch einer").unwrap_err();
        assert_eq!(dup.downcast_ref::<StrapError>(), Some(&StrapError::Duplicate));
        assert!(db.rename_strap(id, " Bobs Gurt ").unwrap());
        assert_eq!(db.list_straps().unwrap()[0].name, "Bobs Gurt");
        let empty = db.rename_strap(id, "  ").unwrap_err();
        assert_eq!(empty.downcast_ref::<StrapError>(), Some(&StrapError::EmptyName));
        assert!(!db.rename_strap(999, "x").unwrap());
    }

    #[test]
    fn delete_strap_clears_assignment() {
        let db = open_in_memory().unwrap();
        let strap = db.add_strap("AA:BB:CC:DD:EE:FF", "HRM-Dual").unwrap();
        let bob = db.add_person("Bob").unwrap();
        assert!(db.update_person(bob, &PersonUpdate { strap_id: Some(Some(strap)), ..Default::default() }).unwrap());
        assert_eq!(db.list_persons().unwrap()[0].strap_id, Some(strap));
        let ride = db.start_ride(bob, None, 1000).unwrap().0;
        db.add_sample(ride, &strap_sample(1, 0, Some(90))).unwrap();
        assert!(db.delete_strap(strap).unwrap());
        assert_eq!(db.list_persons().unwrap()[0].strap_id, None);
        assert_eq!(db.get_ride(ride).unwrap().unwrap().samples[0].strap_pulse, Some(90));
        assert!(!db.delete_strap(strap).unwrap());
        // Clearing works too.
        let other = db.add_strap("11:22:33:44:55:66", "Zweiter").unwrap();
        db.update_person(bob, &PersonUpdate { strap_id: Some(Some(other)), ..Default::default() }).unwrap();
        db.update_person(bob, &PersonUpdate { strap_id: Some(None), ..Default::default() }).unwrap();
        assert_eq!(db.list_persons().unwrap()[0].strap_id, None);
    }

    #[test]
    fn assign_unknown_strap_fails() {
        let db = open_in_memory().unwrap();
        let bob = db.add_person("Bob").unwrap();
        let err = db
            .update_person(bob, &PersonUpdate { name: Some("Robert".into()), strap_id: Some(Some(999)), ..Default::default() })
            .unwrap_err();
        assert_eq!(err.downcast_ref::<StrapError>(), Some(&StrapError::Unknown));
        assert_eq!(db.list_persons().unwrap()[0].name, "Bob");
    }

    #[test]
    fn ride_detail_has_preview_for_programs() {
        let db = open_in_memory().unwrap();
        let p = person_with_weight(&db, 85.0);
        let four = db.list_programs().unwrap().into_iter().find(|p| p.name == "4 × 4").unwrap();
        let r = db.start_ride(p, Some(&four), 1).unwrap().0;
        let pv = db.get_ride(r).unwrap().unwrap().elevation_preview.expect("preview");
        assert!((120.0..=122.0).contains(&pv.gain_m), "gain {}", pv.gain_m);
        let free = db.start_ride(p, None, 2).unwrap().0;
        assert!(db.get_ride(free).unwrap().unwrap().elevation_preview.is_none());
    }

    #[test]
    fn v1_database_migrates_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        {
            // The schema as it was at version 1.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE persons (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                 CREATE TABLE programs (id INTEGER PRIMARY KEY, name TEXT NOT NULL, blocks TEXT NOT NULL);
                 CREATE TABLE rides (
                     id INTEGER PRIMARY KEY,
                     person_id INTEGER NOT NULL REFERENCES persons(id),
                     program_name TEXT,
                     program_json TEXT,
                     started_at INTEGER NOT NULL,
                     finished INTEGER NOT NULL DEFAULT 0,
                     ride_secs INTEGER NOT NULL DEFAULT 0,
                     avg_watts INTEGER NOT NULL DEFAULT 0,
                     max_watts INTEGER NOT NULL DEFAULT 0,
                     avg_pulse INTEGER,
                     max_pulse INTEGER,
                     distance_m REAL NOT NULL DEFAULT 0,
                     energy_kj REAL NOT NULL DEFAULT 0
                 );
                 CREATE INDEX rides_person ON rides(person_id, started_at);
                 CREATE TABLE samples (
                     ride_id INTEGER NOT NULL REFERENCES rides(id),
                     ride_secs INTEGER NOT NULL,
                     pulse INTEGER NOT NULL,
                     rpm INTEGER NOT NULL,
                     speed_x10 INTEGER NOT NULL,
                     distance INTEGER NOT NULL,
                     status_target_watts INTEGER NOT NULL,
                     energy INTEGER NOT NULL,
                     elapsed_secs INTEGER NOT NULL,
                     actual_watts INTEGER NOT NULL,
                     target_watts INTEGER NOT NULL,
                     UNIQUE (ride_id, ride_secs)
                 );
                 INSERT INTO persons (name) VALUES ('Alt');
                 INSERT INTO programs (name, blocks) VALUES ('Eigenes', '[]');
                 INSERT INTO rides (person_id, started_at, finished, ride_secs, avg_watts, max_watts)
                     VALUES (1, 500, 1, 2, 100, 100);
                 INSERT INTO samples VALUES (1, 1, 0, 80, 360, 0, 100, 0, 1, 100, 100);
                 INSERT INTO samples VALUES (1, 2, 0, 80, 360, 0, 100, 0, 2, 100, 100);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let db = open(&path).unwrap();
        let version: i64 = db.conn().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(db.list_persons().unwrap(), vec![Person { id: 1, name: "Alt".into(), weight_kg: 75.0, flat_watts: 70, strap_id: None }]);
        // No templates are added to an existing database.
        let programs = db.conn().query_row("SELECT count(*) FROM programs", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(programs, 1);
        let ride = db.get_ride(1).unwrap().unwrap();
        assert_eq!((ride.summary.started_at, ride.summary.ride_secs, ride.summary.ended_at), (500, 2, None));
        assert_eq!(ride.samples, vec![sample(1, 0, 100), sample(2, 0, 100)]);
        // Both steps ran: cadence is backfilled from the v1 samples (rpm 80).
        assert_eq!((ride.summary.avg_rpm, ride.summary.max_rpm), (Some(80), Some(80)));
        // The new columns work.
        let r = db.start_ride(1, None, 600).unwrap().0;
        db.add_sample(r, &Sample { at_unix_ms: Some(601_000), ..sample(1, 0, 100) }).unwrap();
        drop(db);
        let db = open(&path).unwrap();
        assert_eq!(db.get_ride(r).unwrap().unwrap().summary.ended_at, Some(601));
    }

    fn names(db: &Db) -> Vec<String> {
        db.list_persons().unwrap().into_iter().map(|p| p.name).collect()
    }

    /// Bob with one finished ride of 3 samples, and Alice, backed up to `dir/b.db`.
    fn bob_and_alice_backup(dir: &Path) -> (std::path::PathBuf, i64) {
        let db = open(&dir.join("a.db")).unwrap();
        let bob = db.add_person("Bob").unwrap();
        db.add_person("Alice").unwrap();
        let r = ride_with_samples(&db, bob, 3);
        let mut s = db.get_ride(r).unwrap().unwrap().summary;
        s.ride_secs = 3;
        db.finish_ride(r, &s).unwrap();
        let path = dir.join("b.db");
        db.backup_to(&path).unwrap();
        (path, bob)
    }

    #[test]
    fn backup_roundtrip_restores_everything() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, bob) = bob_and_alice_backup(dir.path());
        let target = open(&dir.path().join("t.db")).unwrap();
        target.add_person("Carol").unwrap();
        let info = target.restore_from(&backup).unwrap();
        assert_eq!(info, BackupInfo { schema_version: SCHEMA_VERSION, persons: 2, programs: 4, rides: 1 });
        assert_eq!(names(&target), ["Bob", "Alice"]);
        let rides = target.list_rides(bob).unwrap();
        assert_eq!(rides.len(), 1);
        assert_eq!(target.get_ride(rides[0].id).unwrap().unwrap().samples.len(), 3);
    }

    #[test]
    fn inspect_backup_counts_without_changing() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, _) = bob_and_alice_backup(dir.path());
        let before = std::fs::read(&backup).unwrap();
        let info = inspect_backup(&backup).unwrap();
        assert_eq!(info, BackupInfo { schema_version: SCHEMA_VERSION, persons: 2, programs: 4, rides: 1 });
        assert_eq!(std::fs::read(&backup).unwrap(), before);
    }

    #[test]
    fn restore_rejects_garbage_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let target = open(&dir.path().join("t.db")).unwrap();
        target.add_person("Bob").unwrap();
        let text = dir.path().join("text.db");
        std::fs::write(&text, b"hello").unwrap();
        let empty = dir.path().join("empty.db");
        std::fs::write(&empty, b"").unwrap();
        let foreign = dir.path().join("foreign.db");
        Connection::open(&foreign).unwrap().execute_batch("CREATE TABLE foo (x)").unwrap();
        for file in [&text, &empty, &foreign] {
            assert!(matches!(inspect_backup(file), Err(RestoreError::Invalid)), "{}", file.display());
            assert!(matches!(target.restore_from(file), Err(RestoreError::Invalid)), "{}", file.display());
        }
        assert_eq!(names(&target), ["Bob"]);
    }

    #[test]
    fn restore_rejects_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, _) = bob_and_alice_backup(dir.path());
        Connection::open(&backup).unwrap().pragma_update(None, "user_version", 99).unwrap();
        let target = open(&dir.path().join("t.db")).unwrap();
        target.add_person("Carol").unwrap();
        assert!(matches!(target.restore_from(&backup), Err(RestoreError::TooNew)));
        assert_eq!(names(&target), ["Carol"]);
    }

    #[test]
    fn restore_migrates_old_backup() {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("v1.db");
        {
            let conn = Connection::open(&backup).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(
                "INSERT INTO persons (name) VALUES ('Bob');
                 INSERT INTO rides (person_id, started_at, finished, ride_secs, avg_watts, max_watts)
                     VALUES (1, 100, 1, 5, 100, 100);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let target = open(&dir.path().join("t.db")).unwrap();
        let info = target.restore_from(&backup).unwrap();
        assert_eq!(info.schema_version, SCHEMA_VERSION);
        let version: i64 = target.conn().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(names(&target), ["Bob"]);
        let bob = target.list_persons().unwrap()[0].id;
        assert_eq!(target.list_rides(bob).unwrap().len(), 1);
    }

    #[test]
    fn restore_rejects_foreign_schema_objects() {
        let dir = tempfile::tempdir().unwrap();
        let target = open(&dir.path().join("t.db")).unwrap();
        target.add_person("Carol").unwrap();
        let extras = [
            "CREATE TRIGGER wipe AFTER INSERT ON samples BEGIN DELETE FROM persons; END",
            "CREATE VIEW v AS SELECT * FROM persons",
            "CREATE TABLE extra (x)",
            "ALTER TABLE persons ADD COLUMN extra TEXT",
            "ALTER TABLE rides DROP COLUMN elevation_gain_m",
            // Same columns, other rules: only the definition differs.
            "PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON;
             ALTER TABLE persons RENAME TO old_persons;
             CREATE TABLE persons (id INTEGER PRIMARY KEY, name TEXT NOT NULL CHECK (length(name) > 0),
                 weight_kg REAL NOT NULL DEFAULT 75, flat_watts INTEGER NOT NULL DEFAULT 70,
                 strap_id INTEGER REFERENCES straps(id));
             INSERT INTO persons SELECT * FROM old_persons;
             DROP TABLE old_persons;",
        ];
        for (i, sql) in extras.iter().enumerate() {
            let sub = dir.path().join(format!("{i}"));
            std::fs::create_dir(&sub).unwrap();
            let (backup, _) = bob_and_alice_backup(&sub);
            Connection::open(&backup).unwrap().execute_batch(sql).unwrap();
            assert!(matches!(target.restore_from(&backup), Err(RestoreError::Invalid)), "{sql}");
        }
        assert_eq!(names(&target), ["Carol"]);
    }

    #[test]
    fn restore_rejects_broken_references() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, _) = bob_and_alice_backup(dir.path());
        Connection::open(&backup).unwrap().execute_batch("PRAGMA foreign_keys = OFF; DELETE FROM persons;").unwrap();
        let target = open(&dir.path().join("t.db")).unwrap();
        assert!(matches!(target.restore_from(&backup), Err(RestoreError::Invalid)));
    }

    #[test]
    fn scratch_copy_is_never_created() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone.db");
        assert!(matches!(open_scratch(&gone), Err(RestoreError::Invalid)));
        assert!(!gone.exists());
    }

    #[test]
    fn restore_closes_unfinished_ride() {
        let dir = tempfile::tempdir().unwrap();
        let source = open(&dir.path().join("a.db")).unwrap();
        let bob = source.add_person("Bob").unwrap();
        ride_with_samples(&source, bob, 4);
        let backup = dir.path().join("b.db");
        source.backup_to(&backup).unwrap();
        let target = open(&dir.path().join("t.db")).unwrap();
        target.restore_from(&backup).unwrap();
        let rides = target.list_rides(bob).unwrap();
        assert_eq!(rides.len(), 1);
        assert!(rides[0].finished);
        assert_eq!(rides[0].ride_secs, 4);
    }
}
