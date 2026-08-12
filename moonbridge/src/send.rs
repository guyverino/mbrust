//! Маппинг команды из очереди (`commands.kind` + `payload`) в вызов MoonProto.
//!
//! Приватные Python-модули пишут «намерение» в таблицу `commands`; здесь оно
//! превращается в конкретный typed-вызов MoonProto. Это единственное место в
//! moonbridge, знающее send-API ядра — добавление новой команды = одна ветка
//! match здесь + документация формата payload для Python-стороны.

use moonproto::{MoonClient, SellOrderParams};
use serde_json::{json, Value};

/// Отправляет одну команду через MoonProto. Возвращает JSON-результат (для
/// `commands.result`) либо человекочитаемую ошибку (для `commands.error`).
pub fn dispatch(client: &MoonClient, kind: &str, payload: &Value) -> Result<Value, String> {
    match kind {
        // ── Стратегии ──────────────────────────────────────────────
        // Запустить/остановить работу стратегий (глобально на ядре).
        "strategy_start" => client
            .strategies()
            .start()
            .map(|_| json!({}))
            .map_err(fmt_err),
        "strategy_stop" => client
            .strategies()
            .stop()
            .map(|_| json!({}))
            .map_err(fmt_err),
        // Галочка стратегии (start/stop конкретной): {strategy_id: u64, checked: bool}.
        "strategy_set_checked" => {
            let id = req_u64(payload, "strategy_id")?;
            let checked = req_bool(payload, "checked")?;
            client
                .strategies()
                .set_checked(id, checked)
                .map(|_| json!({ "strategy_id": id, "checked": checked }))
                .map_err(fmt_err)
        }
        // Правка параметра стратегии: {strategy_id: u64, field: str, value: bool|num|str}.
        // ВНИМАНИЕ: путь отправки (StrategyEditor + sync_local_strategies) требует
        // проверки на живом ядре — семантика "local strategies" в публичном
        // MoonProto не подтверждена (правит ли существующую серверную стратегию,
        // как moonhook SetParam). Обкатать перед боевым использованием (cloner).
        "strategy_set_param" => set_param(client, payload),
        // Правка полей ОДНОЙ стратегии (весь список синкается разом внутри).
        "strategy_set_params" => set_params(client, payload),
        // Правка МНОГИХ стратегий за ОДИН sync: {edits: [{strategy_id, params}]}.
        // sync_local_strategies ЗАМЕНЯЕТ весь локальный список, поэтому массовые
        // правки цикла (генератор/health) обязаны идти одной командой — иначе
        // затирают друг друга (стратегия пропадает из снапшота → not found).
        "strategy_edit_batch" => strategy_edit_batch(client, payload),

        // ── Ордера ─────────────────────────────────────────────────
        // Отменить ордер по uid: {uid: u64}.
        "order_cancel" => {
            let uid = req_u64(payload, "uid")?;
            client
                .orders()
                .cancel(uid)
                .map(|_| json!({ "uid": uid }))
                .map_err(fmt_err)
        }
        // Отменить все ордера (Engine API).
        "cancel_all_orders" => client
            .account()
            .cancel_all_orders()
            .map(|_| json!({}))
            .map_err(fmt_err),
        // Выставить sell-ордер: {market: str, price: f64, size: f64}.
        "sell_order" => {
            let market = req_str(payload, "market")?;
            let price = req_f64(payload, "price")?;
            let size = req_f64(payload, "size")?;
            client
                .trade()
                .sell_order(SellOrderParams {
                    market: market.clone(),
                    price,
                    size,
                })
                .map(|_| json!({ "market": market, "price": price, "size": size }))
                .map_err(fmt_err)
        }

        // ── Глобальный ЧС монет (client settings, НЕ по стратегии) ──
        // Добавить/убрать монету в общий coins_black_list ядра: {coin: str}.
        "coin_blacklist_add" => coin_blacklist(client, payload, true),
        "coin_blacklist_remove" => coin_blacklist(client, payload, false),

        // Panic Sell по монете (вкл/выкл): {coin: str, on?: bool=true}.
        // Порт старого mooncmd `sell <coin>`.
        "panic_sell_coin" => {
            let coin = req_str(payload, "coin")?;
            let on = payload.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
            client
                .orders()
                .switch_panic_sell_by_market(coin.clone(), on)
                .map(|_| json!({ "coin": coin, "on": on }))
                .map_err(fmt_err)
        }

        // Установить плечо на монете: {market: str, leverage: i64}.
        // Для фикса «wrong leverage» (analyzer/logwatch).
        "set_leverage" => {
            let market = req_str(payload, "market")?;
            let leverage = payload
                .get("leverage")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| "payload.leverage required (int)".to_string())?;
            client
                .account()
                .set_leverage(&market, leverage as i32)
                .map(|_| json!({ "market": market, "leverage": leverage }))
                .map_err(fmt_err)
        }

        other => Err(format!("unsupported command kind: {other}")),
    }
}

