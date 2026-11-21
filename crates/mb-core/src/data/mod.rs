//! Data engine: parallel copy, chunked compare, staging sync.

pub mod compare;
pub mod copy;
pub mod sync;

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::ident::quote_ident;
use crate::model::{QName, TableDef};

/// First OID assigned to user objects; anything below is a built-in type.
pub const FIRST_NORMAL_OID: u32 = 16384;

/// Columns copied between two versions of a table (by name, source order),
/// excluding columns that are generated on the target.
pub fn copy_columns(src: &TableDef, tgt: &TableDef) -> (Vec<String>, Vec<String>) {
    let mut cols = Vec::new();
    let mut warnings = Vec::new();
    for c in &src.columns {
        match tgt.column(&c.name) {
            Some(tc) if tc.generated.is_some() => {}
            Some(_) if c.generated.is_some() => {}
            Some(_) => cols.push(c.name.clone()),
            None => warnings.push(format!("{}: column '{}' missing on target, skipped", src.name, c.name)),
        }
    }
    for tc in &tgt.columns {
        if src.column(&tc.name).is_none() && tc.not_null && tc.default.is_none() && tc.identity.is_none() && tc.generated.is_none() {
            warnings.push(format!(
                "{}: target column '{}' is NOT NULL without default and missing in source — copy will fail",
                tgt.name, tc.name
            ));
        }
    }
    (cols, warnings)
}

/// Binary COPY is safe when every column has the same built-in type on both sides.
pub fn binary_safe(src: &TableDef, tgt: &TableDef, cols: &[String]) -> bool {
    cols.iter().all(|n| match (src.column(n), tgt.column(n)) {
        (Some(a), Some(b)) => {
            a.data_type == b.data_type && a.type_oid > 0 && a.type_oid < FIRST_NORMAL_OID && b.type_oid < FIRST_NORMAL_OID
        }
        _ => false,
    })
}

pub fn col_list(cols: &[String]) -> String {
    cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ")
}

/// Live state of one table inside a data job (read by the UI each frame).
#[derive(Debug)]
pub struct TableProgress {
    pub table: QName,
    pub est_rows: u64,
    pub est_bytes: u64,
    pub state: AtomicU8,
    pub rows: AtomicU64,
    pub bytes: AtomicU64,
    pub parts_total: AtomicU32,
    pub parts_done: AtomicU32,
    pub inserted: AtomicU64,
    pub updated: AtomicU64,
    pub deleted: AtomicU64,
    pub message: Mutex<Option<String>>,
    started: Mutex<Option<Instant>>,
    finished: Mutex<Option<Instant>>,
}

pub mod state {
    pub const QUEUED: u8 = 0;
    pub const RUNNING: u8 = 1;
    pub const DONE: u8 = 2;
    pub const FAILED: u8 = 3;
    pub const SKIPPED: u8 = 4;
    pub const CANCELLED: u8 = 5;

    pub fn label(s: u8) -> &'static str {
        match s {
            QUEUED => "queued",
            RUNNING => "running",
            DONE => "done",
            FAILED => "failed",
            SKIPPED => "skipped",
            CANCELLED => "cancelled",
            _ => "?",
        }
    }
}

impl TableProgress {
    pub fn new(table: QName, est_rows: i64, est_bytes: i64) -> Self {
        Self {
            table,
            est_rows: est_rows.max(0) as u64,
            est_bytes: est_bytes.max(0) as u64,
            state: AtomicU8::new(state::QUEUED),
            rows: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            parts_total: AtomicU32::new(1),
            parts_done: AtomicU32::new(0),
            inserted: AtomicU64::new(0),
            updated: AtomicU64::new(0),
            deleted: AtomicU64::new(0),
            message: Mutex::new(None),
            started: Mutex::new(None),
            finished: Mutex::new(None),
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    pub fn mark_running(&self) {
        let mut s = self.started.lock().unwrap();
        if s.is_none() {
            *s = Some(Instant::now());
        }
        let _ = self.state.compare_exchange(state::QUEUED, state::RUNNING, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn finish(&self, st: u8, msg: Option<String>) {
        self.state.store(st, Ordering::Relaxed);
        *self.finished.lock().unwrap() = Some(Instant::now());
        if msg.is_some() {
            *self.message.lock().unwrap() = msg;
        }
    }

    pub fn set_message(&self, msg: impl Into<String>) {
        *self.message.lock().unwrap() = Some(msg.into());
    }

    pub fn message(&self) -> Option<String> {
        self.message.lock().unwrap().clone()
    }

    pub fn elapsed(&self) -> Duration {
        let s = *self.started.lock().unwrap();
        let f = *self.finished.lock().unwrap();
        match (s, f) {
            (Some(s), Some(f)) => f - s,
            (Some(s), None) => s.elapsed(),
            _ => Duration::ZERO,
        }
    }

    /// Rows done, estimating in-flight progress from bytes when the exact count isn't known yet.
    pub fn rows_estimate(&self) -> u64 {
        let rows = self.rows.load(Ordering::Relaxed);
        if self.state() == state::DONE || self.est_bytes == 0 {
            return rows;
        }
        let bytes = self.bytes.load(Ordering::Relaxed) as f64;
        let est = (bytes / self.est_bytes as f64 * self.est_rows as f64) as u64;
        rows.max(est.min(self.est_rows))
    }
}
