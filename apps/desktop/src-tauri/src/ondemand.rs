//! Команды окна «по требованию»: тело письма с инлайновыми картинками, открытие и сохранение
//! вложений, внешние ссылки.
//!
//! Сеть — только через транспорт процесса (`SyncState::transport`): входов здесь нет, лимит
//! сессий на ящик мал. Каждый сетевой шаг ограничен таймаутом; ошибка мёртвой сессии сбрасывает
//! транспорт через `SyncState::invalidate_on`. Диск, диалог и ShellExecute выполняются в
//! блокирующем потоке. Тексты ошибок — фиксированные фразы для пользователя, без ответов сервера.

use crate::sync::SyncState;
use crate::{build_message, err, AppState, CmdResult, MessageDto, INLINE_IMAGE_MAX_BYTES};
use aiowa_store::{AttachmentRow, Store, FETCH_FAILED};
use aiowa_sync::{BodyFetchOutcome, Engine, SyncError, SyncOptions};
use aiowa_transport::TransportError;
use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tracing::{info, warn};

/// Таймаут загрузки тела письма и каждой инлайновой картинки.
const BODY_TIMEOUT: Duration = Duration::from_secs(60);
/// Таймаут скачивания вложения по клику.
const ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Лимит вложения, скачиваемого по клику (не меньше лимита синхронизации из настроек).
const ATTACHMENT_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Расширения, которые системное приложение не откроет, а выполнит (или откроет без защищённого
/// просмотра): исполняемые файлы, скрипты, установщики, ярлыки, образы дисков. Такие вложения
/// можно только сохранить — решение о запуске пользователь принимает сам, уже с меткой зоны.
const UNSAFE_EXTENSIONS: &[&str] = &[
    "exe", "com", "scr", "pif", "bat", "cmd", "ps1", "psm1", "psd1", "ps1xml", "js", "jse", "vbs", "vbe", "vb", "wsf", "wsh", "ws", "wsc", "hta", "msi", "msp", "mst", "lnk",
    "url", "reg", "cpl", "jar", "jnlp", "iso", "img", "vhd", "vhdx", "application", "appx", "appxbundle", "msix", "msixbundle", "appinstaller", "gadget", "inf",
    "scf", "sct", "shb", "shs", "chm", "diagcab", "py", "pyw", "dll", "sys", "drv", "ocx", "msc", "xbap", "settingcontent-ms", "library-ms", "searchconnector-ms",
    "website", "desktop",
];

/// Состояние команд по требованию: замки по письму и по вложению. Повторный вызов по тому же
/// ключу (двойной клик, второй эффект карточки) ждёт первый и видит готовый результат,
/// а не идёт в сеть ещё раз.
#[derive(Default)]
pub(crate) struct OnDemand {
    items: KeyedLocks,
    attachments: KeyedLocks,
}

/// Асинхронные замки по ключу; записи живут, пока замком кто-то пользуется. Ключ — число
/// (письмо, вложение, событие календаря) или кортеж (окно календаря).
pub(crate) struct KeyedLocks<K = i64>(Mutex<HashMap<K, Weak<tokio::sync::Mutex<()>>>>);

impl<K> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
}

impl<K: Eq + Hash> KeyedLocks<K> {
    pub(crate) fn get(&self, key: K) -> Arc<tokio::sync::Mutex<()>> {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        g.retain(|_, w| w.strong_count() > 0);
        if let Some(existing) = g.get(&key).and_then(Weak::upgrade) {
            return existing;
        }
        let fresh = Arc::new(tokio::sync::Mutex::new(()));
        g.insert(key, Arc::downgrade(&fresh));
        fresh
    }
}

/// Код отказа сервера «ключ изменений устарел»: у `ReferenceItemId` ответа и пересылки, у
/// `UpdateItem` без `AlwaysOverwrite`.
pub(crate) const STALE_OBJECT: &str = "ErrorStaleObject";