/// Добавить/убрать монету в ГЛОБАЛЬНЫЙ ЧС ядра (`coins_black_list` в client
/// settings, не по стратегии). MoonProto не умеет атомарного «add one» — читаем
/// текущие настройки из снапшота, правим comma-separated список, включаем
/// `use_coins_black_list` и шлём весь `ClientSettingsCommand` обратно.
fn coin_blacklist(client: &MoonClient, payload: &Value, add: bool) -> Result<Value, String> {
    let coin = req_str(payload, "coin")?;
    let snap = client.snapshot().ok_or_else(|| "no snapshot yet".to_string())?;
    let mut settings = snap
        .settings()
        .client_settings
        .clone()
        .ok_or_else(|| "client settings ещё не загружены (повторить позже)".to_string())?;

    let mut coins: Vec<String> = settings
        .coins_black_list_text
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    coins.retain(|c| !c.eq_ignore_ascii_case(coin.as_str())); // дедуп/удаление
    if add {
        coins.push(coin.clone());
        settings.use_coins_black_list = true;
    }
    settings.coins_black_list_text = coins.join(",");

    client.settings().send(settings).map_err(fmt_err)?;
    Ok(json!({
        "coin": coin,
        "action": if add { "add" } else { "remove" },
        "blacklist_len": coins.len(),
    }))
}

/// Правка одного параметра одной стратегии: {strategy_id: u64, field, value}.
fn set_param(client: &MoonClient, payload: &Value) -> Result<Value, String> {
    let strategy_id = req_u64(payload, "strategy_id")?;
    let field = req_str(payload, "field")?;
    let value = payload
        .get("value")
        .ok_or_else(|| "payload.value required".to_string())?;
    let mut params = serde_json::Map::new();
    params.insert(field, value.clone());
    edit_and_sync(client, &[(strategy_id, &params)])
}

/// Батч-правка параметров ОДНОЙ стратегии: {strategy_id: u64, params: {field: value}}.
fn set_params(client: &MoonClient, payload: &Value) -> Result<Value, String> {
    let strategy_id = req_u64(payload, "strategy_id")?;
    let params = payload
        .get("params")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "payload.params required (object)".to_string())?;
    edit_and_sync(client, &[(strategy_id, params)])
}

/// Батч-правка МНОГИХ стратегий за один sync: {edits: [{strategy_id, params}, …]}.
/// Критично для массовых правок (генератор/health правят десятки стратегий за
/// цикл): `sync_local_strategies` ЗАМЕНЯЕТ весь локальный список, поэтому все
/// правки цикла должны уходить одним вызовом — иначе они затирают друг друга.
fn strategy_edit_batch(client: &MoonClient, payload: &Value) -> Result<Value, String> {
    let arr = payload
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "payload.edits required (array)".to_string())?;
    let mut edits: Vec<(u64, &serde_json::Map<String, Value>)> = Vec::with_capacity(arr.len());
    for e in arr {
        let sid = e
            .get("strategy_id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "edit.strategy_id required (u64)".to_string())?;
        let params = e
            .get("params")
            .and_then(|v| v.as_object())
            .ok_or_else(|| "edit.params required (object)".to_string())?;
        edits.push((sid, params));
    }
    edit_and_sync(client, &edits)
}

