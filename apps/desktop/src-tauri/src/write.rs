//! Команды записи из окна: прочитано/непрочитано, флаг «к исполнению», перенос, удаление.
//!
//! Порядок одной команды: ссылки на письма из базы → локальное изменение сразу (окно видит
//! результат мгновенно) → сервер через транспорт процесса (`SyncState::transport`, без новых
//! входов, если сессия уже есть) пачками не больше `BATCH` идентификаторов (одна пачка — один
//! запрос к серверу) и с таймаутом на пачку → по ответу: успех записывает новый идентификатор,
//! отказ по письму откатывает его локальное изменение обратным действием (`Undo`) и попадает
//! в `failed` с русским текстом — кроме `ErrorItemNotFound`: письма на сервере уже нет, и оно
//! помечается удалённым (следующая синхронизация вернёт его в настоящей папке, если оно там);
//! отказ всего вызова (сессия, сеть, таймаут) откатывает всё, что сервер ещё не подтвердил.
//!
//! Один замок на все операции записи: два действия не переплетаются; синхронизация идёт
//! параллельно. В конце — событие `items-changed` с затронутыми беседами и папками, по нему
//! окно перечитывает открытую беседу, список бесед и счётчики папок.

use crate::ondemand::{describe_transport, timed_out};
use crate::sync::SyncState;
use crate::{err, AppState, CmdResult};
use aiowa_core::{FolderId, FolderRef, ItemId};
use aiowa_store::{ItemRef, Store};
use aiowa_transport::{DeleteMode, MailTransport, TransportError, WriteResult};
use serde::Serialize;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tracing::{info, warn};

/// Таймаут одной пачки на сервере.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Идентификаторов в одной пачке на сервер. Не больше `OwaConfig::get_item_batch` (пачки
/// операций записи в транспорте, `docs/protocol-owa-json.md` § 5e): тогда одна пачка команды —
/// ровно один запрос к серверу, и отказ целого вызова означает, что сервер ничего из неё не сделал.
const BATCH: usize = 25;
/// Известное имя папки «Удалённые» в базе.
const DELETED_ITEMS: &str = "deleteditems";
/// Имя события для окна.
const ITEMS_CHANGED: &str = "items-changed";

/// Состояние команд записи: один замок на все операции.
#[derive(Default)]
pub(crate) struct WriteState {
    lock: tokio::sync::Mutex<()>,
}

/// Отказ по одному письму; локальное изменение по нему уже откачено.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct WriteFailure {
    item_pk: i64,
    /// Текст для пользователя, по-русски, без ответов сервера.
    message: String,
}

/// Итог команды записи.
#[derive(Debug, Default, Serialize)]
pub(crate) struct WriteReport {
    /// Писем, подтверждённых сервером.
    done: i64,
    failed: Vec<WriteFailure>,
}

/// Событие `items-changed`.
#[derive(Debug, Default, Clone, Serialize)]
struct ItemsChanged {
    conversation_pks: Vec<i64>,
    folder_pks: Vec<i64>,
}

/// Событие `items-changed` окну от другой команды (ответ на приглашение по письму): окно
/// перечитает беседу и папку. Ошибка отправки только в журнал.
pub(crate) fn emit_items_changed(app: &AppHandle, conversation_pks: Vec<i64>, folder_pks: Vec<i64>) {
    if let Err(e) = app.emit(ITEMS_CHANGED, ItemsChanged { conversation_pks, folder_pks }) {
        warn!(error = %e, "не удалось отправить событие items-changed");
    }
}

/// Что затронуто: собирается по ходу операции (применение, подтверждение, откат), в конце
/// уходит событием в окно.
#[derive(Default)]
struct Changed {
    conversations: BTreeSet<i64>,
    folders: BTreeSet<i64>,
}

impl Changed {
    fn conversations(&mut self, pks: impl IntoIterator<Item = i64>) {
        self.conversations.extend(pks);
    }

    fn folder(&mut self, folder_pk: i64) {
        self.folders.insert(folder_pk);
    }

    fn item(&mut self, r: &ItemRef) {
        self.folders.insert(r.folder_pk);
        self.conversations.extend(r.conversation_pk);
    }

    fn is_empty(&self) -> bool {
        self.conversations.is_empty() && self.folders.is_empty()
    }

    fn into_event(self) -> ItemsChanged {
        ItemsChanged { conversation_pks: self.conversations.into_iter().collect(), folder_pks: self.folders.into_iter().collect() }
    }
}

/// Операция записи: как применить локально, что вызвать на сервере, как подтвердить и откатить.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Read(bool),
    Flag(bool),
    /// Перенос в папку базы `folder_pk`; `target` — она же для сервера.
    Move { folder_pk: i64, target: FolderRef },
    /// В «Удалённые» (`DeleteItem` MoveToDeletedItems); локально — перенос в папку `deleted_pk`.
    ToDeleted { deleted_pk: i64 },
    /// Навсегда (`DeleteItem` HardDelete); локально — `deleted = 1`.
    Hard,
}

