//! Экстрактор ошибок из логов ядра → таблица `core_errors` (эпизодная модель).
//!
//! Мультибиржевой: понимает Bybit (`{"retCode":N,"retMsg"}`), Binance
//! (`{"code":-N,"msg"}` + текстовые `Set … order: FAIL; … [код]`) и Hyperliquid
//! (`… CancelReplace: FAIL; … {"status":"err","response":…}`). Сырые
//! WS_API-эхо (`WS_API <id> => {…}`) НЕ считаем — тот же фейл переписывается
//! строкой `: FAIL;`, иначе двойной счёт.
//!
//! На каждую строку лога (`ServerLog`) смотрим, не ошибка ли это. Если да —
//! извлекаем структуру (монета, task_id, order_id, cid, retCode/retMsg) и
//! пишем/обновляем ОДНУ строку на сигнатуру (сервер+монета+код+order_id):
//! first_seen (когда впервые), last_seen (последний раз), prev_seen
//! (предпоследний — чтобы был мгновенный интервал), count. Повтор той же ошибки
//! всегда → UPDATE count+1, prev_seen←last_seen, last_seen=now; дубликатов не
//! плодим, даже если ошибка вернулась через час. «Закончилась/возобновилась» и
//! средняя периодичность ((last_seen−first_seen)/(count−1)) — считает модуль
//! logwatch по динамике count/last_seen. Полная история — в сырых лог-файлах.
//!
//! Контекст = N строк лога ДО ошибки (что делал перед фейлом) — кольцевой буфер.
//! Один экземпляр на session-thread (= на сервер), без блокировок.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use regex::Regex;
use rusqlite::{Connection, OptionalExtension};

pub struct ErrorWatch {
    server_id: i32,
    ring: VecDeque<String>,
    enabled: bool,
    context_lines: usize,
    cfg_at: Instant,
    re_ret: Regex,
    re_code: Regex,
    re_bracket: Regex,
    re_resp: Regex,
    re_reason: Regex,
    re_status: Regex,
    re_coin: Regex,
    re_task: Regex,
    re_order: Regex,
    re_cid: Regex,
}

struct ErrInfo {
    category: String,
    coin: Option<String>,
    task_id: Option<i64>,
    order_id: Option<String>,
    cid: Option<String>,
    ret_code: Option<i64>,
    ret_msg: Option<String>,
}

impl ErrInfo {
    /// Сигнатура агрегации: категория+монета+код. НЕ включаем order_id: у статус-
    /// событий `=> BuyFail` в `<...>` стоит уникальный ident MoonProto-сообщения
    /// (не биржевой id), из-за чего каждая строка давала новую sig (count=1) и
    /// таблица раздувалась. Теперь одна и та же ошибка по монете/коду — одна строка
    /// с растущим count.
    fn sig(&self) -> String {
        format!(
            "{}|{}|{}",
            self.category,
            self.coin.as_deref().unwrap_or(""),
            self.ret_code.map(|c| c.to_string()).unwrap_or_default(),
        )
    }
}

