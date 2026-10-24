//! Действия с окном отмены (6 с): удаление в «Удалённые», перенос, архив. Локальное состояние
//! меняется сразу (теми же `Action`/`apply`, что в `write.rs`), а сервер ждёт: откат по кнопке
//! «Отменить» или Ctrl+Z до срока обходится без сервера (`items_undo`), по истечении срока
//! таймер сам отправляет операцию пачками (`write::send_to_server`, без повторного локального
//! применения) и шлёт `items-changed`. Отказ при такой отправке окно уже не ждёт (строка отмены
//! закрыта), поэтому он уходит событием `write-failed { message, item_pks }`, а локальное
//! изменение по отказавшим письмам откатывается, как в обычной команде записи. `items_commit`
//! отправляет сейчас (новое действие с окном отмены коммитит предыдущее); при выходе все
//! отложенные действия коммитятся до logoff (`commit_all_blocking`). Замок записи общий с
//! `write.rs`: применение, откат и отправка не переплетаются.
//!
//! Чистая машина состояний — `StageBook`: регистрация со сроком, откат до срока, коммит,
//! выгрузка всех при выходе; таймеры, база и сервер — поверх неё.

use crate::sync::SyncState;
use crate::write::{apply_local, deleted_items_pk, move_action, prepare, rollback, send_to_server, Action, Changed, WriteFailure, WriteReport, WriteState};
use crate::{err, AppState, CmdResult};
use aiowa_core::Folder;
use aiowa_store::ItemRef;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tracing::{info, warn};

/// Окно отмены: столько локальное изменение ждёт перед отправкой на сервер.
pub(crate) const UNDO_WINDOW: Duration = Duration::from_secs(6);
/// Сколько ждать отправку отложенных действий при выходе.
const EXIT_COMMIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Имя события окну: сервер отказал по действию, окно отмены которого уже закрыто.
const WRITE_FAILED: &str = "write-failed";
/// Текст отказа отмены: действие уже отправлено, отменено или срок прошёл.
pub(crate) const UNDO_EXPIRED: &str = "Отмена уже невозможна";
/// Текст, когда папки архива нет.
pub(crate) const NO_ARCHIVE: &str = "Папки «Архив» нет в ящике: создайте её и повторите";

/// Действие с окном отмены, как его называет окно.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StageAction {
    /// В «Удалённые» (`permanent` — навсегда, без отката на сервере, но с тем же окном).
    Delete,
    /// В папку `folder_pk`.
    Move,
    /// В папку архива: `folder_pk`, если окно её знает, иначе служебная `archive` или «Архив».
    Archive,
}

/// Итог `items_stage`: локально применено, сервер ждёт до `undo_until_ms` (мс эпохи).
#[derive(Debug, Serialize)]
pub(crate) struct StagedDto {
    /// Пусто, если применять было нечего (все письма в `failed`).
    stage_id: String,
    /// Писем, изменённых локально и ожидающих отправки.
    done: i64,
    failed: Vec<WriteFailure>,
    undo_until_ms: i64,
}

/// Событие `write-failed`.
#[derive(Debug, Clone, Serialize)]
struct WriteFailed {
    message: String,
    item_pks: Vec<i64>,
}

// --- чистая машина состояний -------------------------------------------------------------------

/// Запись книги: значение, порядок регистрации и срок, до которого можно отменить (мс эпохи).
#[derive(Debug)]
struct Entry<T> {
    value: T,
    seq: u64,
    undo_until_ms: i64,
}

/// Книга отложенных действий без таймеров и базы: по идентификатору действие можно откатить до
/// срока или забрать на отправку; при выходе забираются все в порядке регистрации.
#[derive(Debug)]
pub(crate) struct StageBook<T> {
    seq: u64,
    entries: HashMap<String, Entry<T>>,
}

impl<T> Default for StageBook<T> {
    fn default() -> Self {
        Self { seq: 0, entries: HashMap::new() }
    }
}

impl<T> StageBook<T> {
    /// Регистрирует действие. Возвращает идентификатор и срок отмены `now_ms + window`.
    pub(crate) fn stage(&mut self, value: T, now_ms: i64, window: Duration) -> (String, i64) {
        self.seq += 1;
        let id = format!("{now_ms}-{}", self.seq);
        let undo_until_ms = now_ms + window.as_millis() as i64;
        self.entries.insert(id.clone(), Entry { value, seq: self.seq, undo_until_ms });
        (id, undo_until_ms)
    }