/// Обратное действие: возвращает одно письмо в состояние до операции при отказе сервера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Undo {
    SetRead(bool),
    SetFlag(bool),
    /// Вернуть в исходную папку (идентификатор не менялся: сервер операцию не подтвердил).
    MoveTo(i64),
    /// Снять `deleted = 1`.
    Restore,
}

impl Action {
    fn name(&self) -> &'static str {
        match self {
            Action::Read(true) => "прочитано",
            Action::Read(false) => "непрочитано",
            Action::Flag(true) => "флаг",
            Action::Flag(false) => "снять флаг",
            Action::Move { .. } => "перенос",
            Action::ToDeleted { .. } => "в удалённые",
            Action::Hard => "удалить навсегда",
        }
    }

    /// Папка назначения (для события), если операция её имеет.
    fn target_folder(&self) -> Option<i64> {
        match self {
            Action::Move { folder_pk, .. } => Some(*folder_pk),
            Action::ToDeleted { deleted_pk } => Some(*deleted_pk),
            Action::Read(_) | Action::Flag(_) | Action::Hard => None,
        }
    }

    /// Почему операция к письму неприменима ещё до сервера (текст для `failed`); `None` — применима.
    fn precheck(&self, r: &ItemRef) -> Option<&'static str> {
        match self {
            Action::Move { folder_pk, .. } if r.folder_pk == *folder_pk => Some("Письмо уже в этой папке"),
            Action::ToDeleted { deleted_pk } if r.folder_pk == *deleted_pk => Some("Письмо уже в «Удалённых»: используйте удаление навсегда"),
            _ => None,
        }
    }

    /// Письмо уже в нужном состоянии (прочитано/флаг): менять нечего, на сервер не ходим,
    /// в отчёт не попадает. Так «снять флаг» по всей беседе не шлёт `UpdateItem` по письмам без флага.
    fn already_applied(&self, r: &ItemRef) -> bool {
        match self {
            Action::Read(read) => r.is_read == *read,
            Action::Flag(flagged) => r.is_flagged == *flagged,
            Action::Move { .. } | Action::ToDeleted { .. } | Action::Hard => false,
        }
    }

    /// Обратное действие для письма `r` (его состояние до операции).
    fn undo_for(&self, r: &ItemRef) -> Undo {
        match self {
            Action::Read(_) => Undo::SetRead(r.is_read),
            Action::Flag(_) => Undo::SetFlag(r.is_flagged),
            Action::Move { .. } | Action::ToDeleted { .. } => Undo::MoveTo(r.folder_pk),
            Action::Hard => Undo::Restore,
        }
    }

    /// Локальное применение ко всем письмам (оптимистично). Возвращает затронутые беседы.
    fn apply(&self, store: &Store, refs: &[ItemRef]) -> aiowa_store::Result<Vec<i64>> {
        let pks: Vec<i64> = refs.iter().map(|r| r.item_pk).collect();
        match self {
            Action::Read(read) => store.set_read_local(&pks, *read),
            Action::Flag(flagged) => store.set_flag_local(&pks, *flagged),
            Action::Move { folder_pk, .. } | Action::ToDeleted { deleted_pk: folder_pk } => {
                for r in refs {
                    store.move_local(r.item_pk, *folder_pk, None)?;
                }
                Ok(refs.iter().filter_map(|r| r.conversation_pk).collect())
            }
            Action::Hard => store.mark_deleted_local(&pks),
        }
    }

    /// Подтверждение сервером: записать идентификатор после операции.
    fn confirm(&self, store: &Store, r: &ItemRef, id: &ItemId) -> aiowa_store::Result<()> {
        match self {
            Action::Read(_) | Action::Flag(_) => store.update_item_id(r.item_pk, id),
            // Перенос уже сделан локально; новый идентификатор записывает тот же `move_local`:
            // он учитывает двойника, которого параллельная синхронизация могла создать в целевой папке.
            Action::Move { folder_pk, .. } | Action::ToDeleted { deleted_pk: folder_pk } => store.move_local(r.item_pk, *folder_pk, Some(id)),
            Action::Hard => Ok(()),
        }
    }

    async fn send(&self, t: &impl MailTransport, ids: &[ItemId]) -> aiowa_transport::Result<Vec<WriteResult>> {
        match self {
            Action::Read(read) => t.set_read(ids, *read).await,
            Action::Flag(flagged) => t.set_flag(ids, *flagged).await,
            Action::Move { target, .. } => t.move_items(ids, target).await,
            Action::ToDeleted { .. } => t.delete_items(ids, DeleteMode::ToDeletedItems).await,
            Action::Hard => t.delete_items(ids, DeleteMode::Hard).await,
        }
    }
}