/// Отказ сервера из-за устаревшего ключа изменений.
pub(crate) fn is_stale_object(e: &TransportError) -> bool {
    matches!(e, TransportError::Server { code, .. } if code == STALE_OBJECT)
}

/// Текст ошибки транспорта для пользователя: фиксированные фразы. Ответ сервера (`Protocol::raw`
/// бывает HTML страницы) и подробности входа в окно не попадают, они остаются в журнале.
pub(crate) fn describe_transport(e: &TransportError) -> String {
    match e {
        TransportError::SessionExpired | TransportError::CsrfRejected => "Сессия истекла, повторите попытку".into(),
        TransportError::SessionLimit => "Сервер занят: исчерпан лимит сессий на ящик, повторите позже".into(),
        TransportError::AuthFailed(_) => "Вход отклонён сервером: проверьте пароль (настройки → «Сменить пароль…»)".into(),
        TransportError::Throttled { .. } => "Сервер занят, повторите позже".into(),
        TransportError::Network(_) => "Нет связи с сервером".into(),
        TransportError::Protocol { status: 0, .. } => "Сервер ответил неожиданно".into(),
        TransportError::Protocol { status, .. } => format!("Сервер ответил неожиданно (HTTP {status})"),
        TransportError::Server { code, .. } if code == STALE_OBJECT => "Письмо на сервере изменилось, повторите попытку".into(),
        TransportError::Server { code, .. } => format!("Сервер отказал: {code}"),
        TransportError::Cancelled => "Отменено".into(),
        TransportError::TooLarge { limit } => format!("Файл больше лимита {} МБ", limit / (1024 * 1024)),
    }
}

fn describe_sync(e: &SyncError) -> String {
    match e {
        SyncError::Transport(t) => describe_transport(t),
        SyncError::Cancelled => "Отменено".into(),
        SyncError::Store(s) => format!("Ошибка локальной базы: {s}"),
        other => other.to_string(),
    }
}

/// Ошибка сетевого шага: при мёртвой сессии сбрасывает транспорт процесса (следующее обращение
/// войдёт заново), возвращает текст для пользователя.
async fn fail(sync: &SyncState, e: SyncError) -> String {
    if let SyncError::Transport(t) = &e {
        sync.invalidate_on(t).await;
    }
    describe_sync(&e)
}

/// Движок на транспорте процесса. Если транспорт уже есть, сети здесь нет; иначе один вход под
/// замком `SyncState` (параллельные вызывающие ждут его, а не входят сами).
async fn engine_for(state: &AppState, sync: &SyncState) -> CmdResult<Engine> {
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    Ok(Engine::new(Arc::new(transport), state.store.clone(), state.account, Arc::new(AtomicBool::new(false))))
}

pub(crate) fn timed_out(limit: Duration) -> String {
    if limit.as_secs() >= 120 { format!("Сервер не ответил за {} мин, повторите попытку", limit.as_secs() / 60) } else { format!("Сервер не ответил за {} с, повторите попытку", limit.as_secs()) }
}

/// Инлайновые картинки письма, которых нет в кэше: есть Content-ID, тип `image/*` или
/// неизвестен, файла на диске нет, размер в лимите, прошлая попытка не кончилась отказом сервера.
fn pending_inline(store: &Store, item_pk: i64) -> CmdResult<Vec<AttachmentRow>> {
    let rows = store.attachments_of_item(item_pk).map_err(err)?;
    Ok(rows
        .into_iter()
        .filter(|a| {
            a.is_inline
                && a.content_id.is_some()
                && (a.content_type.is_none() || a.is_image())
                && a.text_state != FETCH_FAILED
                && !a.size.is_some_and(|s| s as u64 > INLINE_IMAGE_MAX_BYTES)
                && !a.local_path.as_deref().is_some_and(Path::is_file)
        })
        .collect())
}

