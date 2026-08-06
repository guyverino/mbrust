//! Ордера — типизированная репликация БД отчёта Orders из MoonProto (page-based API).
//!
//! Источник: `client.reports().sync(...)` → `Event::Report(ReportEvent::…)`.
//! Догонка идёт СТРАНИЦАМИ с backpressure (docs/reports.md): страницу применяем в
//! одной транзакции и лишь ПОСЛЕ коммита зовём `page_applied` — до этого lib следующую
//! не запрашивает (одна страница в полёте, writer = граница обратного давления).
//! Так исключён OOM от безлимитного потока батчей старого API.
//!
//! Один файл БД держит несколько серверов → таблица `orders` общая, ключ составной
//! `(server_id, newRecID)`. Колонки ядра (BuyDate, ProfitBTC, …) заводятся авто-колонками
//! по `ReportSchema` (ALTER ADD COLUMN); имена PascalCase, но SQLite сопоставляет их без
//! учёта регистра — кэш известных колонок в lowercase, совпадает с lowercase-каталогом.
//!
//! Курсор (новый контракт): всегда `MAX(newRecID)+1` локальной реплики (страницы
//! ack'аются последовательно — дыр в хвосте нет даже после краша). 0 = реплика пуста →
//! fresh-sync. Оффлайн-изменения открытых строк НИЖЕ курсора ловит `check_open_rows`.
//! Reconcile по keep-set (старый API) и таблица-курсор `report_sync` больше не нужны.
//!
//! UNIQUE-индекс из хелпера схемы (`sqlite_unique_index_sql`, только по `newRecID`)
//! НЕ создаём — он сломал бы мультисерверность; конфликт-таргет = составной PK.

use std::cell::RefCell;
use std::collections::HashSet;

use moonproto::{ReportRow, ReportRowsDeleted, ReportSchema, ReportSyncPage, ReportValue};
use rusqlite::types::{ToSql, Value as SqlValue};
use rusqlite::Connection;

const TABLE: &str = "orders";

thread_local! {
    static KNOWN_COLS: RefCell<Option<HashSet<String>>> = const { RefCell::new(None) };
}

/// Применить схему отчёта: завести недостающие колонки ядра (append-only).
/// Вызывать на `ReportEvent::Schema`. Идемпотентно (кэш thread_local из PRAGMA).
pub fn apply_schema(conn: &Connection, schema: &ReportSchema) -> anyhow::Result<()> {
    ensure_columns_loaded(conn)?;
    for field in schema.fields() {
        let key = field.name.to_ascii_lowercase();
        // newRecID — часть составного PK, уже есть; остальные известные пропускаем.
        if key == "newrecid" {
            continue;
        }
        let known = KNOWN_COLS.with(|k| k.borrow().as_ref().is_some_and(|s| s.contains(&key)));
        if known {
            continue;
        }
        // DDL от хелпера схемы: имя и sql_spec из аутентифицированной схемы ядра,
        // идентификатор в кавычках. Имя таблицы контролируем сами.
        let ddl = schema.sqlite_add_column_sql(TABLE, field);
        conn.execute(&ddl, [])?;
        KNOWN_COLS.with(|k| {
            if let Some(set) = k.borrow_mut().as_mut() {
                set.insert(key.clone());
            }
        });
        // Индекс под окно отчётов/Health по closedate (как у прежнего каталога).
        if key == "closedate" {
            let _ = conn.execute(
                &format!("CREATE INDEX IF NOT EXISTS orders_closedate ON {TABLE}(closedate)"),
                [],
            );
        }
        tracing::info!("orders: авто-колонка «{}» {} (поле схемы ядра)", field.name, field.sql_spec);
    }
    Ok(())
}

/// Идемпотентный upsert живой строки по (server_id, newRecID) — `ReportEvent::RowUpsert`.
/// Пишем только присутствующие в строке ИЗВЕСТНЫЕ поля (live-обновление может быть
/// частичным — отсутствующие колонки не затираются). Схема должна быть уже применена.
pub fn upsert_row(
    conn: &Connection,
    server_id: i32,
    schema: &ReportSchema,
    row: &ReportRow,
) -> anyhow::Result<()> {
    write_row(conn, server_id, schema, row)
}

