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

/// UPSERT строки стратегии. `last_date_ms` идёт через COALESCE, а не присваивается:
/// если оба источника даты правки оказались нечитаемыми (`last_edit_ms` вернул
/// `None`), прежнее известное значение надо сохранить, а не затереть NULL-ом.
/// `signal_type` — по той же причине.
const UPSERT_STRATEGY_SQL: &str = "INSERT INTO strategies (
        server_id, moonbot_id, name, signal_type, checked, strategy_ver, last_date_ms, updated_at
     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CURRENT_TIMESTAMP)
     ON CONFLICT (server_id, moonbot_id) DO UPDATE SET
        name         = excluded.name,
        signal_type  = COALESCE(excluded.signal_type, strategies.signal_type),
        checked      = excluded.checked,
        strategy_ver = excluded.strategy_ver,
        last_date_ms = COALESCE(excluded.last_date_ms, strategies.last_date_ms),
        updated_at   = CURRENT_TIMESTAMP";

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
    let last_date_ms = last_edit_ms(snap.last_date, extract_string(snap, "LastEditDate").as_deref());
    let moonbot_id = to_signed_int64(snap.strategy_id);

    conn.execute(
        UPSERT_STRATEGY_SQL,
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

/// Правдоподобный диапазон `LastEditDate` в unix-мс: [2015-01-01, 2100-01-01).
/// Всё, что вне, — мусор, а не дата.
const LAST_DATE_MIN_MS: i64 = 1_420_070_400_000;
const LAST_DATE_MAX_MS: i64 = 4_102_444_800_000;

/// Дата последней правки стратегии в unix-мс.
///
/// По контракту MoonProto (`docs/strats.md`: «`last_date` is the exact
/// Unix-millisecond value») источник — заголовок снапшота. Но ядро шлёт туда
/// мусор для части стратегий (наблюдалось два упакованных f32 в одном u64,
/// напр. 4937127386796687413), и такое значение молча ломает любой гейт вида
/// «стратегия давно не правилась» — health, generator, отбор кандидатов.
/// Поэтому заголовок принимается только в правдоподобном диапазоне, иначе
/// берётся текстовое поле `LastEditDate` из самого дампа (формат ядра
/// `YYYY-MM-DD HH:MM`, UTC — сверено с валидными заголовками: 339 из 400 пар
/// совпадают с точностью до минуты, расхождения — там, где дамп устарел).
/// Ни один источник не читается → `None`, и UPSERT сохранит прежнее значение.
///
/// Точность: заголовок даёт миллисекунды, запасной путь — только минуты. Поэтому у
/// стратегии с «мигающим» заголовком значение может скакнуть назад в пределах минуты.
/// Потребители (`давно ли правилась`) работают на масштабе часов и дней, так что
/// монотонность здесь не навязывается: MAX(старое, новое) заморозил бы поле навсегда,
/// если бы в диапазон один раз попал мусор, — это хуже, чем джиттер в 59 секунд.
fn last_edit_ms(header: u64, text: Option<&str>) -> Option<i64> {
    let header = header as i64;
    if (LAST_DATE_MIN_MS..LAST_DATE_MAX_MS).contains(&header) {
        return Some(header);
    }
    let ms = chrono::NaiveDateTime::parse_from_str(text?, "%Y-%m-%d %H:%M")
        .ok()?
        .and_utc()
        .timestamp_millis();
    (LAST_DATE_MIN_MS..LAST_DATE_MAX_MS).contains(&ms).then_some(ms)
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

    /// Заголовок снапшота — источник по контракту, пока он похож на дату.
    #[test]
    fn last_edit_ms_prefers_plausible_header() {
        // 2026-03-01 10:40:29 UTC; дамп специально расходится — заголовок свежее.
        let got = last_edit_ms(1_772_358_029_681, Some("2026-01-30 13:17"));
        assert_eq!(got, Some(1_772_358_029_681));
    }

    /// Репро: ядро прислало в заголовке два упакованных f32 вместо мс. Раньше это
    /// уходило в БД как есть (`snap.last_date as i64`) и давало «год 158-миллионный».
    #[test]
    fn last_edit_ms_falls_back_to_dump_when_header_is_garbage() {
        let garbage = 4_937_127_386_796_687_413u64;
        assert!(
            garbage as i64 > LAST_DATE_MAX_MS,
            "репро: такое значение раньше писалось в last_date_ms без проверки"
        );
        let got = last_edit_ms(garbage, Some("2026-03-16 17:02")).expect("дамп читается");
        assert_eq!(got, 1_773_680_520_000, "2026-03-16 17:02 UTC в мс");
    }

    /// Нет ни одного читаемого источника → None, чтобы UPSERT сохранил прежнее
    /// значение, а не затёр его мусором.
    #[test]
    fn last_edit_ms_is_none_without_any_readable_source() {
        let garbage = 4_937_127_386_796_687_413u64;
        assert_eq!(last_edit_ms(garbage, None), None);
        assert_eq!(last_edit_ms(garbage, Some("не дата")), None);
        assert_eq!(last_edit_ms(0, Some("1899-12-30 00:00")), None, "дата вне диапазона — тоже мусор");
    }

    /// Форма UPSERT-а: повторный снапшот с нечитаемой датой (`None`) обязан оставить
    /// прежнее `last_date_ms`, а не обнулить его.
    #[test]
    fn upsert_keeps_known_last_date_when_new_one_is_unreadable() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../migrations/001_schema.sql")).unwrap();
        conn.execute(
            "INSERT INTO servers (id, name, token, modules) VALUES (1, 'srv', 'tok', '{}')",
            [],
        )
        .unwrap();

        let known: Option<i64> = Some(1_772_358_029_681);
        conn.execute(UPSERT_STRATEGY_SQL, rusqlite::params![1, 42i64, "S", "sig", 1, 12, known])
            .unwrap();
        conn.execute(UPSERT_STRATEGY_SQL, rusqlite::params![1, 42i64, "S", "sig", 1, 13, None::<i64>])
            .unwrap();

        let (ver, date): (i64, Option<i64>) = conn
            .query_row(
                "SELECT strategy_ver, last_date_ms FROM strategies WHERE server_id=1 AND moonbot_id=42",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(ver, 13, "остальные поля обновляются как раньше");
        assert_eq!(date, known, "дату затирать нельзя");
    }
}
