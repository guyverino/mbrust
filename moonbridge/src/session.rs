//! Один OS-thread = одна сессия = один MoonBot-сервер.
//!
//! Транспорт/handshake/шифрование/reconnect — внутри `moonproto`. Здесь:
//!   * подключение через event-sink модель (MoonEventSink + waker-канал);
//!   * event-loop: drain lifecycle + domain events, маршрутизация в storage.
//!
//! Каждый session-thread открывает своё SQLite-соединение к общему файлу (WAL).
//! Учитывает `servers.modules`: гейтит запись отчётов/стратегий/логов/команд.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use moonproto::{
    ClientConfig, ConnectConfig, Event, InitConfig, InitialStrategies, LifecycleEvent, MoonClient,
    MoonEventSink, ReportEvent, ReportHistoryDepth, ReportSchema, ReportSyncRequest, TransportMode,
};
use rusqlite::Connection;

use crate::config::DbServer;
use crate::{db, settings, storage};

/// Сессия сервера с авто-переподключением. moonproto сам реконнектит при ПОТЕРЕ
/// связи (link loss → Reconnecting → Connected{fresh:false}), но отказ НАЧАЛЬНОГО
/// connect/init приходит как терминальный `LifecycleEvent::ConnectFailed`, который
/// moonproto НЕ ретраит. Поэтому внешний цикл: `connect_and_run` пересоздаёт клиент
/// с экспоненциальным backoff (порт уровня-2 из MoonTerminal). Соединение к SQLite
/// открываем один раз и переиспользуем между попытками.
pub fn run_session(server: &DbServer, db_path: &str) -> anyhow::Result<()> {
    let conn = db::open(db_path).context("DB open failed")?;
    let init_backoff = || settings::get_int("reconnect_backoff_initial_secs", 5).max(1) as u64;
    let max_backoff = || settings::get_int("reconnect_backoff_max_secs", 300).max(1) as u64;

    let mut backoff = init_backoff();
    loop {
        let started = Instant::now();
        match connect_and_run(&conn, server) {
            Ok(()) => tracing::warn!(
                "[{}] session ended after {:?} — переподключаемся",
                server.name, started.elapsed()
            ),
            Err(e) => tracing::warn!(
                "[{}] session failed after {:?}: {e:#}",
                server.name, started.elapsed()
            ),
        }
        // Долго проживший коннект = это был разрыв, а не отказ старта → backoff с нуля.
        if started.elapsed() >= Duration::from_secs(60) {
            backoff = init_backoff();
        }
        tracing::info!("[{}] backoff {backoff}s перед реконнектом", server.name);
        std::thread::sleep(Duration::from_secs(backoff));
        backoff = backoff.saturating_mul(2).min(max_backoff());
    }
}

