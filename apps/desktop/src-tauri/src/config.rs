//! Конфигурация приложения: `%APPDATA%\AIOWA\config.json` (каталог переопределяется переменной
//! `AIOWA_CONFIG_DIR` — для разработки и тестов).
//!
//! ```json
//! { "version": 1, "account": { "host": "mail.example.ru", "user": "login@domain" }, "db_path": null }
//! ```
//!
//! Пароля в файле нет: он в диспетчере учётных данных Windows (служба `AIOWA`, ключ = логин).
//! Запись атомарная (временный файл рядом и переименование поверх). Чтение терпимо: файла нет —
//! конфига нет; JSON битый — файл переименовывается в `config.json.bad`, и приложение стартует
//! мастером первого запуска.
//!
//! Откуда берётся учётная запись при старте (`resolve`): переменные `AIOWA_HOST` и `AIOWA_USER`
//! (заданы обе — режим разработки, как запускается `tauri dev`; база — `AIOWA_DB` или путь по
//! умолчанию) → `config.json` → режим первого запуска (мастер).
//!
//! Блок `initial` пишет мастер: глубину загрузки тел и «начать синхронизацию сразу» некуда
//! положить, пока базы нет. При первом старте в режиме почты блок применяется к базе и удаляется
//! из файла (`settings::apply_initial`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn};

pub(crate) const CONFIG_FILE: &str = "config.json";
pub(crate) const CONFIG_VERSION: u32 = 1;
/// Переменная окружения с каталогом конфигурации вместо `%APPDATA%\AIOWA`.
pub(crate) const CONFIG_DIR_ENV: &str = "AIOWA_CONFIG_DIR";

/// Учётная запись в конфиге: нормализованный адрес сервера (`host[:port]`) и логин.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AccountConfig {
    pub host: String,
    pub user: String,
}

/// Настройки мастера для первого старта: применяются к новой базе и удаляются из файла.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InitialConfig {
    #[serde(default = "default_initial_body_days")]
    pub body_days: u32,
    #[serde(default = "default_true")]
    pub start_sync: bool,
}

fn default_initial_body_days() -> u32 {
    90
}

fn default_true() -> bool {
    true
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AppConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub account: Option<AccountConfig>,
    /// Путь к базе; `null` — по умолчанию `%LOCALAPPDATA%\AIOWA\<host>\<user>\mail.db`.
    #[serde(default)]
    pub db_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<InitialConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { version: CONFIG_VERSION, account: None, db_path: None, initial: None }
    }
}

impl AppConfig {
    /// Учётная запись, если она заполнена (пустые строки — как будто её нет).
    pub(crate) fn usable_account(&self) -> Option<&AccountConfig> {
        self.account.as_ref().filter(|a| !a.host.trim().is_empty() && !a.user.trim().is_empty())
    }
}

/// Каталог конфигурации: `AIOWA_CONFIG_DIR`, иначе `%APPDATA%\AIOWA`.
pub(crate) fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = dirs::config_dir().or_else(|| std::env::var_os("APPDATA").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("."));
    base.join("AIOWA")
}

pub(crate) fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE)
}

/// Конфиг из каталога. Нет файла или его не прочитать — `None`; битый JSON — файл
/// переименовывается в `config.json.bad` (прежний `.bad` заменяется), тоже `None`.
pub(crate) fn load(dir: &Path) -> Option<AppConfig> {
    let path = config_path(dir);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "не удалось прочитать конфигурацию");
            return None;
        }
    };
    // Блокнот сохраняет UTF-8 с BOM: файл, поправленный руками, не должен считаться битым.
    let json = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    match serde_json::from_slice::<AppConfig>(json) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            let bad = dir.join(format!("{CONFIG_FILE}.bad"));
            match std::fs::rename(&path, &bad) {
                Ok(()) => warn!(path = %path.display(), moved_to = %bad.display(), error = %e, "конфигурация повреждена, отложена в сторону"),
                Err(re) => warn!(path = %path.display(), error = %e, rename_error = %re, "конфигурация повреждена и не переименована"),
            }
            None
        }
    }
}