/// Расширение имени вложения (как оно ляжет на диск) в нижнем регистре.
fn extension_of(name: &str) -> Option<String> {
    let safe = aiowa_store::safe_file_name(name);
    let (_, ext) = safe.rsplit_once('.')?;
    let ext = ext.trim().to_ascii_lowercase();
    (!ext.is_empty()).then_some(ext)
}

/// Открывать системным приложением небезопасно: файл будет выполнен, а не показан.
pub(crate) fn is_unsafe_to_open(name: Option<&str>) -> bool {
    name.and_then(extension_of).is_some_and(|ext| UNSAFE_EXTENSIONS.contains(&ext.as_str()))
}

/// Лимит вложения по клику: не меньше лимита синхронизации из настроек.
fn click_limit(store: &Store, account: aiowa_core::AccountId) -> u64 {
    ATTACHMENT_MAX_BYTES.max(crate::settings::Prefs::load(store, account).max_attachment_bytes())
}

/// Путь к файлу вложения в кэше: скачивает, если файла нет (один раз на вложение, повторный клик
/// ждёт), и отмечает обращение для вытеснения по давности.
pub(crate) async fn ensure_cached(state: &AppState, sync: &SyncState, od: &OnDemand, attachment_pk: i64) -> CmdResult<PathBuf> {
    let lock = od.attachments.get(attachment_pk);
    let _busy = lock.lock().await;
    let store = &state.store;
    // Пока ждали замок, файл мог появиться.
    let row = store.attachment(attachment_pk).map_err(err)?.ok_or_else(|| "Вложение не найдено".to_string())?;
    let path = match row.local_path.clone().filter(|p| p.is_file()) {
        Some(p) => p,
        None => {
            let limit = click_limit(store, state.account);
            if row.size.is_some_and(|s| s as u64 > limit) {
                return Err(format!("Файл больше лимита {} МБ", limit / (1024 * 1024)));
            }
            let engine = engine_for(state, sync).await?;
            let text_chars = SyncOptions::default().attachment_text_chars;
            match tokio::time::timeout(ATTACHMENT_TIMEOUT, engine.fetch_one_attachment(&row.todo(), limit, text_chars)).await {
                Err(_) => return Err(timed_out(ATTACHMENT_TIMEOUT)),
                Ok(Err(e)) => return Err(fail(sync, e).await),
                Ok(Ok(f)) => {
                    info!(attachment_pk, bytes = f.bytes, text_chars = f.text_chars, "вложение скачано по требованию");
                    f.path
                }
            }
        }
    };
    if let Err(e) = store.touch_attachment(attachment_pk) {
        warn!(attachment_pk, error = %e, "не удалось отметить обращение к вложению");
    }
    Ok(path)
}

// --- команды Tauri -----------------------------------------------------------

