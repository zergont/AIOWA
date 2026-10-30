//! Команды окна для папок: создать, переименовать, удалить. Сначала сервер (`CreateFolder`,
//! `UpdateFolder`, `DeleteFolder` через транспорт процесса, без новых входов, если сессия уже
//! есть), затем локальная правка таблицы `folders` зеркалом ответа, событие `folders-changed`
//! окну и запрос синхронизации дерева (`SyncState::request_hierarchy`): она сверит имена,
//! родителей и счётчики с сервером и по концу снова пошлёт `folders-changed`.
//!
//! Ограничения: служебные папки (`distinguished`) не переименовываются и не удаляются; имя
//! 1–255 символов без управляющих символов и `\ / : * ? " < > |`; у соседей по родителю имена не
//! повторяются (без учёта регистра). Удаление не навсегда переносит папку с подпапками и письмами
//! в «Удалённые»; папку, уже лежащую в «Удалённых», можно удалить только навсегда.

use crate::ondemand::{describe_transport, timed_out};
use crate::sync::SyncState;
use crate::write::{deleted_items_pk, emit_items_changed};
use crate::{err, folder_dtos, AppState, CmdResult, FolderDto};
use aiowa_core::{DistinguishedFolder, Folder, FolderId, FolderRef};
use aiowa_transport::{MailTransport, TransportError};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tracing::{info, warn};

/// Как очищается папка (docs/obkatka.md пп. 15–16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmptyMode {
    /// Письма — в «Удалённые» (`MoveToDeletedItems`): обычная своя папка.
    ToDeleted,
    /// Навсегда (`SoftDelete`, в OWA их ещё можно восстановить): «Удалённые», «Нежелательная» и
    /// папки внутри «Удалённых».
    Permanent,
}

/// Можно ли очистить папку и как: служебные папки — нет, кроме «Удалённых» и «Нежелательной»
/// (решение автора 25.10.2026: «корневые = служебные»).
pub(crate) fn empty_mode(list: &[(i64, Folder)], folder_pk: i64, deleted_pk: Option<i64>) -> CmdResult<EmptyMode> {
    let f = by_pk(list, folder_pk).ok_or("Папка не найдена")?;
    match f.distinguished.as_deref() {
        Some("deleteditems" | "junkemail") => Ok(EmptyMode::Permanent),
        Some(_) => Err("Служебную папку не очистить: её письма удаляются выбором".into()),
        None if deleted_pk.is_some_and(|d| is_inside(list, folder_pk, d)) => Ok(EmptyMode::Permanent),
        None => Ok(EmptyMode::ToDeleted),
    }
}

/// Итог очистки для окна: сколько писем убрано локально и навсегда ли.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct FolderEmptyDto {
    items: usize,
    permanent: bool,
}

/// Таймаут одной операции с папкой на сервере.
const FOLDER_TIMEOUT: Duration = Duration::from_secs(60);
/// Предел длины имени папки в символах (как у Exchange).
pub(crate) const MAX_NAME_CHARS: usize = 255;
/// Символы, запрещённые в имени папки.
const FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
/// Имя события окну: дерево папок изменилось.
const FOLDERS_CHANGED: &str = "folders-changed";

// --- чистые функции (без базы и сети) --------------------------------------------------------

/// Проверка имени папки. Возвращает имя без пробелов по краям; ошибка — текст для пользователя.
pub(crate) fn validate_folder_name(name: &str) -> CmdResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Введите имя папки".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("Имя папки длиннее {MAX_NAME_CHARS} символов"));
    }
    if name.chars().any(char::is_control) {
        return Err("Имя папки содержит управляющие символы".into());
    }
    if let Some(c) = name.chars().find(|c| FORBIDDEN.contains(c)) {
        return Err(format!("Имя папки не может содержать символ «{c}» (запрещены \\ / : * ? \" < > |)"));
    }
    Ok(name.to_string())
}

/// Строка папки по ключу.
fn by_pk(list: &[(i64, Folder)], folder_pk: i64) -> Option<&Folder> {
    list.iter().find(|(pk, _)| *pk == folder_pk).map(|(_, f)| f)
}

/// Ключ родителя папки среди известных строк (`None` — верхний уровень: родитель — корень ящика,
/// которого в базе нет).
fn parent_pk(list: &[(i64, Folder)], f: &Folder) -> Option<i64> {
    let parent = f.parent_id.as_ref()?;
    list.iter().find(|(_, p)| p.id.id == parent.id).map(|(pk, _)| *pk)
}

