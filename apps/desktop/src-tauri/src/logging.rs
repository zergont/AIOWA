//! Журнал приложения: консоль (видна в `tauri dev`) и файл
//! `%LOCALAPPDATA%\AIOWA\logs\aiowa.ГГГГ-ММ-ДД.log` с ежедневной ротацией; хранятся последние
//! `KEEP_FILES` файлов. Дата в имени файла и метки строк — UTC (ротация `tracing-appender` и
//! таймер `fmt` по умолчанию). Уровень — `RUST_LOG`, по умолчанию `info`.
//!
//! Политика содержания (README, «Журналы»): в журнал не попадают темы, тексты и адреса писем,
//! имена людей, имена файлов вложений, пароли и cookies. Пишутся операции, счётчики, коды
//! ответов, длины, ключи базы (`item_pk`, `attachment_pk`), имена папок и учётная запись.
//!
//! Запись в файл синхронная: строк немного, а буфер фонового писателя потерял бы последние
//! строки при выходе процесса (выход из сессии OWA пишется в самом конце).

use std::path::PathBuf;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

/// Сколько дневных файлов журнала хранить.
pub(crate) const KEEP_FILES: usize = 14;

/// Каталог журналов: `%LOCALAPPDATA%\AIOWA\logs`.
pub(crate) fn log_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("AIOWA").join("logs"))
}

fn file_appender(dir: &std::path::Path) -> Option<RollingFileAppender> {
    std::fs::create_dir_all(dir).ok()?;
    RollingFileAppender::builder().rotation(Rotation::DAILY).filename_prefix("aiowa").filename_suffix("log").max_log_files(KEEP_FILES).build(dir).ok()
}

/// Поднять журнал. Возвращает каталог файлов, если файловый журнал открылся (иначе — только
/// консоль: приложение без журнала на диске всё равно работает).
pub(crate) fn init() -> Option<PathBuf> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let dir = log_dir();
    let file = dir.as_deref().and_then(file_appender);
    let opened = file.is_some();
    let file_layer = file.map(|w| fmt::layer().with_writer(w).with_ansi(false));
    let _ = tracing_subscriber::registry().with(filter).with(fmt::layer()).with(file_layer).try_init();
    dir.filter(|_| opened)
}
