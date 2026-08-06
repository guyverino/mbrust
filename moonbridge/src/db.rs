//! SQLite — открытие файла, схема, загрузка серверов.

use rusqlite::Connection;

use crate::config::{DbServer, ServerModules};

/// Открыть соединение к файлу БД с прагмами для конкурентного доступа.
/// WAL: читатели не блокируют писателя; busy_timeout: писатели ждут, не падают.
pub fn open(path: &str) -> anyhow::Result<Connection> {
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(path)
        .map_err(|e| anyhow::anyhow!("SQLite open {path} failed: {e}"))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; \
         PRAGMA busy_timeout = 15000; \
         PRAGMA foreign_keys = ON; \
         PRAGMA synchronous = NORMAL;",
    )?;
    Ok(conn)
}

/// Открыть + применить схему (идемпотентно). Одна консолидированная миграция.
pub fn connect_and_migrate(path: &str) -> anyhow::Result<Connection> {
    let conn = open(path)?;
    // Ордера перешли со старого report-SQL зеркала (PK db_id) на typed-репликацию
    // MoonProto (PK newRecID). Старую таблицу пересоздаём один раз — до применения
    // консолидированной схемы, которая заведёт её в новой форме.
    drop_legacy_orders(&conn)?;
    // Курсор репликации больше не в отдельной таблице (выводим MAX(newRecID)+1 из
    // orders) — сносим устаревшую report_sync, если осталась от прежней версии.
    conn.execute_batch("DROP TABLE IF EXISTS report_sync")?;
    conn.execute_batch(include_str!("../migrations/001_schema.sql"))
        .map_err(|e| anyhow::anyhow!("schema apply failed: {e}"))?;
    // Колонки, добавленные после первого релиза схемы: CREATE TABLE их уже
    // содержит для новых БД, а существующим нужен ADD COLUMN (SQLite не умеет
    // ADD COLUMN IF NOT EXISTS — проверяем через PRAGMA table_info).
    ensure_column(&conn, "core_errors", "prev_seen", "TEXT")?;
    // Устаревшие настройки (после рефакторинга) — чистим, чтобы не висели в UI.
    // errors_episode_gap_sec: убрано дробление ошибок на эпизоды по паузе.
    conn.execute(
        "DELETE FROM app_settings WHERE key = 'errors_episode_gap_sec'",
        [],
    )?;
    tracing::info!("SQLite schema applied ({path})");
    Ok(conn)
}

/// Старое зеркало ордеров опознаём по колонке `db_id` (ключ старого потока
/// ClosedSellOrderReport). Если она есть — таблица в устаревшей форме (PK
/// (server_id, db_id), lowercase-каталог, raw_sql). DROP → консолидированная
/// схема пересоздаст `orders` под typed-репликацию (PK (server_id, newRecID)).
/// Данные там были незакоммиченным зеркалом отчёта — репликация нальёт заново.
fn drop_legacy_orders(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(orders)")?;
    let has_db_id = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == "db_id");
    if has_db_id {
        conn.execute_batch("DROP TABLE orders")?;
        tracing::info!("migrated: dropped legacy orders table (db_id → newRecID typed replication)");
    }
    Ok(())
}

/// Идемпотентно добавить колонку, если её ещё нет (для миграции старых БД).
fn ensure_column(conn: &Connection, table: &str, col: &str, decl: &str) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == col);
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
        tracing::info!("migrated: added column {table}.{col} {decl}");
    }
    Ok(())
}

/// Загружает серверы с непустым token и хотя бы одним включённым модулем.
pub fn load_servers(conn: &Connection) -> anyhow::Result<Vec<DbServer>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, ip, port, token, modules \
         FROM servers \
         WHERE token IS NOT NULL AND length(token) > 0 \
         ORDER BY id",
    )?;

    let rows = stmt.query_map([], |row| {
        let modules_json: String = row.get(5)?;
        Ok((
            row.get::<_, i64>(0)? as i32,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?.map(|p| p as i32),
            row.get::<_, String>(4)?,
            modules_json,
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (id, name, ip, port, token, modules_json) = r?;
        let modules_val: serde_json::Value =
            serde_json::from_str(&modules_json).unwrap_or(serde_json::Value::Null);
        let modules = ServerModules::from_json(&modules_val);
        if !modules.any_enabled() {
            tracing::info!("skip server #{id} {name} — no modules enabled");
            continue;
        }
        out.push(DbServer {
            id,
            name,
            ip,
            port,
            token,
            modules,
        });
    }
    Ok(out)
}