/// Есть ли у родителя (по идентификатору сервера; `None` — папки без родителя) другая папка с тем
/// же именем без учёта регистра. `except` — сама переименовываемая папка.
pub(crate) fn name_taken(list: &[(i64, Folder)], parent_server_id: Option<&str>, name: &str, except: Option<i64>) -> bool {
    let wanted = name.trim().to_lowercase();
    list.iter().any(|(pk, f)| Some(*pk) != except && f.parent_id.as_ref().map(|p| p.id.as_str()) == parent_server_id && f.display_name.trim().to_lowercase() == wanted)
}

/// Лежит ли папка `folder_pk` внутри `ancestor_pk` (на любой глубине). Сама папка — нет.
pub(crate) fn is_inside(list: &[(i64, Folder)], folder_pk: i64, ancestor_pk: i64) -> bool {
    let mut cur = by_pk(list, folder_pk).and_then(|f| parent_pk(list, f));
    let mut hops = 0;
    while let Some(pk) = cur {
        if pk == ancestor_pk {
            return true;
        }
        hops += 1;
        if hops > 64 {
            break;
        }
        cur = by_pk(list, pk).and_then(|f| parent_pk(list, f));
    }
    false
}

/// Папка и все её подпапки (ключи), сама папка первой.
pub(crate) fn subtree_pks(list: &[(i64, Folder)], folder_pk: i64) -> Vec<i64> {
    let children: HashMap<i64, Vec<i64>> = list.iter().fold(HashMap::new(), |mut m, (pk, f)| {
        if let Some(parent) = parent_pk(list, f) {
            m.entry(parent).or_default().push(*pk);
        }
        m
    });
    let mut out = vec![folder_pk];
    let mut seen: HashSet<i64> = HashSet::from([folder_pk]);
    let mut i = 0;
    while i < out.len() {
        if let Some(kids) = children.get(&out[i]) {
            for &k in kids {
                if seen.insert(k) {
                    out.push(k);
                }
            }
        }
        i += 1;
    }
    out
}

/// Текст отказа сервера по папке: частые коды словами, остальные как у `describe_transport`.
pub(crate) fn describe_folder_error(e: &TransportError) -> String {
    let TransportError::Server { code, .. } = e else { return describe_transport(e) };
    match code.as_str() {
        "ErrorFolderExists" => "Папка с таким именем уже есть".into(),
        "ErrorFolderNotFound" | "ErrorItemNotFound" => "Папки уже нет на сервере: обновите папки".into(),
        "ErrorDeleteDistinguishedFolder" => "Служебную папку нельзя удалить".into(),
        "ErrorCannotDeleteObject" => "Сервер не разрешил удалить папку".into(),
        "ErrorMoveCopyFailed" => "Сервер не смог перенести папку".into(),
        "ErrorAccessDenied" => "Сервер отказал в доступе к папке".into(),
        "ErrorIrresolvableConflict" | "ErrorChangeKeyRequired" | "ErrorInvalidChangeKey" => "Папка изменена на сервере, обновите папки и повторите".into(),
        _ => describe_transport(e),
    }
}

// --- сервер и база -----------------------------------------------------------------------------

/// Один сетевой шаг под таймаутом: ошибка транспорта сбрасывает мёртвую сессию и превращается в
/// текст для пользователя.
async fn server_step<T>(sync: &SyncState, what: &'static str, fut: impl Future<Output = aiowa_transport::Result<T>>) -> CmdResult<T> {
    match tokio::time::timeout(FOLDER_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            warn!(step = what, error = %e, "операция с папкой отклонена");
            sync.invalidate_on(&e).await;
            Err(describe_folder_error(&e))
        }
        Err(_) => {
            warn!(step = what, "операция с папкой не уложилась в таймаут");
            Err(timed_out(FOLDER_TIMEOUT))
        }
    }
}

/// После локальной правки: событие окну и синхронизация дерева (по её концу — ещё одно событие).
fn after_change(app: &AppHandle, sync: &Arc<SyncState>) {
    if let Err(e) = app.emit(FOLDERS_CHANGED, ()) {
        warn!(error = %e, "не удалось отправить событие folders-changed");
    }
    sync.request_hierarchy(app.clone());
}

/// DTO папки по ключу из текущего состояния базы.
fn folder_dto(state: &AppState, folder_pk: i64) -> CmdResult<FolderDto> {
    let list = state.store.list_folders(state.account).map_err(err)?;
    folder_dtos(&list).into_iter().find(|f| f.folder_pk == folder_pk).ok_or_else(|| "Папка не найдена в локальной базе".to_string())
}