    /// Забирает действие для отката. Его нет (уже отправлено, откачено или не было) или срок
    /// прошёл (запись остаётся таймеру) — ошибка `UNDO_EXPIRED`.
    pub(crate) fn undo(&mut self, id: &str, now_ms: i64) -> Result<T, String> {
        let e = self.entries.remove(id).ok_or_else(|| UNDO_EXPIRED.to_string())?;
        if now_ms > e.undo_until_ms {
            self.entries.insert(id.to_string(), e);
            return Err(UNDO_EXPIRED.into());
        }
        Ok(e.value)
    }

    /// Забирает действие для отправки на сервер; `None` — его уже нет.
    pub(crate) fn commit(&mut self, id: &str) -> Option<T> {
        self.entries.remove(id).map(|e| e.value)
    }

    /// Действие по идентификатору (на месте), если оно ещё в книге.
    pub(crate) fn get_mut(&mut self, id: &str) -> Option<&mut T> {
        self.entries.get_mut(id).map(|e| &mut e.value)
    }

    /// Забирает все действия в порядке регистрации (при выходе).
    pub(crate) fn drain(&mut self) -> Vec<T> {
        let mut all: Vec<Entry<T>> = self.entries.drain().map(|(_, e)| e).collect();
        all.sort_by_key(|e| e.seq);
        all.into_iter().map(|e| e.value).collect()
    }
}

/// Папка архива: служебная `archive` (почтовая), иначе почтовая папка верхнего уровня с именем
/// «Архив» или «Archive» (без учёта регистра). Верхний уровень — родитель не среди известных папок
/// (корень ящика в базе не хранится) или отсутствует.
pub(crate) fn find_archive_folder(list: &[(i64, Folder)]) -> Option<i64> {
    if let Some((pk, _)) = list.iter().find(|(_, f)| f.distinguished.as_deref() == Some("archive") && f.is_mail()) {
        return Some(*pk);
    }
    let known: HashSet<&str> = list.iter().map(|(_, f)| f.id.id.as_str()).collect();
    let top_level = |f: &Folder| f.parent_id.as_ref().is_none_or(|p| !known.contains(p.id.as_str()));
    list.iter().find(|(_, f)| f.is_mail() && is_archive_name(&f.display_name) && top_level(f)).map(|(pk, _)| *pk)
}

fn is_archive_name(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n == "архив" || n == "archive"
}

/// Текст события `write-failed` по отказам: один — как есть, одинаковые — с числом писем,
/// разные — число и первый текст. `None` без отказов.
pub(crate) fn failure_summary(failed: &[WriteFailure]) -> Option<String> {
    let first = failed.first()?;
    if failed.len() == 1 {
        return Some(first.message.clone());
    }
    if failed.iter().all(|f| f.message == first.message) {
        return Some(format!("{} (писем: {})", first.message, failed.len()));
    }
    Some(format!("Сервер отказал по {} письмам: {}", failed.len(), first.message))
}

// --- состояние процесса -------------------------------------------------------------------------

/// Отложенное действие: операция, письма в состоянии до применения (для отката и отправки) и
/// таймер отправки.
struct Pending {
    action: Action,
    todo: Vec<ItemRef>,
    timer: Option<tauri::async_runtime::JoinHandle<()>>,
}

/// Книга отложенных действий процесса.
#[derive(Default)]
pub(crate) struct StagedState {
    book: Mutex<StageBook<Pending>>,
}