/// Страница catch-up (`ReportEvent::SyncPage`): всё в ОДНОЙ транзакции. При
/// `database_recreated` — реплику сервера сбрасываем (lib после ack рестартует с нуля).
/// Ошибки отдельных строк логируем, но не валим страницу (иначе догонка застрянет).
pub fn apply_page(
    conn: &Connection,
    server_id: i32,
    schema: &ReportSchema,
    page: &ReportSyncPage,
) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    if page.database_recreated {
        tx.execute(
            &format!("DELETE FROM {TABLE} WHERE server_id = ?1"),
            [server_id as i64],
        )?;
        tracing::warn!(
            "orders: сервер {server_id} пересоздал БД отчёта — реплика сброшена, lib рестартует sync"
        );
    }
    for row in page.rows.iter() {
        if let Err(e) = write_row(&tx, server_id, schema, row) {
            tracing::error!(
                "orders: страница сервера {server_id}: upsert newRecID={} упал: {e}",
                row.rec_id
            );
        }
    }
    tx.commit()?;
    Ok(())
}

/// Общий писатель строки. `prepare_cached`: у батчей catch-up набор полей стабилен →
/// SQL одинаков, кэш компиляции спасает writer от роли бутылочного горла.
fn write_row(
    conn: &Connection,
    server_id: i32,
    schema: &ReportSchema,
    row: &ReportRow,
) -> anyhow::Result<()> {
    ensure_columns_loaded(conn)?;
    let mut names: Vec<String> = Vec::with_capacity(row.fields.len());
    let mut vals: Vec<SqlValue> = Vec::with_capacity(row.fields.len());
    for fv in &row.fields {
        let Some(field) = schema.field(fv.field_index) else {
            continue;
        };
        let name = field.name.to_ascii_lowercase();
        // newRecID пишем отдельно (часть ключа); неизвестные колонки пропускаем
        // (ALTER мог не пройти — не роняем всю вставку из-за одного поля).
        if name == "newrecid" {
            continue;
        }
        let known = KNOWN_COLS.with(|k| k.borrow().as_ref().is_some_and(|s| s.contains(&name)));
        if !known {
            continue;
        }
        vals.push(value_to_sql(&fv.value));
        names.push(field.name.clone());
    }

    let mut cols = String::from("server_id, newRecID");
    let mut ph = String::from("?, ?");
    let mut set = String::from("updated_at = CURRENT_TIMESTAMP");
    for n in &names {
        let q = quote_ident(n);
        cols.push_str(&format!(", {q}"));
        ph.push_str(", ?");
        set.push_str(&format!(", {q} = excluded.{q}"));
    }
    let sql = format!(
        "INSERT INTO {TABLE} ({cols}) VALUES ({ph}) \
         ON CONFLICT (server_id, newRecID) DO UPDATE SET {set}"
    );

    let sid = server_id as i64;
    let mut params: Vec<&dyn ToSql> = Vec::with_capacity(vals.len() + 2);
    params.push(&sid);
    params.push(&row.rec_id);
    for v in &vals {
        params.push(v);
    }
    let mut stmt = conn.prepare_cached(&sql)?;
    stmt.execute(params.as_slice())?;
    Ok(())
}

/// Удалить строку по (server_id, newRecID) — `ReportEvent::RowDelete`.
pub fn delete_row(conn: &Connection, server_id: i32, rec_id: i64) -> anyhow::Result<()> {
    conn.execute(
        &format!("DELETE FROM {TABLE} WHERE server_id = ?1 AND newRecID = ?2"),
        rusqlite::params![server_id, rec_id],
    )?;
    Ok(())
}