// --- команды Tauri -----------------------------------------------------------------------------

/// Создать почтовую папку `name` внутри `parent_pk` (ключ локальной базы); `None` — в корне
/// ящика (`msgfolderroot`). Возвращает созданную папку.
#[tauri::command]
pub(crate) async fn folder_create(parent_pk: Option<i64>, name: String, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>) -> CmdResult<FolderDto> {
    let name = validate_folder_name(&name)?;
    let list = state.store.list_folders(state.account).map_err(err)?;
    let (parent_ref, parent_server_id) = match parent_pk {
        Some(pk) => {
            let f = by_pk(&list, pk).ok_or_else(|| "Родительская папка не найдена".to_string())?;
            if !f.is_mail() {
                return Err("Почтовую папку можно создать только внутри почтовой папки".into());
            }
            (FolderRef::Id(FolderId { id: f.id.id.clone(), change_key: None }), Some(f.id.id.clone()))
        }
        None => (FolderRef::Distinguished(DistinguishedFolder::MsgFolderRoot), state.store.msgfolderroot_server_id(state.account).map_err(err)?),
    };
    if name_taken(&list, parent_server_id.as_deref(), &name, None) {
        return Err("Папка с таким именем уже есть".into());
    }
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let id = server_step(&sync, "CreateFolder", transport.create_folder(&parent_ref, &name)).await?;
    let folder = Folder {
        id,
        parent_id: parent_server_id.map(|p| FolderId { id: p, change_key: None }),
        display_name: name.clone(),
        distinguished: None,
        folder_class: Some(aiowa_transport_owa::protocol::MAIL_FOLDER_CLASS.to_string()),
        total_count: 0,
        unread_count: 0,
        child_folder_count: 0,
    };
    let folder_pk = state.store.insert_folder(state.account, &folder).map_err(err)?;
    info!(folder_pk, parent_pk, name_len = name.chars().count(), "папка создана");
    after_change(&app, &sync);
    folder_dto(&state, folder_pk)
}

/// Переименовать папку. Служебные папки не переименовываются; то же имя — без запроса к серверу.
#[tauri::command]
pub(crate) async fn folder_rename(folder_pk: i64, name: String, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>) -> CmdResult<FolderDto> {
    let name = validate_folder_name(&name)?;
    let list = state.store.list_folders(state.account).map_err(err)?;
    let f = by_pk(&list, folder_pk).ok_or_else(|| "Папка не найдена".to_string())?;
    if f.distinguished.is_some() {
        return Err("Служебную папку нельзя переименовать".into());
    }
    if f.display_name == name {
        return folder_dto(&state, folder_pk);
    }
    if name_taken(&list, f.parent_id.as_ref().map(|p| p.id.as_str()), &name, Some(folder_pk)) {
        return Err("Папка с таким именем уже есть".into());
    }
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let new_id = server_step(&sync, "UpdateFolder", transport.rename_folder(&f.id, &name)).await?;
    state.store.rename_folder_local(folder_pk, &name, Some(&new_id)).map_err(err)?;
    info!(folder_pk, name_len = name.chars().count(), same_id = new_id.id == f.id.id, "папка переименована");
    after_change(&app, &sync);
    folder_dto(&state, folder_pk)
}

/// Удалить папку с подпапками и письмами: `permanent = false` — в «Удалённые» (папка становится
/// подпапкой «Удалённых»; для папки, уже лежащей там, отказ), `permanent = true` — навсегда.
#[tauri::command]
pub(crate) async fn folder_delete(folder_pk: i64, permanent: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>) -> CmdResult<()> {
    let list = state.store.list_folders(state.account).map_err(err)?;
    let f = by_pk(&list, folder_pk).ok_or_else(|| "Папка не найдена".to_string())?;
    if f.distinguished.is_some() {
        return Err("Служебную папку нельзя удалить".into());
    }
    let to_deleted = if permanent {
        None
    } else {
        let deleted_pk = deleted_items_pk(&state)?;
        if is_inside(&list, folder_pk, deleted_pk) {
            return Err("Папка уже в «Удалённых»: удалите её навсегда".into());
        }
        Some(deleted_pk)
    };
    let affected = subtree_pks(&list, folder_pk);
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    server_step(&sync, "DeleteFolder", transport.delete_folder(&f.id, permanent)).await?;
    let stats = state.store.delete_folder_local(folder_pk, to_deleted).map_err(err)?;
    info!(folder_pk, permanent, folders = stats.folders, items = stats.items, "папка удалена");
    after_change(&app, &sync);
    // Письма удалённого навсегда поддерева помечены удалёнными: открытая беседа и список могут
    // их содержать, окно перечитает по событию. Беседы названы явно: открытую окно перечитывает
    // только по её ключу в событии (беседа может быть собрана из нескольких папок).
    if stats.items > 0 {
        emit_items_changed(&app, stats.conversation_pks, affected);
    }
    Ok(())
}