/// Тело письма и его инлайновые картинки: догружаются с сервера, если их нет локально, затем
/// возвращается свежий DTO с `data:` вместо `cid:`. Если всё уже есть — сети нет.
#[tauri::command]
pub(crate) async fn message_fetch(item_pk: i64, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>) -> CmdResult<MessageDto> {
    let lock = od.items.get(item_pk);
    let _busy = lock.lock().await;
    let store = state.store.clone();
    let item = store.item_ref(item_pk).map_err(err)?.ok_or_else(|| "Письмо удалено".to_string())?;
    let mut engine: Option<Engine> = None;

    // Тело: нет (0) или прошлый отказ сервера (2) — пользователь просит ещё раз.
    if item.body_state != 1 {
        let eng = engine_for(&state, &sync).await?;
        match tokio::time::timeout(BODY_TIMEOUT, eng.fetch_body(&item.id, item.folder_pk)).await {
            Err(_) => return Err(timed_out(BODY_TIMEOUT)),
            Ok(Err(e)) => return Err(fail(&sync, e).await),
            Ok(Ok(BodyFetchOutcome::Gone)) => return Err("Письмо удалено на сервере".into()),
            Ok(Ok(BodyFetchOutcome::Failed { code, .. })) => warn!(item_pk, code, "сервер не отдал тело письма, записан отказ"),
            Ok(Ok(BodyFetchOutcome::Stored)) => {}
        }
        engine = Some(eng);
    }

    // Инлайновые картинки без файла в кэше — по одной, с лимитом на файл.
    let pending = pending_inline(&store, item_pk)?;
    if !pending.is_empty() {
        let eng = match engine {
            Some(e) => e,
            None => engine_for(&state, &sync).await?,
        };
        let text_chars = SyncOptions::default().attachment_text_chars;
        for a in pending {
            match tokio::time::timeout(BODY_TIMEOUT, eng.fetch_one_attachment(&a.todo(), INLINE_IMAGE_MAX_BYTES, text_chars)).await {
                Err(_) => return Err(timed_out(BODY_TIMEOUT)),
                Ok(Ok(_)) => {}
                // Отказ по этому вложению (сервер, размер, не файловое вложение) запоминаем, чтобы не
                // просить его при каждом раскрытии; письмо показываем без этой картинки.
                Ok(Err(SyncError::Transport(e))) if e.is_item_failure() || matches!(e, TransportError::TooLarge { .. }) || (matches!(e, TransportError::Protocol { .. }) && !e.is_retryable()) => {
                    warn!(attachment_pk = a.attachment_pk, error = %e, "инлайновая картинка не скачана");
                    if let Err(e) = store.mark_attachment_failed(a.attachment_pk, FETCH_FAILED) {
                        warn!(attachment_pk = a.attachment_pk, error = %e, "не удалось пометить вложение");
                    }
                }
                Ok(Err(e)) => return Err(fail(&sync, e).await),
            }
        }
    }

    // DTO: чтение файлов кэша и base64 — в блокирующем потоке.
    tauri::async_runtime::spawn_blocking(move || {
        let head = store.message_head(item_pk).map_err(err)?.ok_or_else(|| "Письмо удалено на сервере".to_string())?;
        build_message(&store, head)
    })
    .await
    .map_err(err)?
}

/// Скачать вложение при необходимости и открыть системным приложением (ShellExecute, глагол
/// по умолчанию). Исполняемые типы не открываются — только «Сохранить как».
#[tauri::command]
pub(crate) async fn attachment_open(attachment_pk: i64, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>) -> CmdResult<()> {
    let row = state.store.attachment(attachment_pk).map_err(err)?.ok_or_else(|| "Вложение не найдено".to_string())?;
    if is_unsafe_to_open(row.name.as_deref()) {
        return Err("Файлы такого типа открывать небезопасно — используйте «Сохранить как…»".into());
    }
    let path = ensure_cached(&state, &sync, &od, attachment_pk).await?;
    tauri::async_runtime::spawn_blocking(move || open::that_detached(&path)).await.map_err(err)?.map_err(|e| {
        warn!(attachment_pk, error = %e, "системное приложение не открыло файл");
        "Не удалось открыть файл системным приложением".to_string()
    })
}

/// Скачать при необходимости и сохранить копию туда, куда укажет пользователь. `None` — отмена.
/// Путь берётся только из ответа диалога; копия получает метку зоны «Интернет», как файл кэша.
#[tauri::command]
pub(crate) async fn attachment_save_as(attachment_pk: i64, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>) -> CmdResult<Option<String>> {
    let row = state.store.attachment(attachment_pk).map_err(err)?.ok_or_else(|| "Вложение не найдено".to_string())?;
    let cached = ensure_cached(&state, &sync, &od, attachment_pk).await?;
    let suggested = aiowa_store::safe_file_name(row.name.as_deref().unwrap_or("вложение"));
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Сохранить вложение").set_file_name(&suggested);
    if let Some(dir) = dirs::download_dir() {
        dialog = dialog.set_directory(dir);
    }
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }
    let Some(chosen) = dialog.save_file().await else { return Ok(None) };
    let target = chosen.path().to_path_buf();
    let copied = tauri::async_runtime::spawn_blocking(move || -> std::io::Result<PathBuf> {
        std::fs::copy(&cached, &target)?;
        if let Err(e) = aiowa_store::mark_from_internet(&target) {
            warn!(error = %e, "не удалось поставить метку Zone.Identifier на сохранённый файл");
        }
        Ok(target)
    })
    .await
    .map_err(err)?;
    match copied {
        Ok(p) => Ok(Some(p.display().to_string())),
        Err(e) => {
            warn!(attachment_pk, error = %e, "не удалось сохранить вложение");
            Err("Не удалось записать файл: проверьте путь и права доступа".into())
        }
    }
}