/// Массовое soft-delete/restore строк отчёта — `ReportEvent::RowsDeleted` (MoonProto v4).
/// Ядро вещает удаление/восстановление пачки строк по диапазонам rec_id (inclusive) и
/// одиночным id. Наша реплика — журнал сделок с ФИЗИЧЕСКИМ удалением (как `delete_row`),
/// а downstream-запросы (risk/cloner) по колонке `deleted` не фильтруют, поэтому:
///   deleted=true  → физически удаляем затронутые строки (диапазоны + одиночные);
///   deleted=false → no-op (restore: строки уже нет — вернётся обычным sync/RowUpsert).
/// Возвращает число удалённых строк (для лога). Пустой набор — no-op.
pub fn set_rows_deleted(
    conn: &Connection,
    server_id: i32,
    change: &ReportRowsDeleted,
) -> anyhow::Result<usize> {
    if !change.deleted || change.is_empty() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for range in change.ranges.iter() {
        removed += conn.execute(
            &format!("DELETE FROM {TABLE} WHERE server_id = ?1 AND newRecID BETWEEN ?2 AND ?3"),
            rusqlite::params![server_id, range.from_rec_id, range.to_rec_id],
        )?;
    }
    for &rec_id in change.singles.iter() {
        removed += conn.execute(
            &format!("DELETE FROM {TABLE} WHERE server_id = ?1 AND newRecID = ?2"),
            rusqlite::params![server_id, rec_id],
        )?;
    }
    Ok(removed)
}

/// Курсор следующего sync-запроса: `MAX(newRecID)+1` реплики сервера, 0 если пусто.
/// 0 → fresh (вся удержанная история ядра); >0 → resume(cursor).
pub fn resume_cursor(conn: &Connection, server_id: i32) -> anyhow::Result<i64> {
    let max: Option<i64> = conn.query_row(
        &format!("SELECT MAX(newRecID) FROM {TABLE} WHERE server_id = ?1"),
        [server_id as i64],
        |r| r.get::<_, Option<i64>>(0),
    )?;
    Ok(max.map(|m| m.saturating_add(1)).unwrap_or(0))
}

/// Открытые строки сервера (closedate пустой/нулевой), свежие первыми, ≤100 — набор
/// для `check_open_rows`: открытая сделка могла закрыться/удалиться в оффлайне НИЖЕ
/// курсора; результаты придут обычными RowUpsert/RowDelete. (lib сам сортирует/капит,
/// но не гоняем лишнее.)
pub fn open_rows(conn: &Connection, server_id: i32) -> anyhow::Result<Vec<i64>> {
    ensure_columns_loaded(conn)?;
    let has_closedate = KNOWN_COLS.with(|k| k.borrow().as_ref().is_some_and(|s| s.contains("closedate")));
    if !has_closedate {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT newRecID FROM {TABLE} \
         WHERE server_id = ?1 AND newRecID > 0 AND (closedate IS NULL OR closedate <= 0) \
         ORDER BY newRecID DESC LIMIT 100"
    ))?;
    let rows = stmt.query_map([server_id as i64], |r| r.get::<_, i64>(0))?;
    Ok(rows.filter_map(Result::ok).collect())
}

// ── Вспомогательное ────────────────────────────────────────────────────────────

fn ensure_columns_loaded(conn: &Connection) -> anyhow::Result<()> {
    let need = KNOWN_COLS.with(|k| k.borrow().is_none());
    if need {
        let set = load_columns(conn)?;
        KNOWN_COLS.with(|k| *k.borrow_mut() = Some(set));
    }
    Ok(())
}

fn load_columns(conn: &Connection) -> anyhow::Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({TABLE})"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?; // колонка 1 = name
    let mut set = HashSet::new();
    for r in rows {
        set.insert(r?.to_ascii_lowercase());
    }
    Ok(set)
}

fn value_to_sql(v: &ReportValue) -> SqlValue {
    match v {
        ReportValue::Integer(i) => SqlValue::Integer(*i),
        ReportValue::Float(f) => SqlValue::Real(*f),
        ReportValue::Text(s) => SqlValue::Text(s.clone()),
    }
}

