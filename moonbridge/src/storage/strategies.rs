//! Запись StrategySnapshot в `strategies` + `strategy_versions` (SQLite).
//!
//! UPSERT в `strategies` по (server_id, moonbot_id). Новая строка в
//! `strategy_versions` — только если изменился КОНТЕНТ стратегии (порт логики
//! `mb_ai` `is_content_identical`): сравниваем новый дамп полей с последней
//! версией, ВЫКИНУВ «косметические» поля из `strategies_ignore_keys` (Active,
//! LastEditDate, telegram/звук/цвета/чарты/лог/comment). Так правка реальных
//! торговых полей → версия, а изменение статуса/оформления → нет. (LastEditDate
//! в игноре, поэтому даже без его бампа реальная правка даст версию.)
//!
//! `moonbot_id` — signed int64 (`to_signed_int64`), как ядро шлёт StrategyID в
//! report-SQL ордера (иначе join `orders.strategyid = strategies.moonbot_id`
//! ломается). raw_data — JSON-текст полного дампа полей.

use std::collections::HashSet;

use moonproto::{FieldValue, StrategySnapshot};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value};

fn to_signed_int64(v: u64) -> i64 {
    v as i64
}

pub fn upsert_snapshot(
    conn: &Connection,
    server_id: i32,
    snap: &StrategySnapshot,
    checked: bool,
    ignore: &HashSet<String>,
) -> anyhow::Result<i64> {
    let name = extract_string(snap, "StrategyName").unwrap_or_default();
    let signal_type = extract_string(snap, "SignalType");
    let strategy_ver = snap.strategy_ver;
    let last_date_ms = snap.last_date as i64;
    let moonbot_id = to_signed_int64(snap.strategy_id);

    conn.execute(
        "INSERT INTO strategies (
            server_id, moonbot_id, name, signal_type, checked, strategy_ver, last_date_ms, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CURRENT_TIMESTAMP)
         ON CONFLICT (server_id, moonbot_id) DO UPDATE SET
            name         = excluded.name,
            signal_type  = COALESCE(excluded.signal_type, strategies.signal_type),
            checked      = excluded.checked,
            strategy_ver = excluded.strategy_ver,
            last_date_ms = excluded.last_date_ms,
            updated_at   = CURRENT_TIMESTAMP",
        rusqlite::params![
            server_id, moonbot_id, name, signal_type, checked, strategy_ver, last_date_ms
        ],
    )?;

    let strategy_id: i64 = conn.query_row(
        "SELECT id FROM strategies WHERE server_id = ?1 AND moonbot_id = ?2",
        rusqlite::params![server_id, moonbot_id],
        |r| r.get(0),
    )?;

    // Версионирование по контенту (без косметических полей).
    let new_map = fields_to_json(snap);

    let prev_raw: Option<String> = conn
        .query_row(
            "SELECT raw_data FROM strategy_versions \
             WHERE strategy_id = ?1 ORDER BY version_date DESC LIMIT 1",
            rusqlite::params![strategy_id],
            |r| r.get(0),
        )
        .optional()?;

    let needs_new_version = match &prev_raw {
        None => true,
        Some(prev) => content_differs(prev, &new_map, ignore),
    };

    if needs_new_version {
        let raw_json = serde_json::to_string(&Value::Object(new_map))?;
        // Миллисекундный version_date — чтобы не ловить коллизию UNIQUE при
        // двух изменениях в одну секунду.
        conn.execute(
            "INSERT OR IGNORE INTO strategy_versions (strategy_id, version_date, raw_data) \
             VALUES (?1, strftime('%Y-%m-%d %H:%M:%f','now'), ?2)",
            rusqlite::params![strategy_id, raw_json],
        )?;
    }
    Ok(strategy_id)
}

/// Разобрать CSV-настройку `strategies_ignore_keys` в множество.
pub fn ignore_keys_from_csv(csv: &str) -> HashSet<String> {
    csv.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ── Helpers ──────────────────────────────────────────────────────

/// Отличается ли контент (после удаления игнор-полей и наших `__`-метаполей).
fn content_differs(prev_raw: &str, new_map: &Map<String, Value>, ignore: &HashSet<String>) -> bool {
    let prev: Value = match serde_json::from_str(prev_raw) {
        Ok(v) => v,
        Err(_) => return true,
    };
    let Some(prev_map) = prev.as_object() else {
        return true;
    };
    strip(prev_map, ignore) != strip(new_map, ignore)
}

/// Копия map без игнор-полей и без синтетических `__`-ключей.
fn strip(m: &Map<String, Value>, ignore: &HashSet<String>) -> Map<String, Value> {
    m.iter()
        .filter(|(k, _)| !k.starts_with("__") && !ignore.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn extract_string(snap: &StrategySnapshot, name: &str) -> Option<String> {
    snap.fields.get(name).and_then(|v| match v {
        FieldValue::String(s) => Some(s.clone()),
        _ => None,
    })
}

fn fields_to_json(snap: &StrategySnapshot) -> Map<String, Value> {
    let mut out = Map::new();
    for (name, value) in snap.fields.iter() {
        out.insert(name.to_string(), field_to_json(value));
    }
    out.insert("__strategy_ver".to_string(), Value::from(snap.strategy_ver));
    out.insert("__path".to_string(), Value::from(snap.path.to_string()));
    out
}

fn field_to_json(v: &FieldValue) -> Value {
    match v {
        FieldValue::Bool(b) => Value::from(*b),
        FieldValue::Int32(x) => Value::from(*x),
        FieldValue::Int64(x) => Value::from(*x),
        FieldValue::Double(x) => Value::from(*x),
        FieldValue::Single(x) => Value::from(*x as f64),
        FieldValue::String(s) => Value::from(s.clone()),
        FieldValue::Byte(x) => Value::from(*x),
        FieldValue::Word(x) => Value::from(*x),
        FieldValue::UInt32(x) => Value::from(*x),
        FieldValue::UInt64(x) => Value::from(*x),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig() -> HashSet<String> {
        ignore_keys_from_csv("Active,LastEditDate,Comment")
    }

    #[test]
    fn same_content_no_version() {
        let prev = r#"{"TakeProfit":5,"Active":"YES","Comment":"a","__strategy_ver":1}"#;
        let mut new = Map::new();
        new.insert("TakeProfit".into(), Value::from(5));
        new.insert("Active".into(), Value::from("NO")); // косметика
        new.insert("Comment".into(), Value::from("b")); // косметика
        new.insert("__strategy_ver".into(), Value::from(2)); // мета
        assert!(!content_differs(prev, &new, &ig()));
    }

    #[test]
    fn real_change_makes_version() {
        let prev = r#"{"TakeProfit":5,"Active":"YES"}"#;
        let mut new = Map::new();
        new.insert("TakeProfit".into(), Value::from(7)); // реальная правка
        new.insert("Active".into(), Value::from("YES"));
        assert!(content_differs(prev, &new, &ig()));
    }
}
