//! Раздел «Люди» (docs/obkatka.md п. 8): личные контакты ящика из локальной базы — список с
//! поиском, контакт по адресу для карточки и состояние папок контактов. Сервер отсюда не
//! спрашивается: контакты приходят синхронизацией вместе с почтой (`sync::SyncState`), о новых
//! окну сообщает событие `contacts-changed`. В журнал имена и адреса не пишутся.

use crate::{err, AppState, CmdResult};
use aiowa_core::ContactItem;
use aiowa_store::{ContactFolderInfo, ContactRow};
use serde::Serialize;
use tauri::State;

/// Сколько контактов (и отдельно групп) отдаёт список по умолчанию и самое большее.
const LIST_DEFAULT: usize = 1000;
const LIST_MAX: usize = 10_000;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContactDto {
    contact_pk: i64,
    folder_pk: i64,
    /// Имя для списка: `FileAs`, `DisplayName`, имя и фамилия, компания или первый адрес.
    name: String,
    /// Первый SMTP-адрес в нижнем регистре.
    email: Option<String>,
    is_group: bool,
    birthday_ms: Option<i64>,
    modified_ms: Option<i64>,
    /// Все поля контакта, как их отдал сервер (`aiowa_core::ContactItem`).
    item: ContactItem,
}

impl From<ContactRow> for ContactDto {
    fn from(r: ContactRow) -> Self {
        let item = r.item;
        Self {
            contact_pk: r.contact_pk,
            folder_pk: r.folder_pk,
            name: item.list_name().unwrap_or_else(|| "Без имени".to_string()),
            email: item.primary_email(),
            is_group: item.is_group(),
            birthday_ms: item.birthday.map(|t| t.as_millisecond()),
            modified_ms: item.modified_at.map(|t| t.as_millisecond()),
            item,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContactFolderDto {
    folder_pk: i64,
    name: String,
    distinguished: Option<String>,
    /// Контактов в базе и, по счётчику сервера, в папке.
    contacts: i64,
    total_count: i64,
    last_synced_ms: Option<i64>,
    sync_complete: bool,
}

impl From<ContactFolderInfo> for ContactFolderDto {
    fn from(f: ContactFolderInfo) -> Self {
        Self { folder_pk: f.folder_pk, name: f.name, distinguished: f.distinguished, contacts: f.contacts, total_count: f.total_count, last_synced_ms: f.last_synced_at, sync_complete: f.sync_complete }
    }
}

/// Состояние раздела: сколько контактов в базе и какие папки контактов есть (пусто — папок класса
/// `IPF.Contact` в дереве нет или дерево ещё не загружено).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContactsStatusDto {
    total: i64,
    folders: Vec<ContactFolderDto>,
}

/// Контакты и группы по словам запроса (пустой — все), группы после контактов; предел — на
/// каждый вид отдельно.
#[tauri::command]
pub(crate) async fn contacts_list(query: Option<String>, limit: Option<usize>, state: State<'_, AppState>) -> CmdResult<Vec<ContactDto>> {
    let store = state.store.clone();
    let account = state.account;
    let limit = limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
    let query = query.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || Ok(store.contacts_list(account, &query, limit).map_err(err)?.into_iter().map(ContactDto::from).collect())).await.map_err(err)?
}

/// Личный контакт с таким адресом (для карточки контакта: телефоны и заметки из своих контактов).
#[tauri::command]
pub(crate) async fn contact_personal(email: String, state: State<'_, AppState>) -> CmdResult<Option<ContactDto>> {
    let store = state.store.clone();
    let account = state.account;
    tauri::async_runtime::spawn_blocking(move || Ok(store.contact_by_email(account, &email).map_err(err)?.map(ContactDto::from))).await.map_err(err)?
}

#[tauri::command]
pub(crate) async fn contacts_status(state: State<'_, AppState>) -> CmdResult<ContactsStatusDto> {
    let store = state.store.clone();
    let account = state.account;
    tauri::async_runtime::spawn_blocking(move || {
        let total = store.contacts_count(account).map_err(err)?;
        let folders = store.contact_folder_infos(account).map_err(err)?.into_iter().map(ContactFolderDto::from).collect();
        Ok(ContactsStatusDto { total, folders })
    })
    .await
    .map_err(err)?
}