impl ErrorWatch {
    pub fn new(server_id: i32) -> Self {
        Self {
            server_id,
            ring: VecDeque::new(),
            enabled: crate::settings::get_bool("errors_enabled", true),
            context_lines: crate::settings::get_int("errors_context_lines", 6).max(0) as usize,
            cfg_at: Instant::now(),
            re_ret: Regex::new(r#""retCode":(\d+),"retMsg":"([^"]*)""#).unwrap(),
            re_code: Regex::new(r#""code":(-\d+),"msg":"([^"]*)""#).unwrap(),
            re_bracket: Regex::new(r"\[(\d+)\]\s*$").unwrap(),
            re_resp: Regex::new(r#""response":"([^"]*)""#).unwrap(),
            re_reason: Regex::new(r": FAIL;\s+([^<{\[][^<{]*?)\s*(?:TryCount|<|\{|\[\d+\]\s*$|$)").unwrap(),
            re_status: Regex::new(r"Status:\s*(\d{3})").unwrap(),
            // Монета/символ = ведущий токен строки (после опц. внутреннего времени
            // `HH:MM:SS` или `.mmm`), затем опц. `<Short>` и `:`. Допускаем `_`
            // (фьючерсы `ETHUSDT_260626`, `ETH_0626`) и ведущую цифру (`1000SATS`).
            re_coin: Regex::new(r"^\s*(?:\d{1,2}:\d{2}:\d{2}(?:\.\d+)?\s+|\.\d+\s+)?([A-Za-z0-9][A-Za-z0-9_]{1,19})(?:<[^>]*>)?\s*:").unwrap(),
            re_task: Regex::new(r"\[\d+\]\s*\((\d+)\)").unwrap(),
            re_order: Regex::new(r"(?:<|ID:\s*)(\d{10,})").unwrap(),
            re_cid: Regex::new(r"cID:\s*([A-Za-z0-9]+)").unwrap(),
        }
    }

    /// Периодически перечитывает настройки (on-the-fly, но без mutex-лока на
    /// каждую строку лога).
    fn reload_cfg_if_due(&mut self) {
        if self.cfg_at.elapsed() < Duration::from_secs(30) {
            return;
        }
        self.enabled = crate::settings::get_bool("errors_enabled", true);
        self.context_lines = crate::settings::get_int("errors_context_lines", 6).max(0) as usize;
        self.cfg_at = Instant::now();
    }

    /// Обработать одну строку лога ядра.
    pub fn inspect(&mut self, conn: &Connection, msg: &str) {
        self.reload_cfg_if_due();
        if self.enabled {
            if let Some(info) = self.classify(msg) {
                if let Err(e) = self.record(conn, &info, msg) {
                    tracing::debug!("core_errors write failed: {e}");
                }
            }
        }
        // Контекст = строки ДО ошибки, поэтому пушим ПОСЛЕ обработки.
        self.ring.push_back(msg.to_string());
        while self.ring.len() > self.context_lines {
            self.ring.pop_front();
        }
    }

    fn record(&self, conn: &Connection, info: &ErrInfo, line: &str) -> rusqlite::Result<()> {
        let sig = info.sig();
        // Одна строка на сигнатуру: ищем существующую (без окна по времени — та же
        // ошибка не дублируется, даже если вернулась позже). last_seen DESC берёт
        // самую свежую (важно на переходе, пока в БД есть старые эпизоды-дубли).
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM core_errors \
                 WHERE server_id=?1 AND sig=?2 \
                 ORDER BY last_seen DESC LIMIT 1",
                rusqlite::params![self.server_id, sig],
                |r| r.get(0),
            )
            .optional()?;

        match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE core_errors \
                     SET count=count+1, prev_seen=last_seen, last_seen=CURRENT_TIMESTAMP \
                     WHERE id=?1",
                    rusqlite::params![id],
                )?;
            }
            None => {
                let context = self
                    .ring
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                conn.execute(
                    "INSERT INTO core_errors \
                     (server_id, category, coin, task_id, order_id, cid, ret_code, ret_msg, sig, line, context) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    rusqlite::params![
                        self.server_id,
                        info.category,
                        info.coin,
                        info.task_id,
                        info.order_id,
                        info.cid,
                        info.ret_code,
                        info.ret_msg,
                        sig,
                        line,
                        context,
                    ],
                )?;
                tracing::debug!(
                    "core_error[{}]: {} coin={:?} order={:?} code={:?}",
                    self.server_id, info.category, info.coin, info.order_id, info.ret_code
                );
            }
        }
        Ok(())
    }

    /// Ошибка ли строка, и если да — извлечь структуру.
    fn classify(&self, msg: &str) -> Option<ErrInfo> {
        // Сырое WS_API-эхо (`WS_API <id> => {…}`) не классифицируем вообще: тот же
        // фейл всегда переписывается строкой `: FAIL;` (проверено 1:1 по логам), а
        // эхо может нести и ключевые слова → иначе двойной счёт.
        if msg.contains("WS_API") {
            return None;
        }

        // Bybit: {"retCode":N,"retMsg":"…"} (N==0 → успешный ответ, не ошибка).
        let bybit = self.re_ret.captures(msg);
        let bybit_code = bybit
            .as_ref()
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<i64>().ok());
        let bybit_msg = bybit
            .as_ref()
            .and_then(|c| c.get(2))
            .map(|m| m.as_str().to_string());

        // Binance: {"code":-N,"msg":"…"} (только отрицательные = ошибка).
        let bin = self.re_code.captures(msg);
        let bin_code = bin
            .as_ref()
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<i64>().ok());
        let bin_msg = bin
            .as_ref()
            .and_then(|c| c.get(2))
            .map(|m| m.as_str().to_string());

        if !is_error(msg, bybit_code, bin_code) {
            return None;
        }

        // Единый код ошибки: явный JSON → хвостовой `[код]` (Binance/HL FAIL) →
        // `Status: NNN` (напр. `New leverage was not set … Status: 400`).
        let mut ret_code = bybit_code.filter(|c| *c != 0).or(bin_code);
        if ret_code.is_none() {
            ret_code = self
                .re_bracket
                .captures(msg)
                .or_else(|| self.re_status.captures(msg))
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<i64>().ok());
        }
        // Единое сообщение: JSON msg → HL "response" → текст причины после `FAIL;`.
        let ret_msg = bybit_msg
            .or(bin_msg)
            .or_else(|| {
                self.re_resp
                    .captures(msg)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string())
            })
            .or_else(|| {
                self.re_reason
                    .captures(msg)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().trim().to_string())
                    .filter(|s| !s.is_empty())
            });

        let coin = self
            .re_coin
            .captures(msg)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string());
        let task_id = self
            .re_task
            .captures(msg)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<i64>().ok());
        let order_id = self
            .re_order
            .captures(msg)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string());
        let cid = self
            .re_cid
            .captures(msg)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string());

        Some(ErrInfo {
            category: categorize(msg, ret_code).to_string(),
            coin,
            task_id,
            order_id,
            cid,
            ret_code,
            ret_msg,
        })
    }
}