/// Счётчик имён временных файлов: две записи из одного процесса не делят файл.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Записывает конфиг атомарно: JSON во временный файл рядом, сброс на диск, переименование
/// поверх `config.json`. Читатель видит либо старый файл, либо новый целиком.
pub(crate) fn save(dir: &Path, cfg: &AppConfig) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(cfg).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!("{CONFIG_FILE}.{}.{}.tmp", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, config_path(dir))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Читает конфиг (или пустой), меняет и записывает обратно.
pub(crate) fn update(dir: &Path, f: impl FnOnce(&mut AppConfig)) -> std::io::Result<AppConfig> {
    let mut cfg = load(dir).unwrap_or_default();
    cfg.version = CONFIG_VERSION;
    f(&mut cfg);
    save(dir, &cfg)?;
    Ok(cfg)
}

/// Путь к базе по умолчанию: `%LOCALAPPDATA%\AIOWA\<host>\<user>\mail.db` — общая с CLI функция.
/// Хост и логин очищаются до допустимых имён каталогов: `mail.x.ru:8443` → `mail.x.ru_8443`.
pub(crate) fn default_db_path(host: &str, user: &str) -> PathBuf {
    aiowa_store::default_db_path(host, user)
}

/// Учётная запись из переменных окружения (режим разработки). Пустые значения — как не заданные.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EnvAccount {
    pub host: Option<String>,
    pub user: Option<String>,
    pub db: Option<PathBuf>,
}

impl EnvAccount {
    pub(crate) fn from_env() -> Self {
        let text = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self { host: text("AIOWA_HOST"), user: text("AIOWA_USER"), db: std::env::var_os("AIOWA_DB").filter(|v| !v.is_empty()).map(PathBuf::from) }
    }
}

/// С чем стартует окно почты.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailBoot {
    pub host: String,
    pub user: String,
    pub db_path: PathBuf,
    /// Учётная запись из переменных окружения, а не из `config.json`.
    pub dev_env: bool,
    /// Блок мастера, который надо применить к базе на этом старте.
    pub initial: Option<InitialConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Boot {
    Mail(MailBoot),
    /// Учётной записи нет: мастер первого запуска.
    Setup,
}

/// Порядок: переменные окружения (обе заданы) → конфиг с учётной записью → мастер.
pub(crate) fn resolve(env: &EnvAccount, cfg: Option<&AppConfig>) -> Boot {
    if let (Some(host), Some(user)) = (&env.host, &env.user) {
        let db_path = env.db.clone().unwrap_or_else(|| default_db_path(host, user));
        return Boot::Mail(MailBoot { host: host.clone(), user: user.clone(), db_path, dev_env: true, initial: None });
    }
    let Some(cfg) = cfg else { return Boot::Setup };
    let Some(account) = cfg.usable_account() else { return Boot::Setup };
    let db_path = cfg.db_path.clone().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| default_db_path(&account.host, &account.user));
    Boot::Mail(MailBoot { host: account.host.clone(), user: account.user.clone(), db_path, dev_env: false, initial: cfg.initial })
}

/// Убирает блок `initial` из файла после его применения. Файла нет или блока нет — ничего.
pub(crate) fn clear_initial(dir: &Path) -> std::io::Result<()> {
    let Some(mut cfg) = load(dir) else { return Ok(()) };
    if cfg.initial.take().is_none() {
        return Ok(());
    }
    save(dir, &cfg)?;
    info!("настройки первого запуска применены и убраны из конфигурации");
    Ok(())
}