impl Undo {
    fn run(self, store: &Store, item_pk: i64) -> aiowa_store::Result<Vec<i64>> {
        match self {
            Undo::SetRead(read) => store.set_read_local(&[item_pk], read),
            Undo::SetFlag(flagged) => store.set_flag_local(&[item_pk], flagged),
            Undo::MoveTo(folder_pk) => store.move_local(item_pk, folder_pk, None).map(|()| Vec::new()),
            Undo::Restore => store.restore_deleted_local(&[item_pk]),
        }
    }
}

/// Откат локального изменения писем. Ошибка отката только в журнал: событие заставит окно
/// перечитать базу, а следующая синхронизация сверит её с сервером.
fn rollback(action: &Action, store: &Store, refs: &[ItemRef], changed: &mut Changed) {
    for r in refs {
        let undo = action.undo_for(r);
        match undo.run(store, r.item_pk) {
            Ok(convs) => changed.conversations(convs),
            Err(e) => warn!(item_pk = r.item_pk, ?undo, error = %e, "откат локального изменения не удался"),
        }
    }
}

/// Код отказа сервера «письма нет» (удалено или перенесено другим клиентом).
const ITEM_NOT_FOUND: &str = "ErrorItemNotFound";

/// Отказ сервера по одному письму. Обычно — откат локального изменения; при `ErrorItemNotFound`
/// откатывать нечего: письма по этому идентификатору на сервере нет, и строка помечается удалённой
/// (как делает синхронизация по тому же коду), иначе она жила бы в списке до следующего прогона,
/// а для удалённого навсегда — воскресла бы. Следующая синхронизация вернёт письмо в настоящей
/// папке через `Create`, если оно там есть.
fn item_failed(action: &Action, store: &Store, r: &ItemRef, code: &str, changed: &mut Changed) {
    if code != ITEM_NOT_FOUND {
        rollback(action, store, std::slice::from_ref(r), changed);
        return;
    }
    // Для `Hard` строка уже `deleted = 1` — вызов ничего не меняет.
    match store.mark_deleted_local(&[r.item_pk]) {
        Ok(convs) => changed.conversations(convs),
        Err(e) => warn!(item_pk = r.item_pk, error = %e, "письмо, которого нет на сервере, не помечено удалённым"),
    }
}

/// Текст отказа сервера по одному письму: частые коды словами, остальные как у `describe_transport`.
fn describe_failure(code: &str, message: &str) -> String {
    match code {
        ITEM_NOT_FOUND => "Письма уже нет на сервере".into(),
        "ErrorAccessDenied" => "Сервер отказал в доступе к письму".into(),
        "ErrorCannotDeleteObject" => "Сервер не разрешил удалить письмо".into(),
        "ErrorMoveCopyFailed" => "Сервер не смог перенести письмо".into(),
        "ErrorIrresolvableConflict" => "Письмо изменено на сервере, обновите папку и повторите".into(),
        _ => describe_transport(&TransportError::Server { code: code.to_string(), message: message.to_string() }),
    }
}

/// Письма, которые сервер ещё не подтвердил, начиная с пачки `batch_index`.
fn unsent(todo: &[ItemRef], batch_index: usize) -> &[ItemRef] {
    &todo[(batch_index * BATCH).min(todo.len())..]
}

/// Уникальные ключи в порядке первого появления.
fn dedup(item_pks: &[i64]) -> Vec<i64> {
    let mut seen = HashSet::with_capacity(item_pks.len());
    item_pks.iter().copied().filter(|pk| seen.insert(*pk)).collect()
}

