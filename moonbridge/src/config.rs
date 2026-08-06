//! Конфиг — единый и минимальный. Читается из окружения (`.env`).
//!
//! Обязательного НИЧЕГО: путь к SQLite-файлу имеет дефолт, файл создаётся сам.
//! Серверы и runtime-параметры — в БД (таблицы `servers`, `app_settings`).
//!
//! Единый `.env` в корне (общий с hub):
//!   MOONBRIDGE_DB   = data/moonbridge.db   (путь к SQLite-файлу; необязателен)
//!   MOONBRIDGE_LOG_DIR = logs               (каталог логов; необязателен)

/// Путь к SQLite-файлу БД. Дефолт `data/moonbridge.db`.
pub fn db_path() -> String {
    std::env::var("MOONBRIDGE_DB").unwrap_or_else(|_| "data/moonbridge.db".into())
}

/// Сервер, прочитанный из таблицы `servers`. Уже отфильтрован: token не пуст,
/// включён хотя бы один модуль.
#[derive(Debug, Clone)]
pub struct DbServer {
    pub id: i32,
    pub name: String,
    /// ip/port опциональны: обычно зашиты в ключе. Служат фолбэком + бэкфиллятся
    /// из ключа при подключении для отображения.
    pub ip: Option<String>,
    pub port: Option<i32>,
    pub token: String,
    /// Включённые модули, ключи как в `modules.name`.
    pub modules: ServerModules,
}

#[derive(Debug, Clone, Default)]
pub struct ServerModules {
    pub listener_strategies: bool,
    pub listener_orders: bool,
    /// Приём серверных логов ядра → дневные файлы.
    pub listener_logs: bool,
    /// Отправка команд в ядро через MoonProto (дренаж таблицы `commands`).
    pub commander: bool,
}

impl ServerModules {
    /// Разбирает JSON-объект флагов (`servers.modules`, хранится как TEXT).
    pub fn from_json(v: &serde_json::Value) -> Self {
        let bool_at = |k: &str| -> bool { v.get(k).and_then(|x| x.as_bool()).unwrap_or(false) };
        Self {
            listener_strategies: bool_at("listener_strategies"),
            listener_orders: bool_at("listener_orders"),
            listener_logs: bool_at("listener_logs"),
            commander: bool_at("commander"),
        }
    }

    pub fn any_enabled(&self) -> bool {
        self.listener_strategies || self.listener_orders || self.listener_logs || self.commander
    }
}
