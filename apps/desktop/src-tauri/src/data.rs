//! Базы на диске: вкладка «Данные» настроек (все базы `%LOCALAPPDATA%\AIOWA\*\*\mail.db` с
//! размером, письмами и временем синхронизации; «Открыть папку» и удаление лишней), размер базы
//! в строке состояния (`data_usage`), сводки баз для мастера (`mailbox.rs`).
//!
//! Чужие базы читаются только на чтение и без миграций (`aiowa_store::read_summary`). Подсчёт
//! писем дорогой (около полсекунды на 150 тыс. писем), а вкладка опрашивает список раз в
//! несколько секунд: пока файлы базы не менялись, счётчики берутся из памяти, у базы, в которую
//! идёт запись, пересчитываются не чаще раза в `COUNT_REFRESH`. Размеры на диске — всегда свежие.
//!
//! Удаление — только не текущей базы и только каталога `<корень>\<сервер>\<ящик>`: каталог сначала
//! переименовывается (если базу держит другая программа, Windows не даст, и ничего не удалится),
//! затем удаляется целиком. Письма на сервере это не трогает. Если файл внутри всё же оказался
//! занят и каталог удалился не до конца, остаток (`<ящик>.deleting-<время>`) не прячется: он в
//! списке строкой «Остаток удалённой базы» с размером, его можно открыть и удалить повторно.
//! Базу остатка список не открывает: ящик — из имени каталога, размер — по файлам.
//!
//! Список и удаление не идут одновременно (`disk_lock`): опрос вкладки открывает базы на чтение,
//! и открытый в этот момент файл не дал бы переименовать или удалить каталог.

use crate::{err, AppState, CmdResult};
use aiowa_store::identity::{self, DELETING_MARK};
use aiowa_store::DbSummary;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tauri::State;
use tracing::{info, warn};

/// Как часто пересчитывать письма базы, файлы которой меняются (идёт синхронизация).
const COUNT_REFRESH: Duration = Duration::from_secs(15);

/// Отпечаток файлов базы: размер и время изменения файла и журнала.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    db: Option<(u64, SystemTime)>,
    wal: Option<(u64, SystemTime)>,
}

fn file_stamp(p: &Path) -> Option<(u64, SystemTime)> {
    std::fs::metadata(p).ok().and_then(|m| Some((m.len(), m.modified().ok()?)))
}

fn stamp(db: &Path) -> Stamp {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    Stamp { db: file_stamp(db), wal: file_stamp(Path::new(&wal)) }
}

struct Cached {
    stamp: Stamp,
    at: Instant,
    summary: DbSummary,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Сводка базы с памятью счётчиков (см. описание модуля); размеры на диске — всегда свежие.
pub(crate) fn summary_cached(path: &Path) -> Result<DbSummary, String> {
    let st = stamp(path);
    {
        let c = cache().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(hit) = c.get(path)
            && (hit.stamp == st || hit.at.elapsed() < COUNT_REFRESH)
        {
            let mut s = hit.summary.clone();
            s.db_bytes = identity::db_files_bytes(path);
            s.cache_bytes = identity::dir_bytes(&identity::attachments_dir_for(path));
            return Ok(s);
        }
    }
    let s = aiowa_store::read_summary(path).map_err(|e| e.to_string())?;
    cache().lock().unwrap_or_else(|p| p.into_inner()).insert(path.to_path_buf(), Cached { stamp: st, at: Instant::now(), summary: s.clone() });
    Ok(s)
}

fn forget_cached(path: &Path) {
    cache().lock().unwrap_or_else(|p| p.into_inner()).remove(path);
}

/// Один на список баз и удаление: пока вкладка читает базы (а сводка открывает файл), каталог не
/// переименовывается и не удаляется, и наоборот — опрос не открывает базу посреди удаления.
fn disk_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Один и тот же файл: по полному пути, без учёта регистра (Windows); путь, которого нет, — как есть.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase();
    norm(a) == norm(b)
}