/// Является ли строка ошибкой. Явные коды бирж (Bybit retCode!=0 / Binance
/// отрицательный code) решают первыми; иначе — маркер провала ордера `: FAIL;`,
/// статус BuyFail/SellFail и точечные ключевые слова (без общего «error» — шум).
fn is_error(msg: &str, bybit_code: Option<i64>, bin_code: Option<i64>) -> bool {
    if let Some(c) = bybit_code {
        return c != 0; // retCode:0 — успешный ответ
    }
    if bin_code.is_some() {
        return true; // парсим только отрицательные Binance-коды
    }
    // Провал постановки/замены ордера: "Set Buy order: FAIL;", "CancelReplace: FAIL;".
    if msg.contains(": FAIL;") {
        return true;
    }
    if msg.contains("=> BuyFail") || msg.contains("=> SellFail") {
        return true;
    }
    let lower = msg.to_ascii_lowercase();
    if lower.contains("failed!") || lower.contains(" failed,") || lower.contains("cancel failed") {
        return true;
    }
    const KW: &[&str] = &[
        "insufficient",
        "not enough",
        "rejected",
        "denied",
        "leverage error",
        "invalid symbol",
        "order not exist",
        "order does not exist",
    ];
    KW.iter().any(|k| lower.contains(k))
}

fn categorize(msg: &str, ret_code: Option<i64>) -> &'static str {
    if msg.contains("Rate Limit") || msg.contains("Too many") {
        return "rate_limit";
    }
    // Статус-событие ядра (MoonBot) о провале ордера.
    if msg.contains("=> BuyFail") || msg.contains("=> SellFail") {
        return "order_fail";
    }
    // Отказ биржи: есть код ошибки или маркер провала постановки ордера.
    if ret_code.is_some() || msg.contains(": FAIL;") {
        return "exchange_error";
    }
    "action_fail"
}