/// Ядро правки: берёт ВЕСЬ локальный список стратегий (`snapshot_vec`), применяет
/// правки к нужным через `StrategyEditor`, и отправляет весь список ОДНИМ
/// `sync_local_strategies` (документированный контракт — синхронизация списка,
/// а не патч одного поля). Пропущенные (не найденные) id собираются в `missing`.
fn edit_and_sync(
    client: &MoonClient,
    edits: &[(u64, &serde_json::Map<String, Value>)],
) -> Result<Value, String> {
    let snap = client.snapshot().ok_or_else(|| "no snapshot yet".to_string())?;
    let strats = snap.strats();
    let schema = strats
        .strategy_schema()
        .ok_or_else(|| "strategy schema not available yet".to_string())?;

    // Тип поля по схеме ядра — приводим значение к нему (в каталоге bool-поля
    // хранятся как choice "YES"/"NO", а ядро ждёт Bool).
    let ftypes: std::collections::HashMap<&str, moonproto::StrategyFieldType> = schema
        .fields
        .iter()
        .map(|f| (f.name.as_str(), f.type_id))
        .collect();

    let mut all = strats.snapshot_vec(); // владелец полного списка (клон)
    let index: std::collections::HashMap<u64, usize> =
        all.iter().enumerate().map(|(i, s)| (s.strategy_id, i)).collect();

    let mut applied = 0usize; // стратегий, где применилось ≥1 поле
    let mut fields = 0usize; // всего применённых полей
    let mut skipped = 0usize; // полей пропущено (кривой тип/неизвестное поле)
    let mut missing: Vec<u64> = Vec::new();
    let mut skipped_names: Vec<String> = Vec::new(); // диагностика: какие поля пропущены
    let mut init_err: Option<String> = None;
    for (sid, params) in edits {
        let Some(&i) = index.get(sid) else {
            missing.push(*sid);
            continue;
        };
        let mut editor = match moonproto::StrategyEditor::from_snapshot(schema, &all[i]) {
            Ok(ed) => ed,
            Err(e) => {
                init_err.get_or_insert(format!("{sid} editor init: {e:?}"));
                continue;
            }
        };
        // Устойчивость на уровне поля: кривое поле пропускаем (не роняем стратегию
        // и тем более батч) — важно, т.к. служебные поля (напр. LastEditDate)
        // могут отсутствовать в схеме ядра.
        let mut ok = 0usize;
        for (field, value) in params.iter() {
            match apply_one(&mut editor, field, value, ftypes.get(field.as_str()).copied()) {
                Ok(()) => ok += 1,
                Err(e) => {
                    skipped += 1;
                    let note = format!("{field}: {e}");
                    if skipped_names.len() < 8 && !skipped_names.iter().any(|n| n.starts_with(&format!("{field}:"))) {
                        skipped_names.push(note);
                    }
                }
            }
        }
        if ok > 0 {
            // Обновить LastEditDate (last_date) — иначе ядро по своему rollback-guard
            // (docs/strats.md: last_date = «Unix-ms для core rollback guards») сочтёт
            // приходящую стратегию НЕ новее своей и ОТКАТИТ правку к серверной версии.
            // Тогда параметры не применяются, а LastEditDate стоит на месте → Health
            // видит стратегию «старой + без ордеров» и освобождает её каждый цикл.
            editor.touch_now();
            all[i] = editor.into_snapshot();
            applied += 1;
            fields += ok;
        }
    }

    if applied == 0 {
        // id'шники и в ошибку: вызывающему они нужны ровно так же, как в успешном
        // ответе — иначе на полном промахе батча он знает счётчик, но не имена.
        return Err(format!(
            "no strategies applied (found {}, missing {missing:?}, fields skipped {}){}",
            edits.len() - missing.len(),
            skipped,
            init_err.map(|e| format!(": {e}")).unwrap_or_default(),
        ));
    }

    client
        .strategies()
        .sync_local_strategies(all)
        .map_err(fmt_err)?;
    Ok(json!({
        "strategies": applied, "fields": fields,
        // Счётчик оставлен для совместимости, но вызывающему нужен ИМЕННО список:
        // батч считается 'sent' при applied>=1, и без id'шников Python-сторона не
        // может отличить «мою стратегию ядро не нашло» от «чужую в том же батче».
        "missing": missing.len(), "missing_ids": missing, "skipped": skipped,
        "skipped_fields": skipped_names,
    }))
}

