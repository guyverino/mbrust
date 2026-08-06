//! Чтение `app_settings` с TTL-кэшем (SQLite).
//!
//! Значение читается из БД не чаще раза в [`CACHE_TTL`]; между обращениями —
//! из кэша. Позволяет менять параметры в рантайме (UPDATE app_settings) без
//! рестарта. Открывает короткое соединение к файлу БД для refresh.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_secs(60);

struct Cache {
    values: HashMap<String, String>,
    loaded_at: Instant,
    db_path: Option<String>,
}

impl Cache {
    fn empty() -> Self {
        Self {
            values: HashMap::new(),
            loaded_at: Instant::now() - Duration::from_secs(3600),
            db_path: None,
        }
    }
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| Mutex::new(Cache::empty()))
}

/// Один раз при старте — после этого get_* смогут лениво открывать соединение.
pub fn init(db_path: impl Into<String>) {
    let mut c = cache().lock().expect("settings cache lock");
    c.db_path = Some(db_path.into());
}

fn refresh_if_due(c: &mut Cache) {
    if c.loaded_at.elapsed() < CACHE_TTL && !c.values.is_empty() {
        return;
    }
    let Some(path) = c.db_path.clone() else { return };

    let conn = match crate::db::open(&path) {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!("settings refresh: open failed: {e}");
            c.loaded_at = Instant::now();
            return;
        }
    };
    let load = || -> rusqlite::Result<HashMap<String, String>> {
        let mut stmt = conn.prepare("SELECT key, value FROM app_settings")?;
        let rows =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut map = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            map.insert(k, v);
        }
        Ok(map)
    };
    match load() {
        Ok(map) => {
            c.values = map;
            c.loaded_at = Instant::now();
        }
        Err(e) => {
            tracing::warn!("settings refresh: query failed: {e}");
            c.loaded_at = Instant::now();
        }
    }
}

pub fn get(key: &str) -> Option<String> {
    let mut c = cache().lock().expect("settings cache lock");
    refresh_if_due(&mut c);
    c.values.get(key).cloned()
}

pub fn get_int(key: &str, default: i64) -> i64 {
    get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn get_bool(key: &str, default: bool) -> bool {
    get(key)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(default)
}

pub fn poll_interval() -> Duration {
    Duration::from_millis(get_int("event_poll_ms", 500).max(50) as u64)
}

pub fn connect_timeout() -> Duration {
    Duration::from_secs(get_int("connect_timeout_secs", 15).max(1) as u64)
}