/// Удалить строки ошибок старше N дней (по last_seen). 0 = не удалять.
pub fn purge_old(conn: &Connection, retention_days: i64) {
    if retention_days <= 0 {
        return;
    }
    let expr = format!("-{retention_days} days");
    if let Ok(n) = conn.execute(
        "DELETE FROM core_errors WHERE last_seen < datetime('now', ?1)",
        rusqlite::params![expr],
    ) {
        if n > 0 {
            tracing::info!("core_errors: удалено старых строк: {n}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w() -> ErrorWatch {
        // settings недоступны в юните — конструируем напрямую.
        ErrorWatch {
            server_id: 1,
            ring: VecDeque::new(),
            enabled: true,
            context_lines: 6,
            cfg_at: Instant::now(),
            re_ret: Regex::new(r#""retCode":(\d+),"retMsg":"([^"]*)""#).unwrap(),
            re_code: Regex::new(r#""code":(-\d+),"msg":"([^"]*)""#).unwrap(),
            re_bracket: Regex::new(r"\[(\d+)\]\s*$").unwrap(),
            re_resp: Regex::new(r#""response":"([^"]*)""#).unwrap(),
            re_reason: Regex::new(r": FAIL;\s+([^<{\[][^<{]*?)\s*(?:TryCount|<|\{|\[\d+\]\s*$|$)").unwrap(),
            re_status: Regex::new(r"Status:\s*(\d{3})").unwrap(),
            // Монета/символ = ведущий токен строки (после опц. внутреннего времени
            // `HH:MM:SS` или `.mmm`), затем опц. `<Short>` и `:`. Допускаем `_`
            // (фьючерсы `ETHUSDT_260626`, `ETH_0626`) и ведущую цифру (`1000SATS`).
            re_coin: Regex::new(r"^\s*(?:\d{1,2}:\d{2}:\d{2}(?:\.\d+)?\s+|\.\d+\s+)?([A-Za-z0-9][A-Za-z0-9_]{1,19})(?:<[^>]*>)?\s*:").unwrap(),
            re_task: Regex::new(r"\[\d+\]\s*\((\d+)\)").unwrap(),
            re_order: Regex::new(r"(?:<|ID:\s*)(\d{10,})").unwrap(),
            re_cid: Regex::new(r"cID:\s*([A-Za-z0-9]+)").unwrap(),
        }
    }

    #[test]
    fn detects_retcode_error() {
        let e = w()
            .classify(r#"CheckAssets Quarterly: {"retCode":131203,"retMsg":"accountType is invalid for current user"}"#)
            .unwrap();
        assert_eq!(e.category, "exchange_error");
        assert_eq!(e.ret_code, Some(131203));
        assert!(e.ret_msg.unwrap().contains("accountType"));
    }

    #[test]
    fn ignores_retcode_ok() {
        assert!(w().classify(r#"Set Buy order ==> {"retCode":0,"retMsg":"OK"}"#).is_none());
    }

    #[test]
    fn detects_buyfail_with_ids() {
        let e = w()
            .classify("17:18:50  IREN<Short> : [1] (30419) SendMServer: status changed <5312937408504108408> None => BuyFail, epoch 1")
            .unwrap();
        assert_eq!(e.category, "order_fail");
        assert_eq!(e.coin.as_deref(), Some("IREN"));
        assert_eq!(e.task_id, Some(30419));
        assert_eq!(e.order_id.as_deref(), Some("5312937408504108408"));
    }

    #[test]
    fn detects_cancel_failed() {
        let e = w()
            .classify("Cancel order <USDT-DATA P:0.28120 Q:248.5  ID: 1457695721259610112 cID: BaYvXzdGmOonJvgqqXShSWGTJ>  failed! ")
            .unwrap();
        assert_eq!(e.category, "action_fail");
        assert_eq!(e.order_id.as_deref(), Some("1457695721259610112"));
        assert_eq!(e.cid.as_deref(), Some("BaYvXzdGmOonJvgqqXShSWGTJ"));
    }

    #[test]
    fn ignores_normal_line() {
        assert!(w().classify("RAVE: [1] (66071) BUY Order SET!").is_none());
        assert!(w().classify("status changed <123> BuySet => BuyCancel").is_none());
    }

    #[test]
    fn detects_binance_sign_agreement_fail_line() {
        // Текстовая строка провала Binance: код в хвостовом [4411], причина после FAIL;.
        let e = w()
            .classify("00:00:38  MVLL<Short> : [2] (36714) USDT-MVLL Set Buy order: FAIL;  Please sign TradFi-Perps agreement contract fapi.  TryCount=1 <apiKey=dKb1&symbol=MVLLUSDT&price=41.94>  [4411]")
            .unwrap();
        assert_eq!(e.category, "exchange_error");
        assert_eq!(e.coin.as_deref(), Some("MVLL"));
        assert_eq!(e.task_id, Some(36714));
        assert_eq!(e.ret_code, Some(4411));
        assert_eq!(e.ret_msg.as_deref(), Some("Please sign TradFi-Perps agreement contract fapi."));
    }

    #[test]
    fn detects_binance_checkassets_code() {
        // Небиржевой-ордерный отказ Binance: только JSON {"code":-2015,"msg":…}.
        let e = w()
            .classify(r#"CheckAssets Futures Error [401]  {"code":-2015,"msg":"Invalid API-key, IP, or permissions for action"} "#)
            .unwrap();
        assert_eq!(e.category, "exchange_error");
        assert_eq!(e.ret_code, Some(-2015));
        assert_eq!(e.ret_msg.as_deref(), Some("Invalid API-key, IP, or permissions for action"));
        assert!(e.coin.is_none());
    }

    #[test]
    fn ignores_ws_api_echo_to_avoid_double_count() {
        // WS_API-эхо содержит {"code":-4411,…}, но тот же фейл переписывается
        // строкой `: FAIL;`. Считать эхо нельзя — иначе двойной счёт.
        assert!(w()
            .classify(r#"00:00:38  MVLL<Short> : [2] (36714) WS_API <7446479891056737> => {"id":"7446479891056737","status":400,"error":{"code":-4411,"msg":"Please sign TradFi-Perps agreement contract fapi."},"rateLimits":[]}"#)
            .is_none());
    }

    #[test]
    fn extracts_futures_dated_coin() {
        // Символы с `_` (фьючерс) раньше давали coin=None → всё схлопывалось в
        // один action_fail. Теперь берём ведущий токен как монету.
        let e = w()
            .classify("00:00:57  ETH_0626<Short> : [6] (175379) USDT-ETH_0626 Set Buy order: FAIL;  Invalid symbol.  TryCount=1 <symbol=ETHUSDT_260626>  [1121]")
            .unwrap();
        assert_eq!(e.coin.as_deref(), Some("ETH_0626"));
        assert_eq!(e.task_id, Some(175379));
        assert_eq!(e.ret_code, Some(1121));
        assert_eq!(e.category, "exchange_error");
    }

    #[test]
    fn extracts_leverage_fail_coin_and_status() {
        let e = w()
            .classify("BTCUSDT_260626: New leverage was not set: x10 (resp: Status: 400 Invalid symbol.)")
            .unwrap();
        assert_eq!(e.coin.as_deref(), Some("BTCUSDT_260626"));
        assert_eq!(e.ret_code, Some(400)); // из `Status: 400`
    }

    #[test]
    fn coin_not_extracted_from_non_coin_prefix() {
        // Аккаунт-уровневые строки не должны получать «монету» из первого слова.
        let e = w()
            .classify(r#"CheckAssets Futures Error [401]  {"code":-2015,"msg":"Invalid API-key, IP, or permissions for action"}"#)
            .unwrap();
        assert!(e.coin.is_none());
    }

    #[test]
    fn detects_hyperliquid_cancelreplace_fail() {
        let e = w()
            .classify(r#".073  NIL<Short> : [5] (80) NIL CancelReplace: FAIL;  {"type":"action","payload":{"status":"err","response":"Error placing new order during modify: Post only order would have immediately matched."}} < <a="186" b="true" > >  [400]"#)
            .unwrap();
        assert_eq!(e.category, "exchange_error");
        assert_eq!(e.coin.as_deref(), Some("NIL"));
        assert_eq!(e.ret_code, Some(400));
        assert!(e.ret_msg.as_deref().unwrap().contains("Post only"));
    }
}