/// База для вкладки «Данные».
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct StoreDto {
    /// Полный путь к `mail.db`.
    path: String,
    /// Каталог базы (удаляется целиком).
    dir: String,
    /// Сервер и логины учётных записей базы (у базы обычно одна). У остатка удаления — имена
    /// каталогов сервера и ящика (`<сервер>\<ящик>.deleting-<время>`): базу остатка не открывают.
    host: Option<String>,
    logins: Vec<String>,
    /// Адрес ящика, если записан (базы новых версий).
    email: Option<String>,
    /// В базе записан `MailboxGuid` ящика.
    has_guid: bool,
    items: i64,
    last_sync_ms: Option<i64>,
    /// Файл базы с `-wal` и `-shm`.
    db_bytes: u64,
    /// Каталог кэша вложений базы.
    cache_bytes: u64,
    total_bytes: u64,
    /// Текущая база (открыта приложением): её не удалить.
    current: bool,
    /// Остаток неполного удаления (`<ящик>.deleting-<время>`): не база, а место на диске, которое
    /// осталось занятым. `path` — `mail.db` внутри него (файла может уже не быть), `error` —
    /// «не удалено до конца». Окно называет строку «Остаток удалённой базы» и спрашивает
    /// «Удалить остаток?».
    leftover: bool,
    /// База не прочиталась (повреждена, не база, нет доступа) или это остаток удаления: текст для окна.
    error: Option<String>,
}

/// Строка остатка в списке (окно показывает `error` вместо писем и времени синхронизации).
const LEFTOVER_TEXT: &str = "Не удалено до конца: каталог ещё занимает место. Удалите его повторно; если не выходит — закройте программу, которая держит файлы (например, консольную aiowa), или удалите каталог вручную";

fn store_dto(path: &Path, current: &Path) -> StoreDto {
    let base = StoreDto {
        path: path.display().to_string(),
        dir: path.parent().map(|d| d.display().to_string()).unwrap_or_default(),
        current: same_path(path, current),
        ..StoreDto::default()
    };
    match summary_cached(path) {
        Ok(s) => {
            let id = s.identity();
            StoreDto {
                host: s.accounts.first().map(|a| a.host.clone()),
                logins: s.accounts.iter().map(|a| a.login.clone()).collect(),
                email: id.email,
                has_guid: id.mailbox_guid.is_some(),
                items: s.items,
                last_sync_ms: s.last_sync_ms,
                db_bytes: s.db_bytes,
                cache_bytes: s.cache_bytes,
                total_bytes: s.total_bytes(),
                ..base
            }
        }
        Err(e) => {
            let db_bytes = identity::db_files_bytes(path);
            let cache_bytes = identity::dir_bytes(&identity::attachments_dir_for(path));
            StoreDto { error: Some(format!("База не читается: {e}")), db_bytes, cache_bytes, total_bytes: db_bytes + cache_bytes, ..base }
        }
    }
}

/// Остаток неполного удаления: размер каталога целиком (только по файлам), ящик — из имени
/// каталога. Базу остатка не открывать: открытый файл помешал бы удалить остаток повторно, а
/// сводка ничего не добавляет — это уже не база.
fn leftover_dto(dir: &Path) -> StoreDto {
    let db = dir.join("mail.db");
    let total_bytes = identity::dir_bytes(dir);
    let db_bytes = identity::db_files_bytes(&db);
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    let mailbox = name(dir).and_then(|n| n.rsplit_once(DELETING_MARK).map(|(base, _)| base.to_string())).filter(|n| !n.is_empty());
    StoreDto {
        path: db.display().to_string(),
        dir: dir.display().to_string(),
        host: dir.parent().and_then(name),
        logins: mailbox.into_iter().collect(),
        db_bytes,
        cache_bytes: total_bytes.saturating_sub(db_bytes),
        total_bytes,
        leftover: true,
        error: Some(LEFTOVER_TEXT.into()),
        ..StoreDto::default()
    }
}