/// Один цикл жизни клиента: connect → init → event-loop. Возвращает `Ok(())` при
/// штатном завершении (wake-канал закрыт) и `Err` при отказе connect/init
/// (`ConnectFailed`/`BindFailed`) — вызывающий пересоздаёт клиент с backoff.
fn connect_and_run(conn: &Connection, server: &DbServer) -> anyhow::Result<()> {
    let info = moonproto::parse_key_info(&server.token).ok_or_else(|| {
        anyhow::anyhow!(
            "[{}] token is not a valid MoonProto base64 key (length={})",
            server.name,
            server.token.len()
        )
    })?;

    // Endpoint зашит в ключе (host/port/transport). Если ключ их не несёт —
    // фолбэк на явные ip/port из таблицы servers.
    let net = info.network.as_ref();
    let host = net
        .and_then(|n| n.address)
        .map(|a| a.to_string())
        .or_else(|| server.ip.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = net
        .map(|n| n.port)
        .filter(|p| *p != 0)
        .or_else(|| server.port.map(|p| p as u16))
        .unwrap_or(3000);
    let transport = net.map(|n| n.transport_mode).unwrap_or(TransportMode::V0);

    let client_cfg = ClientConfig::new(host.clone(), port, info.keys.master_key, info.keys.mac_key)
        .with_transport_mode(transport);

    // initial_strategies ОБЯЗАТЕЛЬНО — иначе init зависает после Connected.
    let init = InitConfig {
        initial_strategies: Some(InitialStrategies::new(0, Vec::new())),
        ..Default::default()
    };

    tracing::info!(
        server = %server.name,
        modules = ?server.modules,
        "connecting to MoonBot via MoonProto ({host}:{port})"
    );

    // Waker: moonproto будит session-thread после реального события.
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let event_wake_tx = wake_tx.clone();
    let (event_sink, event_queue) = MoonEventSink::queue_with_waker(move || {
        let _ = event_wake_tx.send(());
    });

    let client = MoonClient::connect_with_sink(
        client_cfg,
        ConnectConfig::new(init).with_connect_timeout(settings::connect_timeout()),
        event_sink,
    )
    .with_context(|| format!("[{}] connect/init failed", server.name))?;

    tracing::info!("[{}] MoonProto init done", server.name);

    // Подтянуть client settings в снапшот — команды, читающие текущие настройки
    // ядра (напр. глобальный ЧС монет coin_blacklist_*), опираются на них.
    if let Err(e) = client.settings().refresh() {
        tracing::debug!("[{}] client settings refresh failed: {e:?}", server.name);
    }

    // Бэкфилл декодированного из ключа endpoint в БД — чтобы /servers показывал,
    // куда реально подключились (бот кладёт только name+token).
    if let Err(e) = conn.execute(
        "UPDATE servers SET ip = ?1, port = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?3",
        rusqlite::params![host, port as i64, server.id],
    ) {
        tracing::warn!("[{}] endpoint backfill failed: {e}", server.name);
    }

    // Первичный sync стратегий (ордера приходят только событиями-отчётами).
    initial_sync(conn, &client, server);

    // Запустить типизированную репликацию БД отчёта Orders: resume с сохранённого
    // курсора либо fresh при первом запуске. Схема запрашивается ядром автоматически
    // (runtime сам шлёт schema-request перед догонкой). После hard-reconnect Lib
    // сама переотправит закоммиченный запрос — переотправляем только на новый клиент.
    if server.modules.listener_orders {
        if let Err(e) = start_report_sync(conn, &client, server) {
            tracing::warn!("[{}] report sync bootstrap failed: {e:#}", server.name);
        }
    }

    // Дневной файловый лог ядра (logs/<дата>_<server>.log) + экстрактор ошибок
    // в core_errors — если включён listener_logs.
    let (mut log_writer, mut error_watch) = if server.modules.listener_logs {
        (
            Some(crate::applog::DatedWriter::new(&server.name)),
            Some(crate::errors::ErrorWatch::new(server.id)),
        )
    } else {
        (None, None)
    };

    tracing::info!("[{}] entering event loop", server.name);
    // Последняя валидированная схема отчёта — нужна для маппинга полей строк.
    // Обновляется на `ReportEvent::Schema` (append-only; lib ревалидирует раз на
    // hard-сессию перед страницами/строками).
    let mut report_schema: Option<Arc<ReportSchema>> = None;
    let mut events = Vec::new();
    let mut lifecycle = Vec::new();
    // Периодический ре-синк стратегий (heartbeat). Ядро шлёт `Event::Strat` не на
    // каждую смену статистики, поэтому между событиями `strategies.updated_at`
    // замерзает — и downstream-гейты живости (`strategy_freshness_minutes` в risk/
    // cloner/generator) начинают считать живые стратегии «протухшими» и простаивают.
    // Таймер переупсертит снапшот раз в `strategies_resync_secs`, освежая updated_at.
    // Порт FETCH_FULL_STRATEGIES_INTERVAL из mb_ai.
    let mut last_strat_resync = std::time::Instant::now();
    loop {
        match wake_rx.recv_timeout(settings::poll_interval()) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!("[{}] wake channel closed, exiting session", server.name);
                break;
            }
        }

        lifecycle.clear();
        event_queue.drain_lifecycle_events_into(&mut lifecycle);
        for lc in &lifecycle {
            match lc {
                // Терминальный отказ начального connect/init — moonproto его НЕ
                // ретраит. Выходим с Err → внешний цикл пересоздаст клиент с backoff.
                LifecycleEvent::ConnectFailed { error } => {
                    anyhow::bail!("connect/init failed: {error}");
                }
                LifecycleEvent::BindFailed { .. } => {
                    anyhow::bail!("bind failed: {lc:?}");
                }
                // Разрыв/реконнект по потере связи — обрабатывает сам moonproto.
                LifecycleEvent::Reconnecting => {
                    tracing::info!("[{}] link loss → reconnecting…", server.name)
                }
                LifecycleEvent::ServerRestart => {
                    tracing::info!("[{}] server restart", server.name)
                }
                LifecycleEvent::Connected { fresh } => {
                    tracing::info!("[{}] connected (fresh={fresh})", server.name)
                }
                LifecycleEvent::Ready => tracing::info!("[{}] ready", server.name),
                _ => tracing::debug!("[{}] lifecycle: {lc:?}", server.name),
            }
        }

        events.clear();
        event_queue.drain_events_into(&mut events);
        for ev in &events {
            // Серверные логи ядра — в дневной файл; ошибки — в core_errors.
            if let Event::ServerLog(log) = ev {
                if let Some(w) = log_writer.as_mut() {
                    w.write_line(&log.msg);
                }
                if let Some(ew) = error_watch.as_mut() {
                    ew.inspect(conn, &log.msg);
                }
                continue;
            }
            // Типизированная репликация БД отчёта Orders — схема/страницы/строки.
            if let Event::Report(re) = ev {
                if server.modules.listener_orders {
                    if let Err(e) = handle_report(conn, &client, server, &mut report_schema, re) {
                        tracing::warn!("[{}] report handler error: {e:#}", server.name);
                    }
                }
                continue;
            }
            if let Err(e) = handle_event(conn, &client, server, ev) {
                tracing::warn!("[{}] event handler error: {e:#}", server.name);
            }
        }

        // Исходящие команды: дренаж очереди `commands` → MoonProto каждый тик.
        if server.modules.commander {
            if let Err(e) = crate::commands::drain(conn, &client, server.id) {
                tracing::warn!("[{}] command drain error: {e:#}", server.name);
            }
        }

        // Периодический ре-синк стратегий (heartbeat updated_at). 0 = выключено.
        if server.modules.listener_strategies {
            let secs = settings::get_int("strategies_resync_secs", 300).max(0) as u64;
            if secs > 0 && last_strat_resync.elapsed().as_secs() >= secs {
                match sync_strategies(conn, &client, server) {
                    Ok(n) => {
                        tracing::debug!("[{}] periodic strat re-sync: {n} strategies", server.name)
                    }
                    Err(e) => {
                        tracing::warn!("[{}] periodic strat re-sync failed: {e:#}", server.name)
                    }
                }
                last_strat_resync = std::time::Instant::now();
            }
        }
    }
    Ok(())
}