/// Очистить папку (`EmptyFolder`): письма — в «Удалённые» или навсегда (`empty_mode`), при
/// `subfolders` — вместе с подпапками (из обычной папки они переезжают в «Удалённые», из
/// «Удалённых» и «Нежелательной» — удаляются). Затем локальная правка, события окну и
/// синхронизация дерева и «Удалённых».
#[tauri::command]
pub(crate) async fn folder_empty(folder_pk: i64, subfolders: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>) -> CmdResult<FolderEmptyDto> {
    let list = state.store.list_folders(state.account).map_err(err)?;
    let deleted_pk = deleted_items_pk(&state).ok();
    let mode = empty_mode(&list, folder_pk, deleted_pk)?;
    let f = by_pk(&list, folder_pk).ok_or("Папка не найдена")?;
    let local = match (subfolders, mode) {
        (false, _) => aiowa_store::EmptySubfolders::Keep,
        (true, EmptyMode::Permanent) => aiowa_store::EmptySubfolders::Delete,
        (true, EmptyMode::ToDeleted) => aiowa_store::EmptySubfolders::ToTrash(deleted_pk.ok_or("Папки «Удалённые» нет: дождитесь синхронизации")?),
    };
    let delete_type = if mode == EmptyMode::Permanent { "SoftDelete" } else { "MoveToDeletedItems" };
    let affected = subtree_pks(&list, folder_pk);
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    server_step(&sync, "EmptyFolder", transport.empty_folder(&f.id, delete_type, subfolders)).await?;
    let stats = state.store.empty_folder_local(folder_pk, local).map_err(err)?;
    info!(folder_pk, permanent = mode == EmptyMode::Permanent, subfolders, folders = stats.folders, items = stats.items, "папка очищена");
    after_change(&app, &sync);
    if stats.items > 0 {
        emit_items_changed(&app, stats.conversation_pks, affected);
    }
    // Перенесённые письма появятся в «Удалённых» синхронизацией этой папки.
    if let (EmptyMode::ToDeleted, Some(d)) = (mode, deleted_pk) {
        sync.request(app.clone(), Some(vec![d]));
    }
    Ok(FolderEmptyDto { items: stats.items, permanent: mode == EmptyMode::Permanent })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_mode_by_folder_kind() {
        let list = tree();
        let deleted = list.iter().find(|(_, f)| f.distinguished.as_deref() == Some("deleteditems")).map(|(pk, _)| *pk);
        let pk = |name: &str| list.iter().find(|(_, f)| f.display_name == name).map(|(pk, _)| *pk).unwrap();
        assert!(empty_mode(&list, pk("Входящие"), deleted).is_err(), "служебную не очистить");
        assert_eq!(empty_mode(&list, deleted.unwrap(), deleted).unwrap(), EmptyMode::Permanent);
        assert_eq!(empty_mode(&list, pk("Проекты"), deleted).unwrap(), EmptyMode::ToDeleted);
        assert_eq!(empty_mode(&list, pk("Старое"), deleted).unwrap(), EmptyMode::Permanent, "папка внутри «Удалённых» — навсегда");
        assert!(empty_mode(&list, 9999, deleted).is_err());
    }

    fn folder(name: &str, id: &str, parent: Option<&str>, dist: Option<&str>) -> Folder {
        Folder {
            id: FolderId { id: id.to_string(), change_key: Some("ck".into()) },
            parent_id: parent.map(|p| FolderId { id: p.to_string(), change_key: None }),
            display_name: name.to_string(),
            distinguished: dist.map(String::from),
            folder_class: Some("IPF.Note".into()),
            total_count: 0,
            unread_count: 0,
            child_folder_count: 0,
        }
    }

    /// Корень `ROOT` в базе не хранится; «Входящие», «Удалённые» и «Проекты» — верхнего уровня.
    fn tree() -> Vec<(i64, Folder)> {
        vec![
            (1, folder("Входящие", "INBOX", Some("ROOT"), Some("inbox"))),
            (2, folder("Удаленные", "DEL", Some("ROOT"), Some("deleteditems"))),
            (3, folder("Проекты", "PRJ", Some("ROOT"), None)),
            (4, folder("Альфа", "PRJ-A", Some("PRJ"), None)),
            (5, folder("Бета", "PRJ-B", Some("PRJ"), None)),
            (6, folder("Старое", "DEL-OLD", Some("DEL"), None)),
            (7, folder("Глубже", "PRJ-A-1", Some("PRJ-A"), None)),
        ]
    }

    #[test]
    fn folder_name_rules() {
        assert_eq!(validate_folder_name("  Проекты 2026 ").unwrap(), "Проекты 2026");
        assert_eq!(validate_folder_name("").unwrap_err(), "Введите имя папки");
        assert_eq!(validate_folder_name("   ").unwrap_err(), "Введите имя папки");
        assert!(validate_folder_name(&"я".repeat(255)).is_ok(), "ровно 255 символов проходит (считаются символы, не байты)");
        assert_eq!(validate_folder_name(&"я".repeat(256)).unwrap_err(), "Имя папки длиннее 255 символов");
        assert_eq!(validate_folder_name("а\tб").unwrap_err(), "Имя папки содержит управляющие символы");
        assert!(validate_folder_name("а\u{7f}б").is_err());
        for c in ['\\', '/', ':', '*', '?', '"', '<', '>', '|'] {
            let e = validate_folder_name(&format!("Папка{c}")).unwrap_err();
            assert!(e.contains(&format!("«{c}»")), "{c}: {e}");
        }
        assert!(validate_folder_name("Счета (2026) — важное & срочное").is_ok(), "скобки, тире и амперсанд разрешены");
    }

    #[test]
    fn sibling_names_are_unique_without_case() {
        let t = tree();
        assert!(name_taken(&t, Some("ROOT"), "проекты", None), "тот же родитель, другой регистр");
        assert!(name_taken(&t, Some("ROOT"), " Проекты ", None), "пробелы по краям не считаются");
        assert!(!name_taken(&t, Some("PRJ"), "Проекты", None), "в другой папке имя свободно");
        assert!(name_taken(&t, Some("PRJ"), "Альфа", None));
        assert!(!name_taken(&t, Some("PRJ"), "Альфа", Some(4)), "переименование в своё же имя не конфликт");
        assert!(name_taken(&t, Some("PRJ"), "Альфа", Some(5)), "а в имя соседа — конфликт");
        assert!(!name_taken(&t, None, "Проекты", None), "корень неизвестен — сравниваем только с папками без родителя");
    }

    #[test]
    fn subtree_and_ancestry() {
        let t = tree();
        assert_eq!(subtree_pks(&t, 3), vec![3, 4, 5, 7]);
        assert_eq!(subtree_pks(&t, 4), vec![4, 7]);
        assert_eq!(subtree_pks(&t, 5), vec![5]);
        assert_eq!(subtree_pks(&t, 99), vec![99], "неизвестная папка — только она сама");
        assert!(is_inside(&t, 7, 3), "на любой глубине");
        assert!(is_inside(&t, 4, 3));
        assert!(!is_inside(&t, 3, 3), "сама папка не внутри себя");
        assert!(!is_inside(&t, 3, 4));
        assert!(is_inside(&t, 6, 2), "подпапка «Удалённых» внутри них");
        assert!(!is_inside(&t, 3, 2));
        assert!(!is_inside(&t, 99, 2));
        // Цикл в данных не вешает проверку.
        let mut cyc = vec![(1, folder("A", "A", Some("B"), None)), (2, folder("B", "B", Some("A"), None))];
        assert!(!is_inside(&cyc, 1, 3));
        cyc.push((3, folder("C", "C", None, None)));
        assert_eq!(subtree_pks(&cyc, 1), vec![1, 2]);
    }

    #[test]
    fn folder_error_texts_are_russian() {
        let server = |code: &str| TransportError::Server { code: code.into(), message: "<details>".into() };
        assert_eq!(describe_folder_error(&server("ErrorFolderExists")), "Папка с таким именем уже есть");
        assert_eq!(describe_folder_error(&server("ErrorFolderNotFound")), "Папки уже нет на сервере: обновите папки");
        assert_eq!(describe_folder_error(&server("ErrorDeleteDistinguishedFolder")), "Служебную папку нельзя удалить");
        assert_eq!(describe_folder_error(&server("ErrorIrresolvableConflict")), "Папка изменена на сервере, обновите папки и повторите");
        let other = describe_folder_error(&server("ErrorInternalServerError"));
        assert_eq!(other, "Сервер отказал: ErrorInternalServerError");
        assert!(!other.contains("details"));
        assert_eq!(describe_folder_error(&TransportError::Network("x".into())), "Нет связи с сервером");
    }
}