/// Адрес сервера OWA в виде `host[:port]`: без схемы, пути (`/owa`), запроса и порта 443;
/// имя в нижнем регистре (кириллический домен — в punycode). Принимает `https://host/owa/`,
/// `host`, `host:8443`. Текст ошибки — для пользователя.
pub(crate) fn normalize_host(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("Укажите адрес сервера".into());
    }
    let rest = match s.find("://") {
        Some(i) => {
            let scheme = &s[..i];
            if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
                return Err("Адрес сервера должен начинаться с https:// или быть просто именем сервера".into());
            }
            &s[i + 3..]
        }
        None => s,
    };
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or("").trim();
    if authority.is_empty() {
        return Err("Укажите имя сервера, например mail.example.ru".into());
    }
    if authority.contains('@') {
        return Err("Это похоже на адрес почты, а не сервера: укажите имя сервера OWA, например mail.example.ru".into());
    }
    if authority.chars().any(char::is_whitespace) {
        return Err("В имени сервера не должно быть пробелов".into());
    }
    let url = url::Url::parse(&format!("https://{authority}/")).map_err(|_| format!("Не похоже на адрес сервера: {authority}"))?;
    let host = url.host_str().filter(|h| !h.is_empty()).ok_or_else(|| format!("Не похоже на адрес сервера: {authority}"))?;
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return Err(format!("Не похоже на адрес сервера: {authority}"));
    }
    Ok(match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    })
}