fn initial_sync(conn: &Connection, client: &MoonClient, server: &DbServer) {
    if !server.modules.listener_strategies {
        return;
    }
    match sync_strategies(conn, client, server) {
        Ok(n) => tracing::info!("[{}] initial strategies synced: {n}", server.name),
        Err(e) => tracing::warn!("[{}] initial strategies sync failed: {e:#}", server.name),
    }
}

/// Переупсертить все стратегии из текущего снапшота. Возвращает число.
fn sync_strategies(conn: &Connection, client: &MoonClient, server: &DbServer) -> anyhow::Result<usize> {
    let Some(snap) = client.snapshot() else {
        return Ok(0);
    };
    // Косметические поля (не создают новую версию) — из настройки, один раз на проход.
    let ignore = storage::strategies::ignore_keys_from_csv(
        &settings::get("strategies_ignore_keys").unwrap_or_default(),
    );
    let mut ok = 0usize;
    for s in snap.strats().snapshots() {
        match storage::strategies::upsert_snapshot(conn, server.id, s, s.checked, &ignore) {
            Ok(_) => ok += 1,
            Err(e) => tracing::warn!("[{}] strategy upsert failed: {e:#}", server.name),
        }
    }
    Ok(ok)
}

fn handle_event(
    conn: &Connection,
    client: &MoonClient,
    server: &DbServer,
    event: &Event,
) -> anyhow::Result<()> {
    match event {
        // Ордера идут отдельным типизированным потоком репликации
        // (`Event::Report`, см. `handle_report`) — здесь не обрабатываем.
        // Стратегии: на любое strat-событие переупсертим снапшот.
        Event::Strat(_) if server.modules.listener_strategies => {
            let n = sync_strategies(conn, client, server)?;
            tracing::debug!("[{}] strat event → re-upserted {n} strategies", server.name);
        }
        _ => {}
    }
    Ok(())
}