impl StagedState {
    fn lock(&self) -> MutexGuard<'_, StageBook<Pending>> {
        // Отравленный мьютекс не страшен: внутри только записи книги.
        self.book.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Операция записи по действию окна. Архив — перенос в папку архива.
fn resolve_action(state: &AppState, kind: StageAction, folder_pk: Option<i64>, permanent: bool) -> CmdResult<Action> {
    match kind {
        StageAction::Delete => Ok(if permanent { Action::Hard } else { Action::ToDeleted { deleted_pk: deleted_items_pk(state)? } }),
        StageAction::Move => move_action(state, folder_pk.ok_or_else(|| "Не указана папка назначения".to_string())?),
        StageAction::Archive => {
            let pk = match folder_pk {
                Some(pk) => pk,
                None => {
                    let list = state.store.list_folders(state.account).map_err(err)?;
                    find_archive_folder(&list).ok_or_else(|| NO_ARCHIVE.to_string())?
                }
            };
            move_action(state, pk)
        }
    }
}

fn emit_write_failed(app: &AppHandle, message: String, item_pks: Vec<i64>) {
    warn!(items = item_pks.len(), %message, "отложенное действие отклонено сервером");
    if let Err(e) = app.emit(WRITE_FAILED, WriteFailed { message, item_pks }) {
        warn!(error = %e, "не удалось отправить событие write-failed");
    }
}

/// Отправляет отложенное действие на сервер (локально уже применено). Отказ целого вызова до
/// первого ответа (локально всё откачено) превращается в отказ по каждому письму. Событие
/// `items-changed` уходит в любом случае. Таймер здесь не трогается: вызывающий либо уже снял
/// его, либо сам им является. Действие берётся по ссылке: при выходе отправка ограничена сроком,
/// и неотправленное должно остаться у вызывающего для отката.
async fn commit_pending(app: &AppHandle, state: &AppState, sync: &SyncState, p: &Pending) -> WriteReport {
    let mut changed = Changed::default();
    changed.operation(&p.action, &p.todo);
    let mut report = WriteReport::default();
    if let Err(message) = send_to_server(sync, &state.store, &p.action, &p.todo, &mut changed, &mut report).await {
        report.failed.extend(p.todo.iter().map(|r| WriteFailure { item_pk: r.item_pk, message: message.clone() }));
    }
    changed.emit(app);
    report
}

/// Срок вышел: таймер отправляет действие на сервер. Если книга его уже не содержит (откат или
/// коммит успели раньше), делать нечего. Отказы — событием `write-failed`.
async fn commit_due(app: &AppHandle, stage_id: &str) {
    let state = app.state::<AppState>();
    let sync = app.state::<Arc<SyncState>>();
    let ws = app.state::<WriteState>();
    let staged = app.state::<StagedState>();
    let _busy = ws.lock().await;
    let Some(p) = staged.lock().commit(stage_id) else { return };
    info!(action = p.action.name(), items = p.todo.len(), "окно отмены закрыто, отправка на сервер");
    let report = commit_pending(app, &state, &sync, &p).await;
    if let Some(message) = failure_summary(&report.failed) {
        emit_write_failed(app, message, report.failed.iter().map(|f| f.item_pk).collect());
    }
}

/// При выходе: все отложенные действия на сервер сейчас, до logoff. Вызывается с главного потока,
/// поэтому блокирует; не дольше `EXIT_COMMIT_TIMEOUT` на всё. Что не успело уйти за срок,
/// откатывается локально: инкрементальная `SyncFolderItems` не присылает неизменённые письма, так
/// что применённое локально, но не отправленное действие расходилось бы с сервером надолго. Если
/// зависший вызов сервер всё же выполнил, следующая синхронизация сведёт базу (источник пришлёт
/// `Delete`, приёмник — `Create`).
pub(crate) fn commit_all_blocking(app: &AppHandle) {
    let staged = app.state::<StagedState>();
    let mut pending = staged.lock().drain();
    if pending.is_empty() {
        return;
    }
    for p in &mut pending {
        if let Some(t) = p.timer.take() {
            t.abort();
        }
    }
    let count = pending.len();
    let state = app.state::<AppState>();
    let sync = app.state::<Arc<SyncState>>();
    let ws = app.state::<WriteState>();
    tauri::async_runtime::block_on(async {
        let deadline = tokio::time::Instant::now() + EXIT_COMMIT_TIMEOUT;
        let mut queue = pending.into_iter();
        let mut unsent: Vec<Pending> = Vec::new();
        // Замок ждём тоже не дольше срока: его может держать другая отправка.
        let busy = tokio::time::timeout_at(deadline, ws.lock()).await;
        if busy.is_ok() {
            for p in queue.by_ref() {
                match tokio::time::timeout_at(deadline, commit_pending(app, &state, &sync, &p)).await {
                    Ok(report) if !report.failed.is_empty() => warn!(failed = report.failed.len(), "при выходе сервер отказал по отложенным письмам"),
                    Ok(_) => {}
                    Err(_) => {
                        unsent.push(p);
                        break;
                    }
                }
            }
        }
        unsent.extend(queue);
        if unsent.is_empty() {
            info!(count, "отложенные действия отправлены при выходе");
            return;
        }
        for p in &unsent {
            let mut changed = Changed::default();
            rollback(&p.action, &state.store, &p.todo, &mut changed);
        }
        warn!(count, rolled_back = unsent.len(), "отложенные действия не отправлены за {} с: неотправленные откачены локально", EXIT_COMMIT_TIMEOUT.as_secs());
    });
}

// --- команды Tauri -----------------------------------------------------------------------------

/// Применить действие локально и отложить отправку на сервер на `UNDO_WINDOW`. `folder_pk` —
/// папка назначения для `move` и `archive`; `permanent` — только для `delete`.
#[tauri::command]
pub(crate) async fn items_stage(action: StageAction, item_pks: Vec<i64>, folder_pk: Option<i64>, permanent: bool, app: AppHandle, state: State<'_, AppState>) -> CmdResult<StagedDto> {
    let ws = app.state::<WriteState>();
    let staged = app.state::<StagedState>();
    let op = resolve_action(&state, action, folder_pk, permanent)?;
    let _busy = ws.lock().await;
    let (todo, report) = prepare(&state.store, &item_pks, &op)?;
    let now = now_ms();
    if todo.is_empty() {
        return Ok(StagedDto { stage_id: String::new(), done: 0, failed: report.failed, undo_until_ms: now });
    }
    let mut changed = Changed::default();
    if let Err(e) = apply_local(&state.store, &op, &todo, &mut changed) {
        changed.emit(&app);
        return Err(e);
    }
    let done = todo.len() as i64;
    let name = op.name();
    let (stage_id, undo_until_ms) = staged.lock().stage(Pending { action: op, todo, timer: None }, now, UNDO_WINDOW);
    let timer = tauri::async_runtime::spawn({
        let app = app.clone();
        let id = stage_id.clone();
        async move {
            tokio::time::sleep(UNDO_WINDOW).await;
            commit_due(&app, &id).await;
        }
    });
    match staged.lock().get_mut(&stage_id) {
        Some(p) => p.timer = Some(timer),
        None => timer.abort(),
    }
    changed.emit(&app);
    info!(action = name, done, failed = report.failed.len(), "действие применено локально, сервер ждёт окно отмены");
    Ok(StagedDto { stage_id, done, failed: report.failed, undo_until_ms })
}

/// Откатить отложенное действие, пока окно отмены не истекло; иначе «Отмена уже невозможна».
#[tauri::command]
pub(crate) async fn items_undo(stage_id: String, app: AppHandle, state: State<'_, AppState>, ws: State<'_, WriteState>, staged: State<'_, StagedState>) -> CmdResult<WriteReport> {
    // Срок проверяется по моменту запроса, а не получения замка: пока другая отправка (коммит
    // предыдущего действия, автопометка прочитанным) держит его, своевременный клик «Отменить» не
    // должен превращаться в «Отмена уже невозможна». Замок честный (очередь), поэтому опоздавший
    // к сроку таймер получит его после и уже не найдёт записи.
    let requested = now_ms();
    let _busy = ws.lock().await;
    let mut p = staged.lock().undo(&stage_id, requested)?;
    if let Some(t) = p.timer.take() {
        t.abort();
    }
    let mut changed = Changed::default();
    changed.operation(&p.action, &p.todo);
    rollback(&p.action, &state.store, &p.todo, &mut changed);
    changed.emit(&app);
    info!(action = p.action.name(), items = p.todo.len(), "отложенное действие отменено");
    Ok(WriteReport { done: p.todo.len() as i64, failed: Vec::new() })
}

/// Отправить отложенное действие на сервер сейчас. Если его уже нет (отправлено или отменено) —
/// пустой отчёт.
#[tauri::command]
pub(crate) async fn items_commit(stage_id: String, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, ws: State<'_, WriteState>, staged: State<'_, StagedState>) -> CmdResult<WriteReport> {
    let _busy = ws.lock().await;
    let Some(mut p) = staged.lock().commit(&stage_id) else { return Ok(WriteReport::default()) };
    if let Some(t) = p.timer.take() {
        t.abort();
    }
    info!(action = p.action.name(), items = p.todo.len(), "отложенное действие отправляется по запросу окна");
    Ok(commit_pending(&app, &state, &sync, &p).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::FolderId;

    const WINDOW: Duration = Duration::from_secs(6);

    #[test]
    fn stage_then_undo_before_deadline() {
        let mut book: StageBook<&str> = StageBook::default();
        let (id, until) = book.stage("удалить", 1_000, WINDOW);
        assert_eq!(until, 7_000, "срок — через 6 с");
        assert_eq!(book.entries.len(), 1);
        assert_eq!(book.undo(&id, 6_999), Ok("удалить"), "в последний миг ещё можно");
        assert!(book.entries.is_empty());
        assert_eq!(book.undo(&id, 7_000).unwrap_err(), UNDO_EXPIRED, "второй откат — ошибка");
        assert_eq!(book.commit(&id), None, "коммитить нечего");
    }

    #[test]
    fn undo_after_deadline_fails_and_keeps_entry_for_timer() {
        let mut book: StageBook<&str> = StageBook::default();
        let (id, _) = book.stage("перенести", 1_000, WINDOW);
        assert_eq!(book.undo(&id, 7_001).unwrap_err(), UNDO_EXPIRED);
        assert_eq!(book.entries.len(), 1, "запись остаётся таймеру");
        assert_eq!(book.commit(&id), Some("перенести"));
        assert!(book.entries.is_empty());
        assert_eq!(book.undo("нет такого", 0).unwrap_err(), UNDO_EXPIRED);
    }

    #[test]
    fn commit_removes_and_drain_keeps_order() {
        let mut book: StageBook<u32> = StageBook::default();
        let (a, _) = book.stage(1, 10, WINDOW);
        let (b, _) = book.stage(2, 10, WINDOW);
        let (c, _) = book.stage(3, 11, WINDOW);
        assert_ne!(a, b, "идентификаторы уникальны и при одинаковом времени");
        assert_eq!(book.commit(&b), Some(2));
        assert_eq!(book.commit(&b), None, "повторный коммит пуст");
        *book.get_mut(&a).unwrap() = 10;
        assert_eq!(book.drain(), vec![10, 3], "все оставшиеся в порядке регистрации");
        assert!(book.entries.is_empty());
        assert!(book.drain().is_empty());
        assert_eq!(book.undo(&c, 11).unwrap_err(), UNDO_EXPIRED, "после выгрузки откатывать нечего");
    }

    fn folder(pk: i64, name: &str, parent: Option<&str>, dist: Option<&str>, class: &str) -> (i64, Folder) {
        (
            pk,
            Folder {
                id: FolderId { id: format!("F{pk}"), change_key: None },
                parent_id: parent.map(|p| FolderId { id: p.to_string(), change_key: None }),
                display_name: name.to_string(),
                distinguished: dist.map(String::from),
                folder_class: Some(class.into()),
                total_count: 0,
                unread_count: 0,
                child_folder_count: 0,
            },
        )
    }

    #[test]
    fn archive_folder_resolution() {
        // Корень ROOT в базе не хранится: «Входящие» и «Архив» — верхнего уровня.
        let inbox = folder(1, "Входящие", Some("ROOT"), Some("inbox"), "IPF.Note");
        let nested = folder(2, "Архив", Some("F1"), None, "IPF.Note");
        assert_eq!(find_archive_folder(&[inbox.clone(), nested.clone()]), None, "вложенный «Архив» не считается");
        let top = folder(3, " archive ", Some("ROOT"), None, "IPF.Note");
        assert_eq!(find_archive_folder(&[inbox.clone(), nested.clone(), top.clone()]), Some(3), "верхний уровень, любой регистр и пробелы");
        let calendar_archive = folder(4, "Архив", Some("ROOT"), None, "IPF.Appointment");
        assert_eq!(find_archive_folder(&[inbox.clone(), calendar_archive]), None, "не почтовая папка не подходит");
        let distinguished = folder(5, "Что угодно", Some("ROOT"), Some("archive"), "IPF.Note");
        assert_eq!(find_archive_folder(&[inbox.clone(), top, distinguished]), Some(5), "служебная archive важнее имени");
        let no_parent = folder(6, "Archive", None, None, "IPF.Note");
        assert_eq!(find_archive_folder(&[inbox, no_parent]), Some(6), "без родителя — верхний уровень");
        assert_eq!(find_archive_folder(&[]), None);
    }

    #[test]
    fn failure_summary_texts() {
        let f = |pk: i64, m: &str| WriteFailure { item_pk: pk, message: m.into() };
        assert_eq!(failure_summary(&[]), None);
        assert_eq!(failure_summary(&[f(1, "Нет связи с сервером")]).unwrap(), "Нет связи с сервером");
        assert_eq!(failure_summary(&[f(1, "Нет связи с сервером"), f(2, "Нет связи с сервером")]).unwrap(), "Нет связи с сервером (писем: 2)");
        assert_eq!(failure_summary(&[f(1, "Сервер отказал в доступе к письму"), f(2, "Письма уже нет на сервере")]).unwrap(), "Сервер отказал по 2 письмам: Сервер отказал в доступе к письму");
    }

    #[test]
    fn stage_action_uses_snake_case() {
        assert_eq!(serde_json::from_str::<StageAction>("\"archive\"").unwrap(), StageAction::Archive);
        assert_eq!(serde_json::from_str::<StageAction>("\"delete\"").unwrap(), StageAction::Delete);
        assert_eq!(serde_json::to_string(&StageAction::Move).unwrap(), "\"move\"");
        assert!(serde_json::from_str::<StageAction>("\"read\"").is_err(), "пометки идут без окна отмены");
        assert_eq!(UNDO_WINDOW, Duration::from_secs(6));
    }
}