/// Логин: `user@domain`, `DOMAIN\user` или просто имя. Пробелы по краям убираются; пустой,
/// с пробелами внутри или с пустой частью — ошибка.
pub(crate) fn normalize_user(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("Укажите логин".into());
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("В логине не должно быть пробелов".into());
    }
    let parts_ok = |sep: char| s.split_once(sep).is_none_or(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains(sep));
    if !parts_ok('@') || !parts_ok('\\') {
        return Err("Логин указывается как user@domain или DOMAIN\\user".into());
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aiowa-desktop-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample() -> AppConfig {
        AppConfig { account: Some(AccountConfig { host: "mail.x.ru".into(), user: "ivanov@x.ru".into() }), ..AppConfig::default() }
    }

    #[test]
    fn host_normalization() {
        assert_eq!(normalize_host("https://mail.x.ru/owa/").as_deref(), Ok("mail.x.ru"));
        assert_eq!(normalize_host("HTTPS://Mail.X.ru/owa/#path=/mail").as_deref(), Ok("mail.x.ru"));
        assert_eq!(normalize_host("mail.x.ru").as_deref(), Ok("mail.x.ru"));
        assert_eq!(normalize_host("  mail.x.ru  ").as_deref(), Ok("mail.x.ru"));
        assert_eq!(normalize_host("mail.x.ru:8443").as_deref(), Ok("mail.x.ru:8443"));
        assert_eq!(normalize_host("https://mail.x.ru:8443/owa").as_deref(), Ok("mail.x.ru:8443"));
        assert_eq!(normalize_host("mail.x.ru:443").as_deref(), Ok("mail.x.ru"), "порт по умолчанию не пишется");
        assert_eq!(normalize_host("http://mail.x.ru/owa/auth/logon.aspx?replaceCurrent=1").as_deref(), Ok("mail.x.ru"));
        assert_eq!(normalize_host("mail.x.ru.").as_deref(), Ok("mail.x.ru"));
        assert_eq!(normalize_host("10.0.0.5:444").as_deref(), Ok("10.0.0.5:444"));
        assert_eq!(normalize_host("почта.рф").as_deref(), Ok("xn--80a1acny.xn--p1ai"));
        assert!(normalize_host("").is_err());
        assert!(normalize_host("   ").is_err());
        assert!(normalize_host("https://").is_err());
        assert!(normalize_host("https:///owa").is_err());
        assert!(normalize_host("ivanov@x.ru").is_err(), "адрес почты вместо сервера");
        assert!(normalize_host("mail x.ru").is_err());
        assert!(normalize_host("ftp://mail.x.ru").is_err());
        assert!(normalize_host("mail.x.ru:99999").is_err());
        assert!(normalize_host("mail.x.ru:abc").is_err());
    }

    #[test]
    fn user_normalization() {
        assert_eq!(normalize_user(" ivanov@x.ru ").as_deref(), Ok("ivanov@x.ru"));
        assert_eq!(normalize_user("CORP\\ivanov").as_deref(), Ok("CORP\\ivanov"));
        assert_eq!(normalize_user("ivanov").as_deref(), Ok("ivanov"));
        assert!(normalize_user("").is_err());
        assert!(normalize_user("ivan ov@x.ru").is_err());
        assert!(normalize_user("@x.ru").is_err());
        assert!(normalize_user("ivanov@").is_err());
        assert!(normalize_user("CORP\\").is_err());
        assert!(normalize_user("a@b@c").is_err());
    }

    #[test]
    fn config_roundtrip_and_atomic_write() {
        let dir = temp_dir("roundtrip");
        assert_eq!(load(&dir), None, "нет каталога — нет конфига");
        let cfg = sample();
        save(&dir, &cfg).unwrap();
        assert_eq!(load(&dir), Some(cfg.clone()));
        // Файл — валидный JSON нужной формы; `initial` без значения не пишется, `db_path` пишется null.
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(config_path(&dir)).unwrap()).unwrap();
        assert_eq!(raw["version"], 1);
        assert_eq!(raw["account"]["host"], "mail.x.ru");
        assert!(raw["db_path"].is_null() && raw.as_object().unwrap().contains_key("db_path"));
        assert!(!raw.as_object().unwrap().contains_key("initial"));
        // Перезапись поверх существующего файла; временных файлов не остаётся.
        let mut second = cfg.clone();
        second.db_path = Some(PathBuf::from(r"D:\data\mail.db"));
        second.initial = Some(InitialConfig { body_days: 30, start_sync: false });
        save(&dir, &second).unwrap();
        assert_eq!(load(&dir), Some(second));
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n != CONFIG_FILE).collect();
        assert!(leftovers.is_empty(), "лишние файлы: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_json_is_moved_aside() {
        let dir = temp_dir("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(config_path(&dir), b"{ \"version\": 1, \"account\": ").unwrap();
        assert_eq!(load(&dir), None);
        assert!(!config_path(&dir).exists(), "битый файл убран");
        assert_eq!(std::fs::read(dir.join("config.json.bad")).unwrap(), b"{ \"version\": 1, \"account\": ");
        // Повторно битый — заменяет прежний .bad.
        std::fs::write(config_path(&dir), b"not json").unwrap();
        assert_eq!(load(&dir), None);
        assert_eq!(std::fs::read(dir.join("config.json.bad")).unwrap(), b"not json");
        // После этого запись работает как обычно.
        save(&dir, &sample()).unwrap();
        assert_eq!(load(&dir), Some(sample()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tolerant_fields() {
        let dir = temp_dir("tolerant");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(config_path(&dir), br#"{"account":{"host":"h","user":"u"},"future_field":42}"#).unwrap();
        let cfg = load(&dir).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.usable_account(), Some(&AccountConfig { host: "h".into(), user: "u".into() }));
        std::fs::write(config_path(&dir), br#"{"version":1,"account":{"host":" ","user":"u"}}"#).unwrap();
        assert_eq!(load(&dir).unwrap().usable_account(), None, "пустой хост — учётной записи нет");
        std::fs::write(config_path(&dir), b"\xEF\xBB\xBF{\"version\":1,\"account\":{\"host\":\"h\",\"user\":\"u\"},\"db_path\":null}").unwrap();
        assert!(load(&dir).unwrap().usable_account().is_some(), "UTF-8 с BOM читается");
        assert!(!dir.join("config.json.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_has_priority_over_config() {
        let cfg = AppConfig { db_path: Some(PathBuf::from(r"D:\cfg\mail.db")), initial: Some(InitialConfig { body_days: 30, start_sync: true }), ..sample() };
        let env = EnvAccount { host: Some("dev.host".into()), user: Some("dev@user".into()), db: Some(PathBuf::from(r"D:\dev\mail.db")) };
        match resolve(&env, Some(&cfg)) {
            Boot::Mail(m) => {
                assert_eq!((m.host.as_str(), m.user.as_str()), ("dev.host", "dev@user"));
                assert_eq!(m.db_path, PathBuf::from(r"D:\dev\mail.db"));
                assert!(m.dev_env);
                assert_eq!(m.initial, None, "блок мастера в режиме разработки не применяется");
            }
            Boot::Setup => panic!("ожидался режим почты"),
        }
        // Только одна переменная — не режим разработки: берётся конфиг.
        let half = EnvAccount { host: Some("dev.host".into()), ..EnvAccount::default() };
        match resolve(&half, Some(&cfg)) {
            Boot::Mail(m) => {
                assert_eq!((m.host.as_str(), m.user.as_str()), ("mail.x.ru", "ivanov@x.ru"));
                assert_eq!(m.db_path, PathBuf::from(r"D:\cfg\mail.db"));
                assert!(!m.dev_env);
                assert_eq!(m.initial, Some(InitialConfig { body_days: 30, start_sync: true }));
            }
            Boot::Setup => panic!("ожидался режим почты"),
        }
        // Конфиг без пути к базе — путь по умолчанию; без переменных AIOWA_DB.
        let plain = sample();
        match resolve(&EnvAccount::default(), Some(&plain)) {
            Boot::Mail(m) => assert_eq!(m.db_path, default_db_path("mail.x.ru", "ivanov@x.ru")),
            Boot::Setup => panic!("ожидался режим почты"),
        }
        // Режим разработки без AIOWA_DB — путь по умолчанию от переменных.
        let env_no_db = EnvAccount { db: None, ..env };
        match resolve(&env_no_db, None) {
            Boot::Mail(m) => assert_eq!(m.db_path, default_db_path("dev.host", "dev@user")),
            Boot::Setup => panic!("ожидался режим почты"),
        }
        assert_eq!(resolve(&EnvAccount::default(), None), Boot::Setup);
        assert_eq!(resolve(&EnvAccount::default(), Some(&AppConfig::default())), Boot::Setup, "учётная запись забыта");
    }

    #[test]
    fn default_db_path_accepts_host_with_port() {
        // Мастер допускает «сервер:порт» и IPv6: имя каталога базы не должно их содержать,
        // иначе Store::open не создаст каталог.
        for host in ["mail.x.ru:8443", "[::1]", "[::1]:8443"] {
            let host = normalize_host(host).unwrap();
            let path = default_db_path(&host, "ivanov@x.ru");
            let host_dir = path.parent().and_then(Path::parent).and_then(Path::file_name).unwrap().to_string_lossy().into_owned();
            assert!(!host_dir.contains([':', '[', ']']), "{host} → {host_dir}");
            let dir = temp_dir("dbpath");
            let under = aiowa_store::db_path_under(&dir, &host, "ivanov@x.ru");
            std::fs::create_dir_all(under.parent().unwrap()).unwrap_or_else(|e| panic!("{}: {e}", under.display()));
            let _ = std::fs::remove_dir_all(&dir);
        }
        let path = default_db_path("mail.x.ru", "ivanov@x.ru");
        assert!(path.ends_with(Path::new("AIOWA").join("mail.x.ru").join("ivanov_x.ru").join("mail.db")), "имя без порта не меняется: {}", path.display());
    }

    #[test]
    fn update_keeps_other_fields_and_clear_initial() {
        let dir = temp_dir("update");
        let cfg = AppConfig { db_path: Some(PathBuf::from(r"D:\x\mail.db")), initial: Some(InitialConfig { body_days: 365, start_sync: false }), ..sample() };
        save(&dir, &cfg).unwrap();
        clear_initial(&dir).unwrap();
        let after = load(&dir).unwrap();
        assert_eq!(after.initial, None);
        assert_eq!(after.db_path, cfg.db_path);
        assert_eq!(after.account, cfg.account);
        // Забыть учётную запись: путь к базе остаётся.
        let forgotten = update(&dir, |c| c.account = None).unwrap();
        assert_eq!(forgotten.db_path, cfg.db_path);
        assert_eq!(load(&dir).unwrap().account, None);
        // Без файла `clear_initial` ничего не создаёт.
        let empty = temp_dir("update-empty");
        clear_initial(&empty).unwrap();
        assert!(!config_path(&empty).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
