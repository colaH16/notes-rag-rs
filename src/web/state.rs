//! Port of `public_search.State`: persistent, server-wide budgets, traffic
//! limits and the excerpt cache in the existing SQLite file.
//!
//! The DDL and statements are byte-identical to Python, so the Rust web role
//! continues the same `/state/search.db` without resetting budgets, and a
//! rollback to Python reads what Rust wrote. Attempts are reserved *before*
//! upstream calls and failures still consume budget. Each operation opens its
//! own connection (`sqlite3.connect(timeout=3)`) and uses `BEGIN IMMEDIATE`
//! where Python did, so concurrent processes cannot over-reserve.

use crate::fault::{Fault, Result};
use crate::py::json as pyjson;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const SEARCH_CACHE_TTL_SECONDS: i64 = 3 * 60 * 60;

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    })
}

#[derive(Clone)]
pub struct State {
    path: PathBuf,
    daily: i64,
    hourly: i64,
    pub clock: Clock,
}

/// SQLite errors (locks, I/O) are an ordinary `Exception` in Python: callers
/// fail closed with 503 and never call upstream without a reservation.
fn sql(_: rusqlite::Error) -> Fault {
    Fault::Other("state database failure")
}

impl State {
    pub fn new(path: impl Into<PathBuf>, daily: i64, hourly: i64, clock: Clock) -> Result<Self> {
        if daily < 1 || hourly < 1 {
            return Err(Fault::Value("Positive budgets required"));
        }
        let state = Self {
            path: path.into(),
            daily,
            hourly,
            clock,
        };
        let db = state.connect()?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS attempts(at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS attempts_at ON attempts(at);
             CREATE TABLE IF NOT EXISTS cache(key TEXT PRIMARY KEY, expires INTEGER, body TEXT);
             CREATE TABLE IF NOT EXISTS traffic_v2(bucket TEXT NOT NULL, at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS traffic_v2_at ON traffic_v2(at);",
        )
        .map_err(sql)?;
        Ok(state)
    }

    pub fn connect(&self) -> Result<Connection> {
        let db = Connection::open(&self.path).map_err(sql)?;
        db.busy_timeout(Duration::from_secs(3)).map_err(sql)?;
        Ok(db)
    }

    fn now(&self) -> i64 {
        (self.clock)().floor() as i64
    }

    /// Run `work` inside `BEGIN IMMEDIATE ... COMMIT` (rollback on error/drop).
    pub fn immediate<T>(&self, work: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let db = self.connect()?;
        db.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
        let value = work(&db)?;
        db.execute_batch("COMMIT").map_err(sql)?;
        Ok(value)
    }

    /// Revoke cached excerpts on a policy change without touching budgets.
    pub fn activate_policy(&self, revision: &str) -> Result<()> {
        self.immediate(|db| {
            db.execute(
                "CREATE TABLE IF NOT EXISTS policy(singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision TEXT)",
                [],
            )
            .map_err(sql)?;
            let current: Option<Option<String>> = db
                .query_row("SELECT revision FROM policy WHERE singleton=1", [], |row| row.get(0))
                .optional()
                .map_err(sql)?;
            if current != Some(Some(revision.to_string())) {
                db.execute("DELETE FROM cache", []).map_err(sql)?;
                db.execute("INSERT OR REPLACE INTO policy VALUES(1, ?)", params![revision]).map_err(sql)?;
            }
            Ok(())
        })
    }

    /// Server-wide per-minute request limits by bucket.
    pub fn traffic(&self, bucket: &str) -> Result<()> {
        let limit = match bucket {
            "public" | "owner" => 60,
            "public-assets" | "owner-assets" => 120,
            "auth-login" | "auth-callback" => 30,
            _ => return Err(Fault::Value("Invalid traffic bucket")),
        };
        let now = self.now();
        self.immediate(|db| {
            db.execute("DELETE FROM traffic_v2 WHERE at<=?", params![now - 60])
                .map_err(sql)?;
            let count: i64 = db
                .query_row(
                    "SELECT count(*) FROM traffic_v2 WHERE bucket=?",
                    params![bucket],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if count >= limit {
                return Err(Fault::denied(
                    "요청이 많습니다. 잠시 후 다시 검색해 주세요.",
                ));
            }
            db.execute("INSERT INTO traffic_v2 VALUES(?,?)", params![bucket, now])
                .map_err(sql)?;
            Ok(())
        })
    }

    pub fn cached(&self, key: &str) -> Result<Option<Value>> {
        let db = self.connect()?;
        let body: Option<String> = db
            .query_row(
                "SELECT body FROM cache WHERE key=? AND expires>?",
                params![key, self.now()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        body.map(|b| serde_json::from_str(&b).map_err(|_| Fault::Value("invalid cached JSON")))
            .transpose()
    }

    /// Reserve one paid upstream attempt (daily UTC budget + rolling hour).
    pub fn reserve(&self) -> Result<()> {
        let now = self.now();
        let midnight = now.div_euclid(86400) * 86400;
        self.immediate(|db| {
            db.execute(
                "DELETE FROM attempts WHERE at<?",
                params![midnight.min(now - 3600)],
            )
            .map_err(sql)?;
            let daily: i64 = db
                .query_row(
                    "SELECT count(*) FROM attempts WHERE at>=?",
                    params![midnight],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if daily >= self.daily {
                return Err(Fault::Denied {
                    message: "오늘의 새 검색 한도에 도달했습니다. 잠시 쉬어갑니다.",
                    retry: (midnight + 86400 - now) as u64,
                });
            }
            let hourly: i64 = db
                .query_row(
                    "SELECT count(*) FROM attempts WHERE at>?",
                    params![now - 3600],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if hourly >= self.hourly {
                return Err(Fault::Denied {
                    message: "시간당 새 검색 한도에 도달했습니다. 나중에 다시 검색해 주세요.",
                    retry: 3600,
                });
            }
            db.execute("INSERT INTO attempts VALUES(?)", params![now])
                .map_err(sql)?;
            Ok(())
        })
    }

    pub fn save(&self, key: &str, body: &Value) -> Result<()> {
        let now = self.now();
        let db = self.connect()?;
        // Python's implicit (deferred) transaction around the three statements.
        db.execute_batch("BEGIN").map_err(sql)?;
        db.execute("DELETE FROM cache WHERE expires<=?", params![now])
            .map_err(sql)?;
        db.execute(
            "INSERT OR REPLACE INTO cache VALUES(?,?,?)",
            params![
                key,
                now + SEARCH_CACHE_TTL_SECONDS,
                pyjson::dumps_ascii(body)
            ],
        )
        .map_err(sql)?;
        db.execute("DELETE FROM cache WHERE key NOT IN (SELECT key FROM cache ORDER BY expires DESC LIMIT 256)", [])
            .map_err(sql)?;
        db.execute_batch("COMMIT").map_err(sql)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn fixture(daily: i64, hourly: i64) -> (tempfile::TempDir, State, Arc<AtomicI64>) {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(2_000_000_000));
        let clock_now = now.clone();
        let state = State::new(
            dir.path().join("state.db"),
            daily,
            hourly,
            Arc::new(move || clock_now.load(Ordering::SeqCst) as f64),
        )
        .unwrap();
        (dir, state, now)
    }

    #[test]
    fn budget_persists_across_restart_and_windows() {
        let (dir, state, now) = fixture(3, 2);
        state.reserve().unwrap();
        state.reserve().unwrap();
        assert!(matches!(
            state.reserve(),
            Err(Fault::Denied { retry: 3600, .. })
        ));
        let restarted = State::new(dir.path().join("state.db"), 3, 2, state.clock.clone()).unwrap();
        assert!(restarted.reserve().is_err());
        now.fetch_add(3601, Ordering::SeqCst);
        restarted.reserve().unwrap();
        now.fetch_add(3601, Ordering::SeqCst);
        // 2_000_007_202 is still the same UTC day: the daily budget (3) is exhausted.
        assert!(
            matches!(restarted.reserve(), Err(Fault::Denied { message, .. }) if message.contains("오늘"))
        );
    }

    #[test]
    fn cache_expiry_and_policy_rotation_keep_budget() {
        let (_dir, state, now) = fixture(3, 3);
        state.save("old", &json!({"results": ["private"]})).unwrap();
        state.reserve().unwrap();
        state.activate_policy(&"a".repeat(64)).unwrap();
        assert!(state.cached("old").unwrap().is_none());
        state.reserve().unwrap();
        state.save("new", &json!({"results": []})).unwrap();
        state.activate_policy(&"a".repeat(64)).unwrap();
        assert!(state.cached("new").unwrap().is_some());
        now.fetch_add(SEARCH_CACHE_TTL_SECONDS, Ordering::SeqCst);
        assert!(state.cached("new").unwrap().is_none());
        state.activate_policy(&"b".repeat(64)).unwrap();
        state.reserve().unwrap();
        assert!(state.reserve().is_err());
    }

    #[test]
    fn traffic_buckets() {
        let (_dir, state, now) = fixture(3, 3);
        for _ in 0..30 {
            state.traffic("auth-login").unwrap();
        }
        assert!(matches!(
            state.traffic("auth-login"),
            Err(Fault::Denied { retry: 60, .. })
        ));
        state.traffic("public").unwrap();
        assert!(state.traffic("bogus").is_err());
        now.fetch_add(61, Ordering::SeqCst);
        state.traffic("auth-login").unwrap();
    }

    #[test]
    fn concurrent_reservations_never_exceed_budget() {
        let (dir, _state, _) = fixture(3, 3);
        let path = dir.path().join("state.db");
        let handles: Vec<_> = (0..12)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    State::new(path, 3, 3, system_clock())
                        .unwrap()
                        .reserve()
                        .is_ok()
                })
            })
            .collect();
        let granted = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(granted, 3);
    }
}