/// Все базы под `root` и текущая (даже если она в другом месте), затем остатки неполного удаления:
/// текущая первой, остальное по пути. Не одновременно с удалением (`disk_lock`).
fn list_stores(root: &Path, current: &Path) -> Vec<StoreDto> {
    let _lock = disk_lock();
    let mut paths = identity::scan_databases(root);
    if !paths.iter().any(|p| same_path(p, current)) {
        paths.push(current.to_path_buf());
    }
    let mut out: Vec<StoreDto> = paths.iter().map(|p| store_dto(p, current)).collect();
    out.extend(identity::scan_leftovers(root).iter().map(|d| leftover_dto(d)));
    out.sort_by(|a, b| b.current.cmp(&a.current).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Путь из окна → одна из баз списка или `mail.db` остатка удаления (файла может уже не быть);
/// иначе ошибка (окно не может назвать произвольный путь).
fn known_store(root: &Path, current: &Path, path: &str) -> Result<PathBuf, String> {
    let wanted = PathBuf::from(path);
    if same_path(&wanted, current) {
        return Ok(current.to_path_buf());
    }
    identity::scan_databases(root)
        .into_iter()
        .chain(identity::scan_leftovers(root).into_iter().map(|d| d.join("mail.db")))
        .find(|p| same_path(p, &wanted))
        .ok_or_else(|| "Такой базы нет в списке: обновите вкладку «Данные»".to_string())
}

/// Удалить каталог не текущей базы под `root` (или повторно — остаток неполного удаления);
/// возвращает освобождённые байты. Не одновременно со списком (`disk_lock`): опрос вкладки не
/// откроет базу, пока каталог переименовывается и удаляется.
fn delete_store(root: &Path, current: &Path, path: &str) -> Result<u64, String> {
    let _lock = disk_lock();
    let db = known_store(root, current, path)?;
    if same_path(&db, current) {
        return Err("Текущую базу удалить нельзя: приложение работает с ней".into());
    }
    let dir = db.parent().map(Path::to_path_buf).ok_or_else(|| "Каталог базы не определён".to_string())?;
    // Только `<корень>\<сервер>\<ящик>\mail.db`: выше по дереву ничего не удаляется.
    let under_root = dir.parent().and_then(Path::parent).is_some_and(|r| same_path(r, root));
    if !under_root || db.file_name().is_none_or(|n| n != "mail.db") {
        return Err("Удалять можно только базы в каталоге приложения".into());
    }
    let freed = identity::dir_bytes(&dir);
    forget_cached(&db);
    let leftover = identity::is_deleting_dir(&dir);
    let doomed = if leftover {
        // Остаток уже переименован: удаляется как есть.
        dir.clone()
    } else {
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let doomed = dir.with_file_name(format!("{name}{DELETING_MARK}{}", jiff::Timestamp::now().as_millisecond()));
        std::fs::rename(&dir, &doomed).map_err(|e| {
            warn!(error = %e, "каталог базы не переименован перед удалением");
            "База занята: её держит другая программа (например, консольная утилита aiowa). Закройте её и повторите".to_string()
        })?;
        doomed
    };
    if let Err(e) = std::fs::remove_dir_all(&doomed) {
        let left = identity::dir_bytes(&doomed);
        warn!(error = %e, leftover, left_mb = left / (1024 * 1024), "каталог базы удалён не полностью");
        return Err(format!("Каталог удалён не до конца ({e}): остаток в списке строкой «Остаток удалённой базы». Закройте программу, которая держит файлы, и удалите его ещё раз"));
    }
    // Каталог сервера опустел — убрать и его.
    if let Some(host_dir) = dir.parent()
        && std::fs::read_dir(host_dir).is_ok_and(|mut rd| rd.next().is_none())
    {
        let _ = std::fs::remove_dir(host_dir);
    }
    info!(freed_mb = freed / (1024 * 1024), leftover, "лишняя база удалена с диска");
    Ok(freed)
}

// --- команды Tauri -----------------------------------------------------------

/// Все базы на диске для вкладки «Данные».
#[tauri::command]
pub(crate) async fn data_stores(state: State<'_, AppState>) -> CmdResult<Vec<StoreDto>> {
    let current = state.store.path().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || list_stores(&identity::data_root(), &current)).await.map_err(err)
}

/// Место на диске под текущую базу (строка состояния).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageDto {
    db_bytes: u64,
    cache_bytes: u64,
}