/// Идентификатор в двойных кавычках (как в хелперах схемы MoonProto).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../migrations/001_schema.sql"))
            .expect("migration must apply cleanly");
        for id in [1, 2] {
            conn.execute(
                "INSERT INTO servers (id, name, token, modules) VALUES (?1, ?2, 'tok', '{}')",
                rusqlite::params![id, format!("srv{id}")],
            )
            .unwrap();
        }
        // KNOWN_COLS — thread_local и переживает между тестами в одном потоке; сбросим,
        // чтобы кэш перечитался под свежую in-memory БД.
        KNOWN_COLS.with(|k| *k.borrow_mut() = None);
        conn
    }

    fn insert_order(conn: &Connection, server_id: i32, rec_id: i64, status: i64, closedate: Option<i64>) {
        conn.execute(
            "INSERT INTO orders (server_id, newRecID, status, closedate) VALUES (?1,?2,?3,?4)",
            rusqlite::params![server_id, rec_id, status, closedate],
        )
        .unwrap();
    }

    fn ids(conn: &Connection, server_id: i32) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT newRecID FROM orders WHERE server_id=?1 ORDER BY newRecID")
            .unwrap();
        stmt.query_map([server_id], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn composite_pk_allows_same_rec_id_across_servers() {
        let conn = migrated_conn();
        insert_order(&conn, 1, 10, 0, None);
        insert_order(&conn, 2, 10, 0, None);
        assert_eq!(ids(&conn, 1), vec![10]);
        assert_eq!(ids(&conn, 2), vec![10]);
    }

    #[test]
    fn delete_row_scoped_to_server() {
        let conn = migrated_conn();
        insert_order(&conn, 1, 10, 0, None);
        insert_order(&conn, 2, 10, 0, None);
        delete_row(&conn, 1, 10).unwrap();
        assert!(ids(&conn, 1).is_empty());
        assert_eq!(ids(&conn, 2), vec![10]);
    }

    #[test]
    fn resume_cursor_is_max_plus_one_or_zero() {
        let conn = migrated_conn();
        assert_eq!(resume_cursor(&conn, 1).unwrap(), 0, "пустая реплика → fresh");
        insert_order(&conn, 1, 7, 1, Some(100));
        insert_order(&conn, 1, 42, 0, None);
        insert_order(&conn, 2, 5, 0, None); // другой сервер не влияет
        assert_eq!(resume_cursor(&conn, 1).unwrap(), 43);
        assert_eq!(resume_cursor(&conn, 2).unwrap(), 6);
    }

    #[test]
    fn open_rows_returns_unclosed_newest_first() {
        let conn = migrated_conn();
        insert_order(&conn, 1, 10, 1, Some(123)); // закрыт
        insert_order(&conn, 1, 11, 0, None); // открыт
        insert_order(&conn, 1, 12, 0, Some(0)); // открыт (closedate=0)
        insert_order(&conn, 2, 99, 0, None); // чужой сервер
        assert_eq!(open_rows(&conn, 1).unwrap(), vec![12, 11], "свежие первыми, только открытые");
        assert_eq!(open_rows(&conn, 2).unwrap(), vec![99]);
    }

    /// Форма SQL из `write_row`: PascalCase-имена ядра в кавычках сливаются с
    /// предобъявленными lowercase-колонками (SQLite без учёта регистра), upsert по
    /// составному ключу, поздняя строка обновляет поля, prepare_cached валиден.
    #[test]
    fn upsert_sql_shape_merges_core_columns_case_insensitively() {
        let conn = migrated_conn();
        conn.execute("ALTER TABLE orders ADD COLUMN \"SellPrice\" double", []).unwrap();
        KNOWN_COLS.with(|k| *k.borrow_mut() = None); // перечитать колонки после ALTER
        let up = "INSERT INTO orders (server_id, \"newRecID\", \"Status\", \"ProfitBTC\", \"SellPrice\") \
                  VALUES (?1,?2,?3,?4,?5) \
                  ON CONFLICT (server_id, newRecID) DO UPDATE SET \
                    \"Status\"=excluded.\"Status\", \"ProfitBTC\"=excluded.\"ProfitBTC\", \
                    \"SellPrice\"=excluded.\"SellPrice\", updated_at=CURRENT_TIMESTAMP";
        conn.execute(up, rusqlite::params![1, 10i64, 0i64, 1.5f64, 0.42f64]).unwrap();
        conn.execute(up, rusqlite::params![1, 10i64, 1i64, 2.5f64, 0.55f64]).unwrap();
        assert_eq!(ids(&conn, 1), vec![10], "upsert, не дубль");
        let (st, pr, sl): (i64, f64, f64) = conn
            .query_row("SELECT status, profitbtc, sellprice FROM orders WHERE server_id=1 AND newrecid=10", [],
                       |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((st, pr, sl), (1, 2.5, 0.55));
    }
}