/// Применить одно поле к редактору, приводя значение к типу поля из схемы.
fn apply_one(
    editor: &mut moonproto::StrategyEditor,
    field: &str,
    value: &Value,
    ftype: Option<moonproto::StrategyFieldType>,
) -> Result<(), String> {
    use moonproto::StrategyFieldType as T;
    match ftype {
        Some(T::Bool) => editor
            .set_bool(field, coerce_bool(value).map_err(|e| format!("{field}: {e}"))?)
            .map_err(|e| format!("set_bool {field}: {e:?}")),
        Some(
            T::Double | T::Single | T::Int32 | T::Int64 | T::Byte | T::Word | T::UInt32 | T::UInt64,
        ) => editor
            .set_number(field, coerce_f64(value).map_err(|e| format!("{field}: {e}"))?)
            .map_err(|e| format!("set_number {field}: {e:?}")),
        Some(T::String) => editor
            .set_string(field, &coerce_string(value))
            .map_err(|e| format!("set_string {field}: {e:?}")),
        // Тип неизвестен схеме — угадываем по JSON-типу (старое поведение).
        _ => {
            if let Some(b) = value.as_bool() {
                editor.set_bool(field, b).map_err(|e| format!("set_bool {field}: {e:?}"))
            } else if let Some(n) = value.as_f64() {
                editor.set_number(field, n).map_err(|e| format!("set_number {field}: {e:?}"))
            } else if let Some(s) = value.as_str() {
                editor.set_string(field, s).map_err(|e| format!("set_string {field}: {e:?}"))
            } else {
                Err(format!("param {field}: value must be bool, number, or string"))
            }
        }
    }
}

/// YES/TRUE/1/ON → true; NO/FALSE/0/OFF/"" → false; число → !=0.
fn coerce_bool(value: &Value) -> Result<bool, String> {
    if let Some(b) = value.as_bool() {
        Ok(b)
    } else if let Some(n) = value.as_f64() {
        Ok(n != 0.0)
    } else if let Some(s) = value.as_str() {
        match s.trim().to_ascii_uppercase().as_str() {
            "YES" | "TRUE" | "1" | "ON" | "Y" => Ok(true),
            "NO" | "FALSE" | "0" | "OFF" | "N" | "" => Ok(false),
            other => Err(format!("bool: cannot parse {other:?}")),
        }
    } else {
        Err("bool: unsupported value".to_string())
    }
}

/// Число как есть; строка парсится; bool → 1.0/0.0.
fn coerce_f64(value: &Value) -> Result<f64, String> {
    if let Some(n) = value.as_f64() {
        Ok(n)
    } else if let Some(b) = value.as_bool() {
        Ok(if b { 1.0 } else { 0.0 })
    } else if let Some(s) = value.as_str() {
        s.trim()
            .parse::<f64>()
            .map_err(|_| format!("number: cannot parse {s:?}"))
    } else {
        Err("number: unsupported value".to_string())
    }
}

/// Строка как есть; число/bool → строковое представление.
fn coerce_string(value: &Value) -> String {
    match value.as_str() {
        Some(s) => s.to_string(),
        None => value.to_string(),
    }
}

// ── payload helpers ──────────────────────────────────────────────

fn req_u64(p: &Value, key: &str) -> Result<u64, String> {
    p.get(key)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("payload.{key} required (u64)"))
}

fn req_bool(p: &Value, key: &str) -> Result<bool, String> {
    p.get(key)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| format!("payload.{key} required (bool)"))
}

fn req_f64(p: &Value, key: &str) -> Result<f64, String> {
    p.get(key)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("payload.{key} required (number)"))
}

fn req_str(p: &Value, key: &str) -> Result<String, String> {
    p.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("payload.{key} required (string)"))
}

fn fmt_err(e: moonproto::MoonClientError) -> String {
    format!("{e:?}")
}