/// Выполняет операцию над письмами. `changed` заполняется и при ошибке: локальные изменения
/// и их откаты окно должно увидеть в любом случае.
async fn execute(state: &AppState, sync: &SyncState, item_pks: &[i64], action: &Action, changed: &mut Changed) -> CmdResult<WriteReport> {
    let store = &state.store;
    let mut report = WriteReport::default();

    // Ссылки на письма: неизвестные и уже удалённые — сразу в отказ.
    let unique = dedup(item_pks);
    let refs = store.item_refs(&unique).map_err(err)?;
    let known: HashSet<i64> = refs.iter().map(|r| r.item_pk).collect();
    for &pk in &unique {
        if !known.contains(&pk) {
            report.failed.push(WriteFailure { item_pk: pk, message: "Письмо уже удалено".into() });
        }
    }

    // Проверки до сервера.
    let mut todo: Vec<ItemRef> = Vec::with_capacity(refs.len());
    for r in refs {
        if action.already_applied(&r) {
            continue;
        }
        match action.precheck(&r) {
            Some(why) => report.failed.push(WriteFailure { item_pk: r.item_pk, message: why.into() }),
            None => todo.push(r),
        }
    }
    if todo.is_empty() {
        return Ok(report);
    }
    for r in &todo {
        changed.item(r);
    }
    if let Some(f) = action.target_folder() {
        changed.folder(f);
    }

    // Локально, сразу.
    match action.apply(store, &todo) {
        Ok(convs) => changed.conversations(convs),
        Err(e) => {
            warn!(action = action.name(), error = %e, "локальное применение не удалось");
            rollback(action, store, &todo, changed);
            return Err(format!("Ошибка локальной базы: {e}"));
        }
    }

    // Транспорт процесса: без новых входов, если сессия уже есть.
    let transport = match sync.transport().await {
        Ok(t) => t,
        Err(e) => {
            rollback(action, store, &todo, changed);
            return Err(e.to_string());
        }
    };

    // На сервер пачками. Отказ целого вызова откатывает всё, что сервер ещё не подтвердил:
    // до первого ответа это ошибка команды, после — отказ по оставшимся письмам (подтверждённые
    // пачки уже применены на сервере, их откатывать нельзя).
    let mut answered = 0usize;
    for (i, chunk) in todo.chunks(BATCH).enumerate() {
        let ids: Vec<ItemId> = chunk.iter().map(|r| r.id.clone()).collect();
        let outcome = match tokio::time::timeout(WRITE_TIMEOUT, action.send(&transport, &ids)).await {
            Ok(Ok(results)) => Ok(results),
            Ok(Err(e)) => {
                warn!(action = action.name(), error = %e, "операция записи отклонена целиком");
                sync.invalidate_on(&e).await;
                Err(describe_transport(&e))
            }
            Err(_) => Err(timed_out(WRITE_TIMEOUT)),
        };
        match outcome {
            Ok(results) => {
                let mut results = results.into_iter();
                for r in chunk {
                    match results.next() {
                        Some(WriteResult::Ok { id }) => {
                            // Сервер операцию выполнил: локальное состояние уже верное, не хватает
                            // только идентификатора; его подхватит следующая синхронизация.
                            if let Err(e) = action.confirm(store, r, &id) {
                                warn!(item_pk = r.item_pk, error = %e, "идентификатор после операции не записан");
                            }
                            report.done += 1;
                        }
                        Some(WriteResult::Failed { code, message, .. }) => {
                            warn!(item_pk = r.item_pk, code, "сервер отказал по письму");
                            item_failed(action, store, r, &code, changed);
                            report.failed.push(WriteFailure { item_pk: r.item_pk, message: describe_failure(&code, &message) });
                        }
                        None => {
                            rollback(action, store, std::slice::from_ref(r), changed);
                            report.failed.push(WriteFailure { item_pk: r.item_pk, message: "Сервер ответил неожиданно".into() });
                        }
                    }
                }
                answered += chunk.len();
            }
            Err(message) => {
                let rest = unsent(&todo, i);
                rollback(action, store, rest, changed);
                if answered == 0 {
                    return Err(message);
                }
                report.failed.extend(rest.iter().map(|r| WriteFailure { item_pk: r.item_pk, message: message.clone() }));
                break;
            }
        }
    }
    info!(action = action.name(), done = report.done, failed = report.failed.len(), "операция записи завершена");
    Ok(report)
}

/// Общая обвязка команд: замок на операции записи, выполнение, событие окну.
async fn run(app: &AppHandle, state: &AppState, sync: &SyncState, ws: &WriteState, item_pks: &[i64], action: Action) -> CmdResult<WriteReport> {
    let _busy = ws.lock.lock().await;
    let mut changed = Changed::default();
    let result = execute(state, sync, item_pks, &action, &mut changed).await;
    if !changed.is_empty() {
        if let Err(e) = app.emit(ITEMS_CHANGED, changed.into_event()) {
            warn!(error = %e, "не удалось отправить событие items-changed");
        }
        // Локальная запись уже поправила счётчики папок в базе: заголовок окна и значок на
        // панели задач обновляются сразу, не дожидаясь следующей синхронизации.
        if let Some(notify) = app.try_state::<Arc<crate::notify::NotifyState>>() {
            notify.refresh_unread(app);
        }
    }
    result
}

// --- команды Tauri -----------------------------------------------------------

/// Пометить письма прочитанными или непрочитанными.
#[tauri::command]
pub(crate) async fn items_mark_read(item_pks: Vec<i64>, read: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, ws: State<'_, WriteState>) -> CmdResult<WriteReport> {
    run(&app, &state, &sync, &ws, &item_pks, Action::Read(read)).await
}

/// Поставить или снять флаг «к исполнению».
#[tauri::command]
pub(crate) async fn items_set_flag(item_pks: Vec<i64>, flagged: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, ws: State<'_, WriteState>) -> CmdResult<WriteReport> {
    run(&app, &state, &sync, &ws, &item_pks, Action::Flag(flagged)).await
}