/// Глубина первичной репликации из настройки `orders_history_depth`:
/// `all` (по умолчанию) | `server` (server-default) | `<дней>`.
fn history_depth() -> ReportHistoryDepth {
    match settings::get("orders_history_depth").as_deref().map(str::trim) {
        Some("server") => ReportHistoryDepth::ServerDefault,
        Some(d) if !d.is_empty() && d != "all" => match d.parse::<u16>() {
            // Days(0) и Days(u16::MAX) — зарезервированные сентинелы (невалидны).
            Ok(n) if n > 0 && n < u16::MAX => ReportHistoryDepth::Days(n),
            _ => ReportHistoryDepth::All,
        },
        _ => ReportHistoryDepth::All,
    }
}

/// Запустить репликацию отчёта: resume с MAX(newRecID)+1 реплики либо fresh, если пусто.
/// Плюс регистрируем открытые строки — их close/delete в оффлайне (ниже курсора)
/// вернутся обычными RowUpsert/RowDelete.
fn start_report_sync(conn: &Connection, client: &MoonClient, server: &DbServer) -> anyhow::Result<()> {
    let cursor = storage::orders::resume_cursor(conn, server.id)?;
    let req = if cursor > 0 {
        ReportSyncRequest::resume(cursor)
    } else {
        ReportSyncRequest::fresh(history_depth())
    };
    client
        .reports()
        .sync(req)
        .map_err(|e| anyhow::anyhow!("reports().sync: {e}"))?;

    let open = storage::orders::open_rows(conn, server.id)?;
    if let Err(e) = client.reports().check_open_rows(&open) {
        tracing::debug!("[{}] check_open_rows failed: {e}", server.name);
    }
    tracing::info!(
        "[{}] report sync requested (cursor={cursor}, open_rows={})",
        server.name, open.len()
    );
    Ok(())
}

