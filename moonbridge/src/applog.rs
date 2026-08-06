//! Дневной файловый лог серверных сообщений ядра (порт идеи из MoonTerminal
//! `applog::DatedWriter`).
//!
//! Логи ядер НЕ пишем в БД (могут быть большими) — по файлу на ядро с датой:
//! `logs/<YYYY-MM-DD>_<server>.log`, при смене суток файл переоткрывается.
//! Один экземпляр на session-thread (= на сервер), поэтому без блокировок.
//! Формат строки: `HH:MM:SS.mmm \t <msg>`.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use chrono::Utc;

pub struct DatedWriter {
    label: String,
    dir: String,
    date: String,
    file: Option<BufWriter<File>>,
}

impl DatedWriter {
    pub fn new(label: &str) -> Self {
        let dir = std::env::var("MOONBRIDGE_LOG_DIR").unwrap_or_else(|_| "logs".into());
        Self {
            label: sanitize_label(label),
            dir,
            date: String::new(),
            file: None,
        }
    }

    /// Дописать одну строку лога ядра (текущее UTC-время как префикс).
    pub fn write_line(&mut self, msg: &str) {
        let now = Utc::now();
        let date = now.format("%Y-%m-%d").to_string();
        let hms = now.format("%H:%M:%S%.3f").to_string();
        if self.file.is_none() || self.date != date {
            self.open(&date);
        }
        if let Some(f) = self.file.as_mut() {
            let clean = msg.replace(['\n', '\r'], " ");
            let _ = writeln!(f, "{hms}\t{clean}");
            let _ = f.flush();
        }
    }

    fn open(&mut self, date: &str) {
        let _ = std::fs::create_dir_all(&self.dir);
        let path = Path::new(&self.dir).join(format!("{date}_{}.log", self.label));
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(BufWriter::new);
        self.date = date.to_string();
    }
}

/// Удалить файлы логов ядра `<YYYY-MM-DD>_<label>.log` старше N дней.
/// 0 = не удалять. Возраст берётся из даты в имени (не mtime). Файлы без даты в
/// начале имени (напр. `moonbridge.log.<дата>`) не трогаем.
pub fn purge_old(retention_days: i64) {
    if retention_days <= 0 {
        return;
    }
    let dir = std::env::var("MOONBRIDGE_LOG_DIR").unwrap_or_else(|_| "logs".into());
    let cutoff = Utc::now().date_naive() - chrono::Duration::days(retention_days);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".log") || name.len() < 11 || name.as_bytes()[10] != b'_' {
            continue; // не наш формат <дата>_<label>.log
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(&name[0..10], "%Y-%m-%d") {
            if d < cutoff {
                if std::fs::remove_file(entry.path()).is_ok() {
                    tracing::info!("logs: удалён старый файл {name}");
                }
            }
        }
    }
}

/// Имя файла безопасно: только [a-zA-Z0-9_-], остальное → '_'.
fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