/// Перенести письма в папку `folder_pk` (ключ локальной базы).
#[tauri::command]
pub(crate) async fn items_move(item_pks: Vec<i64>, folder_pk: i64, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, ws: State<'_, WriteState>) -> CmdResult<WriteReport> {
    let folder = state.store.folder_by_pk(state.account, folder_pk).map_err(err)?.ok_or_else(|| "Папка не найдена".to_string())?;
    let target = FolderRef::Id(FolderId { id: folder.server_id, change_key: None });
    run(&app, &state, &sync, &ws, &item_pks, Action::Move { folder_pk, target }).await
}

/// Удалить письма: `permanent = false` — в «Удалённые» (письмо, уже лежащее там, отказывает),
/// `permanent = true` — навсегда.
#[tauri::command]
pub(crate) async fn items_delete(item_pks: Vec<i64>, permanent: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, ws: State<'_, WriteState>) -> CmdResult<WriteReport> {
    let action = if permanent {
        Action::Hard
    } else {
        let (deleted_pk, _) = state.store.folder_by_distinguished(state.account, DELETED_ITEMS).map_err(err)?.ok_or_else(|| "Папка «Удалённые» не найдена в локальной базе: выполните синхронизацию".to_string())?;
        Action::ToDeleted { deleted_pk }
    };
    run(&app, &state, &sync, &ws, &item_pks, action).await
}

