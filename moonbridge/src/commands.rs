//! Дренаж очереди исходящих команд (`commands` таблица) → MoonProto (SQLite).
//!
//! Вызывается в session-loop каждого сервера (если включён модуль `commander`).
//! Один session-thread на сервер = один дренер на server_id, гонок нет.

use moonproto::MoonClient;
use rusqlite::Connection;
use serde_json::Value;

/// Обрабатывает до 50 pending-команд для сервера: отправляет через MoonProto,
/// помечает sent/failed. Ошибка отдельной команды не роняет дренаж.
pub fn drain(conn: &Connection, client: &MoonClient, server_id: i32) -> anyhow::Result<()> {
    let pending: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, kind, payload FROM commands \
             WHERE server_id = ?1 AND status = 'pending' ORDER BY id LIMIT 50",
        )?;
        let rows = stmt.query_map(rusqlite::params![server_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };

    for (id, kind, payload_s) in pending {
        let payload: Value = serde_json::from_str(&payload_s).unwrap_or(Value::Null);
        match crate::send::dispatch(client, &kind, &payload) {
            Ok(result) => {
                let result_s = serde_json::to_string(&result).unwrap_or_default();
                conn.execute(
                    "UPDATE commands SET status='sent', result=?2, error=NULL, \
                     sent_at=CURRENT_TIMESTAMP WHERE id=?1",
                    rusqlite::params![id, result_s],
                )?;
                tracing::info!("command #{id} {kind} → sent");
            }
            Err(e) => {
                conn.execute(
                    "UPDATE commands SET status='failed', error=?2, \
                     sent_at=CURRENT_TIMESTAMP WHERE id=?1",
                    rusqlite::params![id, e],
                )?;
                tracing::warn!("command #{id} {kind} → failed: {e}");
            }
        }
    }
    Ok(())
}