/// Открыть ссылку из письма во внешнем приложении. Схема проверяется по разобранному URL
/// (не по префиксу строки): `file:`, `javascript:`, UNC и голые пути отклоняются.
#[tauri::command]
pub(crate) async fn open_external(url: String) -> CmdResult<()> {
    let parsed = url::Url::parse(url.trim()).map_err(|_| "Ссылка не распознана".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https" | "mailto") {
        return Err("Можно открывать только ссылки http, https и mailto".into());
    }
    let target = parsed.to_string();
    tauri::async_runtime::spawn_blocking(move || open::that_detached(&target)).await.map_err(err)?.map_err(|e| {
        warn!(error = %e, "системное приложение не открыло ссылку");
        "Не удалось открыть ссылку в системном приложении".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_extensions_are_blocked_case_insensitively() {
        assert!(is_unsafe_to_open(Some("Счёт.pdf.exe")));
        assert!(is_unsafe_to_open(Some("script.JS")));
        assert!(is_unsafe_to_open(Some("ярлык.lnk")));
        assert!(is_unsafe_to_open(Some("setup.MSI ")), "хвостовые пробелы и точки убирает safe_file_name");
        assert!(is_unsafe_to_open(Some("run.bat.")));
        assert!(!is_unsafe_to_open(Some("отчёт.pdf")));
        assert!(!is_unsafe_to_open(Some("фото.JPG")));
        assert!(!is_unsafe_to_open(Some("без расширения")));
        assert!(!is_unsafe_to_open(None));
    }

    #[test]
    fn transport_errors_have_fixed_user_texts() {
        let raw = "<html>секретный ответ сервера</html>";
        let p = describe_transport(&TransportError::Protocol { op: "GetItem".into(), status: 500, raw: raw.into() });
        assert!(!p.contains("секретный") && p.contains("500"), "{p}");
        let a = describe_transport(&TransportError::AuthFailed("reason=2 и прочие детали".into()));
        assert!(!a.contains("reason") && a.contains("пароль"), "{a}");
        assert_eq!(describe_transport(&TransportError::TooLarge { limit: 200 * 1024 * 1024 }), "Файл больше лимита 200 МБ");
        assert_eq!(describe_transport(&TransportError::SessionExpired), "Сессия истекла, повторите попытку");
        assert_eq!(describe_transport(&TransportError::Network(Box::new(std::io::Error::other("x")))), "Нет связи с сервером");
    }

    #[test]
    fn external_links_allow_only_web_and_mail_schemes() {
        let ok = |u: &str| url::Url::parse(u).is_ok_and(|p| matches!(p.scheme(), "http" | "https" | "mailto"));
        assert!(ok("https://example.org/path?q=1"));
        assert!(ok("HTTP://example.org"));
        assert!(ok("mailto:someone@example.org"));
        assert!(!ok("file:///C:/Windows/System32/calc.exe"));
        assert!(!ok("javascript:alert(1)"));
        assert!(!ok(r"\\server\share\run.exe"));
        assert!(!ok("C:\\Windows\\notepad.exe"));
        assert!(!ok("example.org"));
    }
}
