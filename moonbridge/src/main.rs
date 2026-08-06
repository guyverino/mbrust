//! moonbridge — entry point.
//!
//! 1. Открываем SQLite-файл (создаётся сам), применяем схему (идемпотентно)
//! 2. Инициализируем settings-кэш (TTL 60s, читает app_settings)
//! 3. Загружаем servers (фильтр: token есть, какой-то модуль включён)
//! 4. Для каждого сервера запускаем thread с MoonProto-сессией (своё соединение).

mod applog;
mod commands;
mod config;
mod db;
mod errors;
mod send;
mod session;
mod settings;
mod storage;

use std::thread;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let _log_guard = init_logging();

    let db_path = config::db_path();
    tracing::info!("db file: {db_path}");

    // Открыть + применить схему + ping.
    let conn = db::connect_and_migrate(&db_path).map_err(|e| anyhow::anyhow!("DB init failed: {e}"))?;
    let _: i64 = conn.query_row("SELECT 1", [], |r| r.get(0))?;
    tracing::info!("DB ok");

    settings::init(db_path.clone());

    // Очистка старых файлов логов ядра и эпизодов ошибок по настройкам retention.
    applog::purge_old(settings::get_int("logs_retention_days", 14));
    errors::purge_old(&conn, settings::get_int("errors_retention_days", 90));

    let servers = db::load_servers(&conn)?;
    tracing::info!("loaded {} servers from DB", servers.len());
    for s in &servers {
        tracing::info!(
            "  · #{} {}  modules={{strategies={}, orders={}, logs={}, commander={}}}",
            s.id, s.name,
            s.modules.listener_strategies, s.modules.listener_orders,
            s.modules.listener_logs, s.modules.commander
        );
    }
    if servers.is_empty() {
        tracing::warn!(
            "No servers configured. Add one via the hub bot (/addserver <name> <key>), \
             then restart moonbridge. Waiting…"
        );
        // Не выходим — иначе под systemd Restart=always будет рестарт-петля.
        // Новые серверы подхватываются при рестарте демона.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    drop(conn); // главный коннект больше не нужен

    // Фоновое обслуживание: purge выше — разовый (при старте), а демон живёт
    // неделями без рестарта, поэтому чистим ретеншен периодически.
    spawn_maintenance(db_path.clone())?;

    let mut handles = Vec::new();
    for server in servers {
        let db_path = db_path.clone();
        let name = server.name.clone();
        let h = thread::Builder::new()
            .name(format!("mp-{name}"))
            .spawn(move || {
                if let Err(e) = session::run_session(&server, &db_path) {
                    tracing::error!("[{name}] session terminated: {e:#}");
                }
            })?;
        handles.push(h);
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

/// Фоновый поток обслуживания: раз в N часов чистит старые строки `core_errors`
/// и старые лог-файлы ядра по настройкам retention (читаются на лету из
/// app_settings). Своё SQLite-соединение (открывается на каждый прогон).
fn spawn_maintenance(db_path: String) -> anyhow::Result<()> {
    thread::Builder::new()
        .name("maintenance".into())
        .spawn(move || loop {
            // Интервал читаем каждую итерацию — меняется на лету без рестарта.
            let hours = settings::get_int("errors_purge_interval_hours", 6).max(1);
            std::thread::sleep(std::time::Duration::from_secs(hours as u64 * 3600));
            applog::purge_old(settings::get_int("logs_retention_days", 14));
            match db::open(&db_path) {
                Ok(conn) => errors::purge_old(&conn, settings::get_int("errors_retention_days", 90)),
                Err(e) => tracing::warn!("maintenance: DB open failed: {e:#}"),
            }
        })?;
    Ok(())
}

/// Инициализация tracing:
///   * файл `logs/mb_ingester.YYYY-MM-DD` — дневная ротация, полный фильтр
///   * stderr — только WARN/ERROR (терминал тихий, видны только проблемы)
///
/// Уровень логирования настраивается через `RUST_LOG` (env-filter синтаксис).
/// Возвращаемый guard нужно держать живым на всё время работы программы,
/// иначе buffered-запись в файл потеряется на shutdown.
fn init_logging() -> WorkerGuard {
    let log_dir = std::env::var("MOONBRIDGE_LOG_DIR").unwrap_or_else(|_| "logs".into());
    std::fs::create_dir_all(&log_dir).expect("create log dir");

    let file_appender = tracing_appender::rolling::daily(&log_dir, "moonbridge.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    // moonproto::crypted=error глушит спам "replay/duplicate detected" —
    // нормальный UDP-шум, протокол сам дедуплицирует. Если нужно отлаживать
    // транспорт — RUST_LOG=info,moonproto::crypted=debug.
    let default_filter = "info,moonproto=info,moonbridge=debug,moonproto::crypted=error";
    let file_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_filter));
    let stderr_filter = EnvFilter::new("warn,moonproto::crypted=error");

    let file_layer = fmt::layer()
        .with_target(true)
        .with_thread_names(true)
        .with_ansi(false)
        .with_writer(file_writer)
        .with_filter(file_filter);

    let stderr_layer = fmt::layer()
        .with_target(true)
        .with_thread_names(true)
        .with_filter(stderr_filter);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .init();

    guard
}