/// Письма беседы (не удалённые), при `folder_pk` — только лежащие в этой папке; новые первыми.
#[tauri::command]
pub(crate) fn conversation_item_pks(conversation_pk: i64, folder_pk: Option<i64>, state: State<'_, AppState>) -> CmdResult<Vec<i64>> {
    state.store.conversation_item_pks(state.account, conversation_pk, folder_pk).map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{ConversationId, Folder, FolderId, Importance, ItemChange, MessageMeta, Participant};

    fn item_ref(item_pk: i64, folder_pk: i64, is_read: bool, is_flagged: bool) -> ItemRef {
        ItemRef { item_pk, folder_pk, id: ItemId { id: format!("id{item_pk}"), change_key: Some("ck".into()) }, body_state: 1, is_read, is_flagged, conversation_pk: Some(7) }
    }

    #[test]
    fn batches_are_capped_and_rest_is_everything_unsent() {
        // Одна пачка команды — один запрос транспорта: иначе отказ второго запроса внутри одной
        // пачки терял бы подтверждённые результаты первого, и откат задел бы уже изменённое на сервере.
        assert!(BATCH <= aiowa_transport_owa::OwaConfig::default().get_item_batch, "пачка команды не больше пачки транспорта");
        let todo: Vec<ItemRef> = (1..=123).map(|pk| item_ref(pk, 1, false, false)).collect();
        let sizes: Vec<usize> = todo.chunks(BATCH).map(<[ItemRef]>::len).collect();
        assert_eq!(sizes, vec![25, 25, 25, 25, 23]);
        // Отказ целого вызова на второй пачке: откатываются вторая и дальше, первая уже подтверждена.
        let rest = unsent(&todo, 1);
        assert_eq!(rest.len(), 98);
        assert_eq!(rest.first().map(|r| r.item_pk), Some(26));
        assert_eq!(unsent(&todo, 0).len(), 123, "отказ на первой пачке откатывает всё");
        assert_eq!(unsent(&todo, 2).len(), 73);
        assert_eq!(unsent(&todo, 4).len(), 23);
        assert!(unsent(&todo, 5).is_empty(), "за последней пачкой откатывать нечего");
        let one: Vec<ItemRef> = vec![item_ref(1, 1, false, false)];
        assert_eq!(one.chunks(BATCH).count(), 1);
        assert!(Vec::<ItemRef>::new().chunks(BATCH).next().is_none(), "пустой список — ни одного вызова сервера");
    }

    #[test]
    fn already_applied_skips_read_and_flag_in_target_state_only() {
        let unread_flagged = item_ref(1, 10, false, true);
        let read_plain = item_ref(2, 10, true, false);
        assert!(Action::Read(true).already_applied(&read_plain));
        assert!(!Action::Read(true).already_applied(&unread_flagged));
        assert!(Action::Read(false).already_applied(&unread_flagged));
        assert!(Action::Flag(false).already_applied(&read_plain), "снять флаг с письма без флага — нечего");
        assert!(!Action::Flag(false).already_applied(&unread_flagged));
        assert!(Action::Flag(true).already_applied(&unread_flagged));
        let target = FolderRef::Id(FolderId { id: "F".into(), change_key: None });
        assert!(!Action::Move { folder_pk: 20, target }.already_applied(&read_plain), "перенос и удаление решает precheck");
        assert!(!Action::ToDeleted { deleted_pk: 30 }.already_applied(&read_plain));
        assert!(!Action::Hard.already_applied(&read_plain));
    }

    #[test]
    fn undo_restores_state_before_operation() {
        let unread_flagged = item_ref(1, 10, false, true);
        let read_plain = item_ref(2, 10, true, false);
        assert_eq!(Action::Read(true).undo_for(&unread_flagged), Undo::SetRead(false));
        assert_eq!(Action::Read(false).undo_for(&read_plain), Undo::SetRead(true));
        assert_eq!(Action::Read(true).undo_for(&read_plain), Undo::SetRead(true), "уже прочитанное остаётся прочитанным");
        assert_eq!(Action::Flag(false).undo_for(&unread_flagged), Undo::SetFlag(true));
        assert_eq!(Action::Flag(true).undo_for(&read_plain), Undo::SetFlag(false));
        let target = FolderRef::Id(FolderId { id: "F".into(), change_key: None });
        assert_eq!(Action::Move { folder_pk: 20, target }.undo_for(&read_plain), Undo::MoveTo(10), "назад в исходную папку");
        assert_eq!(Action::ToDeleted { deleted_pk: 30 }.undo_for(&unread_flagged), Undo::MoveTo(10));
        assert_eq!(Action::Hard.undo_for(&read_plain), Undo::Restore);
    }

    #[test]
    fn prechecks_reject_same_folder_and_deleted_items() {
        let in_inbox = item_ref(1, 10, true, false);
        let in_deleted = item_ref(2, 30, true, false);
        let target = FolderRef::Id(FolderId { id: "F".into(), change_key: None });
        assert_eq!(Action::Move { folder_pk: 10, target: target.clone() }.precheck(&in_inbox), Some("Письмо уже в этой папке"));
        assert_eq!(Action::Move { folder_pk: 20, target }.precheck(&in_inbox), None);
        assert_eq!(Action::ToDeleted { deleted_pk: 30 }.precheck(&in_deleted), Some("Письмо уже в «Удалённых»: используйте удаление навсегда"));
        assert_eq!(Action::ToDeleted { deleted_pk: 30 }.precheck(&in_inbox), None);
        assert_eq!(Action::Hard.precheck(&in_deleted), None, "навсегда можно откуда угодно");
        assert_eq!(Action::Read(true).precheck(&in_deleted), None);
        assert_eq!(Action::Flag(true).precheck(&in_deleted), None);
    }

    #[test]
    fn target_folders_and_dedup() {
        assert_eq!(Action::ToDeleted { deleted_pk: 30 }.target_folder(), Some(30));
        assert_eq!(Action::Move { folder_pk: 20, target: FolderRef::Id(FolderId { id: "F".into(), change_key: None }) }.target_folder(), Some(20));
        assert_eq!(Action::Hard.target_folder(), None);
        assert_eq!(Action::Read(true).target_folder(), None);
        assert_eq!(dedup(&[3, 1, 3, 2, 1]), vec![3, 1, 2]);
        assert!(dedup(&[]).is_empty());
    }

    #[test]
    fn failure_texts_are_russian_without_server_details() {
        assert_eq!(describe_failure("ErrorItemNotFound", "The specified object was not found in the store."), "Письма уже нет на сервере");
        let other = describe_failure("ErrorInternalServerError", "<html>подробности</html>");
        assert_eq!(other, "Сервер отказал: ErrorInternalServerError");
        assert!(!other.contains("подробности"));
    }

    // --- обратные действия на настоящей базе ----------------------------------

    fn tmp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("aiowa-desktop-write-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(dir.join("mail.db")).unwrap()
    }

    fn folder(name: &str, dist: Option<&str>) -> Folder {
        Folder {
            id: FolderId { id: format!("F-{name}"), change_key: Some("ck".into()) },
            parent_id: None,
            display_name: name.to_string(),
            distinguished: dist.map(String::from),
            folder_class: Some("IPF.Note".into()),
            total_count: 10,
            unread_count: 1,
            child_folder_count: 0,
        }
    }

    fn meta(id: &str, received: &str, conv: &str, is_read: bool) -> MessageMeta {
        MessageMeta {
            item_id: ItemId { id: id.to_string(), change_key: Some("c".into()) },
            parent_folder_id: None,
            conversation_id: Some(ConversationId(conv.to_string())),
            internet_message_id: Some(format!("<{id}@x>")),
            item_class: Some("IPM.Note".into()),
            subject: Some("Тема".into()),
            from: Some(Participant { name: Some("Отправитель".into()), email: Some("s@x.ru".into()), routing_type: None }),
            sender: None,
            display_to: None,
            received_at: Some(received.parse().unwrap()),
            sent_at: None,
            last_modified_at: None,
            is_read,
            is_draft: false,
            has_attachments: false,
            importance: Importance::Normal,
            flag: Default::default(),
            size: Some(100),
            preview: None,
        }
    }

    /// Применение и откат каждой операции возвращают письмо ровно в исходное состояние
    /// (папка, прочитанность, флаг, удалённость); подтверждение записывает идентификатор.
    #[test]
    fn apply_then_undo_is_identity_on_store() {
        let s = tmp_store("undo");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox")), folder("Архив", None), folder("Удаленные", Some("deleteditems"))]).unwrap();
        let (inbox, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let (deleted, _) = s.folder_by_distinguished(acc, DELETED_ITEMS).unwrap().unwrap();
        let archive = s.resolve_folder(acc, "Архив").unwrap().unwrap().folder_pk;
        s.apply_item_changes(acc, inbox, &[ItemChange::Create(meta("A", "2026-10-01T10:00:00Z", "c1", false)), ItemChange::Create(meta("B", "2026-10-02T10:00:00Z", "c1", true))]).unwrap();
        let conv = s.list_conversations(acc, inbox, 10).unwrap()[0].conversation_pk;
        let pks = s.conversation_item_pks(acc, conv, Some(inbox)).unwrap();
        assert_eq!(pks.len(), 2);
        let before = s.item_refs(&pks).unwrap();
        assert_eq!(before.len(), 2);
        assert_eq!(before[0].conversation_pk, Some(conv), "ссылка знает беседу для события");
        let snapshot = |s: &Store| -> Vec<(i64, bool, bool, bool)> {
            pks.iter().map(|&pk| s.item_ref(pk).unwrap().map(|r| (r.folder_pk, r.is_read, r.is_flagged, true)).unwrap_or((0, false, false, false))).collect()
        };
        let initial = snapshot(&s);
        let target = FolderRef::Id(FolderId { id: "F-Архив".into(), change_key: None });
        let actions = [Action::Read(true), Action::Read(false), Action::Flag(true), Action::Move { folder_pk: archive, target }, Action::ToDeleted { deleted_pk: deleted }, Action::Hard];
        for action in &actions {
            let mut changed = Changed::default();
            action.apply(&s, &before).unwrap_or_else(|e| panic!("{}: {e}", action.name()));
            assert_ne!(snapshot(&s), initial, "{}: локальное применение что-то меняет", action.name());
            rollback(action, &s, &before, &mut changed);
            assert_eq!(snapshot(&s), initial, "{}: откат возвращает исходное состояние", action.name());
        }
        // Отказ по одному письму из пачки: откатывается только оно (A было непрочитанным,
        // B — прочитанным; порядок ссылок — новые первыми, поэтому ищем по состоянию).
        let was_unread = before.iter().find(|r| !r.is_read).expect("A непрочитано");
        let was_read = before.iter().find(|r| r.is_read).expect("B прочитано");
        let read_all = Action::Read(true);
        read_all.apply(&s, &before).unwrap();
        assert!(s.item_ref(was_unread.item_pk).unwrap().unwrap().is_read, "оптимистично прочитано");
        rollback(&read_all, &s, std::slice::from_ref(was_unread), &mut Changed::default());
        assert!(!s.item_ref(was_unread.item_pk).unwrap().unwrap().is_read, "откат вернул непрочитанное");
        assert!(s.item_ref(was_read.item_pk).unwrap().unwrap().is_read, "соседнее письмо не тронуто");
        rollback(&read_all, &s, std::slice::from_ref(was_read), &mut Changed::default());
        assert!(s.item_ref(was_read.item_pk).unwrap().unwrap().is_read, "откат уже прочитанного оставляет его прочитанным");
        assert_eq!(snapshot(&s), initial);

        // Подтверждение: новый ChangeKey после UpdateItem, новый идентификатор после переноса.
        read_all.apply(&s, &before).unwrap();
        read_all.confirm(&s, &before[0], &ItemId { id: before[0].id.id.clone(), change_key: Some("ck2".into()) }).unwrap();
        assert_eq!(s.item_ref(before[0].item_pk).unwrap().unwrap().id.change_key.as_deref(), Some("ck2"));
        let to_deleted = Action::ToDeleted { deleted_pk: deleted };
        to_deleted.apply(&s, std::slice::from_ref(&before[1])).unwrap();
        to_deleted.confirm(&s, &before[1], &ItemId { id: "B-new".into(), change_key: Some("k".into()) }).unwrap();
        let moved = s.item_ref(before[1].item_pk).unwrap().unwrap();
        assert_eq!((moved.folder_pk, moved.id.id.as_str()), (deleted, "B-new"));
        assert_eq!(to_deleted.precheck(&moved), Some("Письмо уже в «Удалённых»: используйте удаление навсегда"));
    }

    /// Отказ `ErrorItemNotFound` не откатывает письмо, а помечает его удалённым: исходная папка
    /// теряет его из счётчиков, целевая — не приобретает; для «навсегда» строка остаётся удалённой.
    /// Любой другой код по-прежнему откатывает.
    #[test]
    fn item_not_found_marks_item_deleted_instead_of_rollback() {
        let s = tmp_store("notfound");
        let acc = s.account("h", "u").unwrap();
        // Счётчики папки как на сервере: три письма, два непрочитанных (A и C), иначе локальный
        // пересчёт упёрся бы в ноль раньше времени.
        let mut inbox_folder = folder("Входящие", Some("inbox"));
        inbox_folder.total_count = 3;
        inbox_folder.unread_count = 2;
        s.upsert_folders(acc, &[inbox_folder, folder("Удаленные", Some("deleteditems"))]).unwrap();
        let (inbox, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let (deleted, _) = s.folder_by_distinguished(acc, DELETED_ITEMS).unwrap().unwrap();
        s.apply_item_changes(acc, inbox, &[ItemChange::Create(meta("A", "2026-10-01T10:00:00Z", "c1", false)), ItemChange::Create(meta("B", "2026-10-02T10:00:00Z", "c1", true)), ItemChange::Create(meta("C", "2026-10-03T10:00:00Z", "c2", false))]).unwrap();
        let counts = |s: &Store, pk: i64| -> (i64, i64) {
            let (_, f) = s.list_folders(acc).unwrap().into_iter().find(|(p, _)| *p == pk).unwrap();
            (f.total_count, f.unread_count)
        };
        let inbox0 = counts(&s, inbox);
        let deleted0 = counts(&s, deleted);
        let conv = s.list_conversations(acc, inbox, 10).unwrap().into_iter().find(|c| c.total_count == 2).unwrap().conversation_pk;
        let pks = s.conversation_item_pks(acc, conv, Some(inbox)).unwrap();
        let refs = s.item_refs(&pks).unwrap();
        let a = refs.iter().find(|r| !r.is_read).expect("A непрочитано").clone();
        let b = refs.iter().find(|r| r.is_read).expect("B прочитано").clone();

        // В «Удалённые», сервер: письма нет → строка удалена, «Входящие» −1 (и −1 непрочитанное), «Удалённые» без изменений.
        let to_deleted = Action::ToDeleted { deleted_pk: deleted };
        let mut changed = Changed::default();
        to_deleted.apply(&s, std::slice::from_ref(&a)).unwrap();
        item_failed(&to_deleted, &s, &a, ITEM_NOT_FOUND, &mut changed);
        assert!(s.item_ref(a.item_pk).unwrap().is_none(), "письма больше нет среди живых");
        assert_eq!(counts(&s, inbox), (inbox0.0 - 1, inbox0.1 - 1));
        assert_eq!(counts(&s, deleted), deleted0);
        assert!(changed.conversations.contains(&conv), "окно перечитает беседу");

        // Прочитано, сервер: письма нет → не откат к «прочитано», а удаление.
        let read = Action::Read(false);
        let mut changed = Changed::default();
        read.apply(&s, std::slice::from_ref(&b)).unwrap();
        item_failed(&read, &s, &b, ITEM_NOT_FOUND, &mut changed);
        assert!(s.item_ref(b.item_pk).unwrap().is_none());
        assert_eq!(counts(&s, inbox), (inbox0.0 - 2, inbox0.1 - 1), "B было прочитанным и после apply непрочитанным: unread вернулся");

        // Навсегда, сервер: письма нет → остаётся удалённым, счётчики не трогаются.
        let other = s.list_conversations(acc, inbox, 10).unwrap().into_iter().find(|c| c.conversation_pk != conv && c.item_count > 0).expect("беседа c2");
        let c = s.item_refs(&s.conversation_item_pks(acc, other.conversation_pk, Some(inbox)).unwrap()).unwrap().remove(0);
        let after_two = counts(&s, inbox);
        Action::Hard.apply(&s, std::slice::from_ref(&c)).unwrap();
        let hard_applied = counts(&s, inbox);
        assert_eq!(hard_applied, (after_two.0 - 1, after_two.1 - 1));
        item_failed(&Action::Hard, &s, &c, ITEM_NOT_FOUND, &mut Changed::default());
        assert!(s.item_ref(c.item_pk).unwrap().is_none(), "удалённое навсегда не воскресает");
        assert_eq!(counts(&s, inbox), hard_applied, "повторная пометка счётчики не меняет");

        // Другой код — обычный откат.
        let s2 = tmp_store("notfound-other");
        let acc2 = s2.account("h", "u").unwrap();
        s2.upsert_folders(acc2, &[folder("Входящие", Some("inbox"))]).unwrap();
        let (inbox2, _) = s2.folder_by_distinguished(acc2, "inbox").unwrap().unwrap();
        s2.apply_item_changes(acc2, inbox2, &[ItemChange::Create(meta("A", "2026-10-01T10:00:00Z", "c1", true))]).unwrap();
        let r = s2.item_refs(&s2.conversation_item_pks(acc2, s2.list_conversations(acc2, inbox2, 10).unwrap()[0].conversation_pk, None).unwrap()).unwrap().remove(0);
        Action::Hard.apply(&s2, std::slice::from_ref(&r)).unwrap();
        item_failed(&Action::Hard, &s2, &r, "ErrorAccessDenied", &mut Changed::default());
        assert!(s2.item_ref(r.item_pk).unwrap().is_some(), "при обычном отказе письмо возвращается");
    }
}