/// Обработка событий типизированной репликации БД отчёта Orders (page-based API).
fn handle_report(
    conn: &Connection,
    client: &MoonClient,
    server: &DbServer,
    schema_slot: &mut Option<Arc<ReportSchema>>,
    re: &ReportEvent,
) -> anyhow::Result<()> {
    match re {
        // Схема ядра (append-only) — заводим недостающие авто-колонки, запоминаем.
        ReportEvent::Schema(schema) => {
            storage::orders::apply_schema(conn, schema)?;
            *schema_slot = Some(Arc::clone(schema));
            tracing::info!("[{}] report schema rev={}", server.name, schema.revision());
        }
        // Страница catch-up: применяем в одной транзакции и ПОДТВЕРЖДАЕМ (page_applied)
        // — backpressure: lib следующую страницу не запросит до ack. database_recreated
        // обрабатывает apply_page (сброс реплики), после ack lib рестартует sync.
        ReportEvent::SyncPage(page) => {
            if let Some(schema) = schema_slot.as_ref() {
                storage::orders::apply_page(conn, server.id, schema, page)?;
            } else {
                tracing::warn!("[{}] report page до схемы — не применена", server.name);
            }
            client
                .reports()
                .page_applied(page)
                .map_err(|e| anyhow::anyhow!("page_applied: {e}"))?;
        }
        // Живой typed upsert/delete строки по newRecID.
        ReportEvent::RowUpsert(row) => {
            let Some(schema) = schema_slot.as_ref() else {
                tracing::debug!("[{}] report row до схемы — пропуск", server.name);
                return Ok(());
            };
            storage::orders::upsert_row(conn, server.id, schema, row)?;
        }
        ReportEvent::RowDelete { rec_id } => {
            storage::orders::delete_row(conn, server.id, *rec_id)?;
        }
        // Массовое soft-delete/restore строк отчёта (MoonProto v4): deleted=true →
        // физически удаляем затронутые строки (диапазоны+id); restore (deleted=false) →
        // no-op (строки уже нет, вернётся на sync). См. storage::orders::set_rows_deleted.
        ReportEvent::RowsDeleted(change) => {
            let removed = storage::orders::set_rows_deleted(conn, server.id, change)?;
            if removed > 0 || !change.deleted {
                tracing::debug!(
                    "[{}] report rows {}: ranges={} singles={} removed={}",
                    server.name,
                    if change.deleted { "deleted" } else { "restored (no-op)" },
                    change.ranges.len(),
                    change.singles.len(),
                    removed,
                );
            }
        }
        // Догонка завершена (после ack последней страницы). Курсор выводим из реплики,
        // хранить нечего; перерегистрируем открытые строки под свежее состояние.
        ReportEvent::SyncComplete(done) => {
            tracing::info!(
                "[{}] report sync complete: pages={} rows={} max_rec={} next_cursor={}",
                server.name, done.page_count, done.total_rows, done.max_rec_id, done.next_from_rec_id
            );
            let open = storage::orders::open_rows(conn, server.id)?;
            if let Err(e) = client.reports().check_open_rows(&open) {
                tracing::debug!("[{}] check_open_rows failed: {e}", server.name);
            }
        }
        // Карта живых строк (reconcile_alive, MoonProto 52dc7fd+). moonbridge её НЕ
        // запрашивает, поэтому событие приходит незапрошенным — как и в MoonTerminal,
        // только логируем и НЕ применяем: трактовать чужой битмап как истину поверх
        // covered_up_to значило бы удалить живые строки реплики.
        ReportEvent::AliveMapComplete(map) => {
            tracing::warn!(
                "[{}] незапрошенная alive-map (epoch={}, covered_up_to={}, outcome={:?}) — не применяю",
                server.name, map.epoch, map.covered_up_to, map.outcome
            );
        }
        ReportEvent::SyncStarted { request, .. } => {
            tracing::info!(
                "[{}] report sync started: from={} depth={}",
                server.name, request.from_rec_id, request.history_depth
            );
        }
        ReportEvent::OpenRowsCheckStarted { rec_ids } => {
            tracing::debug!("[{}] open-rows check started: {} ids", server.name, rec_ids.len());
        }
        ReportEvent::OpenRowsCheckComplete { rec_ids } => {
            tracing::debug!("[{}] open-rows check complete: {} ids", server.name, rec_ids.len());
        }
        ReportEvent::SchemaRejected { reason } => {
            tracing::warn!("[{}] report schema rejected: {reason}", server.name);
        }
    }
    Ok(())
}