#[tauri::command]
pub(crate) async fn data_usage(state: State<'_, AppState>) -> CmdResult<UsageDto> {
    let db = state.store.path().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || UsageDto { db_bytes: identity::db_files_bytes(&db), cache_bytes: identity::dir_bytes(&identity::attachments_dir_for(&db)) }).await.map_err(err)
}

/// Открыть каталог базы из списка в Проводнике.
#[tauri::command]
pub(crate) async fn data_store_open_dir(path: String, state: State<'_, AppState>) -> CmdResult<()> {
    let current = state.store.path().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || -> CmdResult<()> {
        let db = known_store(&identity::data_root(), &current, &path)?;
        let dir = db.parent().map(Path::to_path_buf).ok_or_else(|| "Каталог базы не определён".to_string())?;
        open::that_detached(&dir).map_err(|e| {
            warn!(error = %e, "Проводник не открыл каталог базы");
            "Не удалось открыть каталог базы".to_string()
        })
    })
    .await
    .map_err(err)?
}

/// Удалить не текущую базу (каталог целиком). Окно спрашивает подтверждение до вызова.
#[tauri::command]
pub(crate) async fn data_store_delete(path: String, state: State<'_, AppState>) -> CmdResult<u64> {
    let current = state.store.path().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || delete_store(&identity::data_root(), &current, &path)).await.map_err(err)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_store::Store;

    fn root(tag: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("aiowa-desktop-data-{tag}-{}", std::process::id())).join("AIOWA");
        let _ = std::fs::remove_dir_all(&r);
        r
    }

    fn make_db(path: &Path, login: &str) {
        let s = Store::open(path).unwrap();
        s.account("mail.x.ru", login).unwrap();
    }

    #[test]
    fn stores_are_listed_with_current_first() {
        let root = root("list");
        let current = root.join("mail.x.ru").join("new").join("mail.db");
        let old = root.join("mail.x.ru").join("old").join("mail.db");
        make_db(&current, "savvidi");
        make_db(&old, "savvidi@ngenergo");
        std::fs::create_dir_all(identity::attachments_dir_for(&old)).unwrap();
        std::fs::write(identity::attachments_dir_for(&old).join("1-a.bin"), [0u8; 10]).unwrap();
        // Не база — в списке с ошибкой, а не пропадает.
        let junk = root.join("mail.x.ru").join("junk").join("mail.db");
        std::fs::create_dir_all(junk.parent().unwrap()).unwrap();
        std::fs::write(&junk, b"not a database, just some text here").unwrap();
        let list = list_stores(&root, &current);
        assert_eq!(list.len(), 3);
        assert!(list[0].current && list[0].logins == vec!["savvidi".to_string()]);
        let old_dto = list.iter().find(|s| s.logins == vec!["savvidi@ngenergo".to_string()]).unwrap();
        assert!(!old_dto.current);
        assert_eq!(old_dto.cache_bytes, 10);
        assert_eq!(old_dto.total_bytes, old_dto.db_bytes + 10);
        assert!(list.iter().any(|s| s.error.is_some() && s.path.ends_with("mail.db") && s.dir.ends_with("junk")));
        // Текущая база вне корня тоже в списке.
        let elsewhere = root.parent().unwrap().join("dev").join("mail.db");
        make_db(&elsewhere, "dev");
        let list = list_stores(&root, &elsewhere);
        assert_eq!(list.len(), 4);
        assert!(list[0].current && list[0].logins == vec!["dev".to_string()]);
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn only_other_stores_under_root_are_deleted() {
        let root = root("delete");
        let current = root.join("mail.x.ru").join("new").join("mail.db");
        let old = root.join("mail.x.ru").join("old").join("mail.db");
        make_db(&current, "savvidi");
        make_db(&old, "savvidi@ngenergo");
        std::fs::create_dir_all(identity::attachments_dir_for(&old).join("sub")).unwrap();
        std::fs::write(identity::attachments_dir_for(&old).join("sub").join("2-b.bin"), [0u8; 7]).unwrap();
        let outside = root.parent().unwrap().join("dev").join("mail.db");
        make_db(&outside, "dev");

        assert!(delete_store(&root, &current, &current.display().to_string()).unwrap_err().contains("Текущую"));
        assert!(delete_store(&root, &current, &outside.display().to_string()).is_err(), "база вне каталога приложения");
        assert!(outside.exists());
        assert!(delete_store(&root, &current, &root.join("mail.x.ru").join("nope").join("mail.db").display().to_string()).is_err());

        // Путь в другом регистре — та же база.
        let freed = delete_store(&root, &current, &old.display().to_string().to_uppercase()).unwrap();
        assert!(freed >= 7);
        assert!(!old.parent().unwrap().exists(), "каталог базы удалён целиком");
        assert!(current.exists(), "текущая на месте");
        assert!(!std::fs::read_dir(root.join("mail.x.ru")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(DELETING_MARK)));

        // Последняя база сервера — каталог сервера убирается тоже.
        let other_host = root.join("mail.y.ru").join("a").join("mail.db");
        make_db(&other_host, "a");
        delete_store(&root, &current, &other_host.display().to_string()).unwrap();
        assert!(!root.join("mail.y.ru").exists());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn leftovers_of_deletion_are_listed_and_deleted_again() {
        let root = root("leftover");
        let current = root.join("mail.x.ru").join("new").join("mail.db");
        make_db(&current, "savvidi");
        // Остаток с файлом базы (его держали) и остаток, где осталось только вложение.
        let with_db = root.join("mail.x.ru").join(format!("old{DELETING_MARK}1761390000000"));
        make_db(&with_db.join("mail.db"), "savvidi@ngenergo");
        std::fs::create_dir_all(with_db.join("attachments")).unwrap();
        std::fs::write(with_db.join("attachments").join("1-a.bin"), [0u8; 5]).unwrap();
        let bare = root.join("mail.y.ru").join(format!("a{DELETING_MARK}1"));
        std::fs::create_dir_all(bare.join("attachments")).unwrap();
        std::fs::write(bare.join("attachments").join("2-b.bin"), [0u8; 9]).unwrap();

        let list = list_stores(&root, &current);
        assert_eq!(list.len(), 3, "текущая и два остатка");
        assert!(list[0].current && !list[0].leftover);
        let l = list.iter().find(|s| s.dir == with_db.display().to_string()).unwrap();
        assert!(l.leftover && !l.current);
        assert!(l.error.as_deref().is_some_and(|e| e.starts_with("Не удалено до конца")));
        // Ящик и сервер — из имён каталогов: базу остатка список не открывает (сводки нет и в памяти).
        assert_eq!((l.host.as_deref(), l.logins.clone()), (Some("mail.x.ru"), vec!["old".to_string()]));
        assert!(l.email.is_none() && !l.has_guid && l.items == 0);
        assert!(!cache().lock().unwrap().contains_key(&with_db.join("mail.db")), "база остатка не открывалась");
        assert!(l.db_bytes > 0, "файлы базы остатка посчитаны");
        assert_eq!(l.total_bytes, identity::dir_bytes(&with_db));
        assert_eq!(l.db_bytes + l.cache_bytes, l.total_bytes);
        let b = list.iter().find(|s| s.dir == bare.display().to_string()).unwrap();
        assert!(b.leftover && b.db_bytes == 0);
        assert_eq!((b.host.as_deref(), b.logins.clone()), (Some("mail.y.ru"), vec!["a".to_string()]));
        assert_eq!(b.total_bytes, 9);
        // «Открыть папку» и удаление принимают путь остатка из списка.
        assert_eq!(known_store(&root, &current, &b.path).unwrap().parent(), Some(bare.as_path()));
        assert!(identity::scan_databases(&root).iter().all(|p| !identity::is_deleting_dir(p.parent().unwrap())), "остаток не база: мастер его не предлагает");

        // Файл остатка всё ещё занят — повтор не удаётся, остаток остаётся в списке.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_SHARE_READ: u32 = 1;
            let held = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(bare.join("attachments").join("2-b.bin")).unwrap();
            let e = delete_store(&root, &current, &b.path).unwrap_err();
            assert!(e.contains("не до конца"), "{e}");
            assert!(list_stores(&root, &current).iter().any(|s| s.leftover && s.dir == b.dir));
            drop(held);
        }
        assert_eq!(delete_store(&root, &current, &b.path).unwrap(), 9);
        assert!(!bare.exists() && !root.join("mail.y.ru").exists(), "остаток и опустевший каталог сервера удалены");
        assert!(delete_store(&root, &current, &l.path.to_uppercase()).unwrap() >= 5);
        assert!(!with_db.exists());
        assert_eq!(list_stores(&root, &current).len(), 1);
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn listing_waits_for_deletion() {
        // Опрос вкладки идёт непрерывно и каждый раз открывает базы заново (память сводок
        // сбрасывается); удаления при этом не срываются: список и удаление не пересекаются.
        let root = root("race");
        let current = root.join("mail.x.ru").join("new").join("mail.db");
        make_db(&current, "savvidi");
        let dbs: Vec<PathBuf> = (0..2).map(|i| root.join("mail.x.ru").join(format!("old{i}")).join("mail.db")).collect();
        for db in &dbs {
            make_db(db, "u");
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (started, wait_started) = std::sync::mpsc::channel();
        let poller = std::thread::spawn({
            let (root, current, dbs, stop) = (root.clone(), current.clone(), dbs.clone(), stop.clone());
            move || loop {
                for db in &dbs {
                    forget_cached(db);
                }
                list_stores(&root, &current);
                let _ = started.send(());
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
            }
        });
        wait_started.recv().unwrap();
        // Много раундов «завести → удалить» на тех же базах, пока опрос крутится: без замка удаление
        // хотя бы раз застаёт открытую опросом базу и падает (один раунд ловил это через раз).
        for round in 0..25 {
            for db in &dbs {
                if round > 0 {
                    make_db(db, "u");
                }
                delete_store(&root, &current, &db.display().to_string()).unwrap_or_else(|e| panic!("раунд {round}: {e}"));
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        poller.join().unwrap();
        let list = list_stores(&root, &current);
        assert_eq!(list.len(), 1, "удалены все, остатков нет");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn summary_counts_are_cached_while_files_are_unchanged() {
        let root = root("cache");
        let db = root.join("h").join("a").join("mail.db");
        make_db(&db, "u");
        let first = summary_cached(&db).unwrap();
        assert_eq!(first.accounts.len(), 1);
        // Размер кэша вложений меняется без изменения базы — виден сразу.
        std::fs::create_dir_all(identity::attachments_dir_for(&db)).unwrap();
        std::fs::write(identity::attachments_dir_for(&db).join("x"), [0u8; 3]).unwrap();
        assert_eq!(summary_cached(&db).unwrap().cache_bytes, 3);
        assert!(same_path(&db, Path::new(&db.display().to_string().to_uppercase())));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }
}
