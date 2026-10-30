//! Раздел «Люди» (docs/obkatka.md п. 8): личные контакты ящика из локальной базы — список с
//! поиском, контакт по адресу для карточки и состояние папок контактов. Контакты приходят
//! синхронизацией вместе с почтой (`sync::SyncState`), о новых окну сообщает событие
//! `contacts-changed`. Запись — создание, правка, удаление — операциями персон браузерной OWA
//! (`contact_save`, `contact_delete`, docs/protocol-owa-json.md § 5r). В журнал имена и адреса не
//! пишутся.

use crate::ondemand::{describe_transport, timed_out};
use crate::sync::SyncState;
use crate::write::WriteState;
use crate::{err, AppState, CmdResult};
use aiowa_core::{ContactItem, FolderId, ItemId};
use aiowa_store::{ContactFolderInfo, ContactRow};
use aiowa_transport_owa::protocol::{self, persona_update, PERSONA_PERSON};
use aiowa_transport_owa::OwaTransport;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use tracing::{info, warn};

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

// --- запись: создание, правка, удаление (операции персон, docs/protocol-owa-json.md § 5r) ----

/// Таймаут одной записи контакта на сервер (вместе с перечитыванием).
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Персон в одной странице `FindPeople` по папке и самое большее страниц при поиске персоны.
const PERSONA_PAGE: usize = 200;
const PERSONA_PAGES: usize = 25;

/// Форма контакта из окна: пустая строка — поле не заполнено. Заметок нет: персонами они не
/// пишутся (`PersonaNotes` — HTTP 500, § 5r).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct ContactFormDto {
    #[serde(default)]
    given_name: String,
    #[serde(default)]
    surname: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    business_phone: String,
    #[serde(default)]
    mobile_phone: String,
    #[serde(default)]
    home_phone: String,
    #[serde(default)]
    company: String,
    #[serde(default)]
    job_title: String,
    #[serde(default)]
    department: String,
    #[serde(default)]
    office: String,
}

/// Поле формы ↔ поле персоны: значение в форме и текущее значение контакта.
type FieldGet = fn(&ContactFormDto) -> &str;
type ItemGet = fn(&ContactItem) -> Option<String>;

fn phone(c: &ContactItem, key: &str) -> Option<String> {
    c.phones.iter().find(|p| p.key.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(key))).map(|p| p.number.clone())
}

/// Адрес контакта, который правится полем «Почта»: `EmailAddress1`, иначе первый.
fn email1(c: &ContactItem) -> Option<String> {
    c.emails.iter().find(|e| e.key.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("EmailAddress1"))).or_else(|| c.emails.first()).map(|e| e.email.clone())
}

/// Поля персоны, кроме адреса (у него свои три поля): проверены пробой `aiowa contact-check`.
const PERSONA_FIELDS: &[(&str, FieldGet, ItemGet)] = &[
    ("PersonaGivenNames", |f| &f.given_name, |c| c.given_name.clone()),
    ("PersonaSurnames", |f| &f.surname, |c| c.surname.clone()),
    ("PersonaBusinessPhoneNumbers", |f| &f.business_phone, |c| phone(c, "BusinessPhone")),
    ("PersonaMobilePhones", |f| &f.mobile_phone, |c| phone(c, "MobilePhone")),
    ("PersonaHomePhones", |f| &f.home_phone, |c| phone(c, "HomePhone")),
    ("PersonaCompanyNames", |f| &f.company, |c| c.company.clone()),
    ("PersonaTitles", |f| &f.job_title, |c| c.job_title.clone()),
    ("PersonaDepartments", |f| &f.department, |c| c.department.clone()),
    ("PersonaOfficeLocations", |f| &f.office, |c| c.office.clone()),
];

/// Проверка формы: имя, фамилия, компания или адрес — хоть что-то; адрес — похожий на почтовый.
pub(crate) fn check_form(f: &ContactFormDto) -> Result<(), String> {
    let email = f.email.trim();
    if [&f.given_name, &f.surname, &f.company].iter().all(|s| s.trim().is_empty()) && email.is_empty() {
        return Err("Укажите имя, фамилию, компанию или адрес".into());
    }
    if !email.is_empty() && !crate::compose::looks_like_email(email) {
        return Err("Адрес почты не похож на адрес".into());
    }
    Ok(())
}

/// Изменения полей персоны (`PersonaPropertyUpdate`) для формы `form`: у нового контакта (`old`
/// нет) — заполненные поля с `OldValue: ""`, у существующего — только изменившиеся, с прежним
/// значением с сервера; очищенное поле — `NewValue: ""`. Адрес — тремя полями, как у OWA.
pub(crate) fn persona_updates(old: Option<&ContactItem>, form: &ContactFormDto) -> Vec<serde_json::Value> {
    let clean = |s: Option<String>| s.map(|s| s.trim().to_string()).unwrap_or_default();
    let mut out = Vec::new();
    for (field, get, cur) in PERSONA_FIELDS {
        let new = get(form).trim();
        let was = clean(old.and_then(cur));
        if new != was && !(old.is_none() && new.is_empty()) {
            out.push(persona_update(field, Some(&was), Some(new)));
        }
    }
    let new = form.email.trim();
    match old {
        None if !new.is_empty() => {
            out.push(persona_update("PersonaEmails1", None, Some(new)));
            out.push(persona_update("PersonaEmails1OriginalDisplayNames", None, Some(new)));
            out.push(persona_update("PersonaEmails1DisplayNames", None, None));
        }
        Some(c) => {
            let was = clean(email1(c));
            if !new.eq_ignore_ascii_case(&was) {
                let was = (!was.is_empty()).then_some(was);
                out.push(persona_update("PersonaEmails1", was.as_deref(), Some(new)));
                out.push(persona_update("PersonaEmails1OriginalDisplayNames", was.as_deref(), Some(new)));
            }
        }
        None => {}
    }
    out
}

/// Персона контакта `item_id` в папке: `FindPeople` по папке со свойством `PersonaAttributions`
/// постранично, совпадение по `AttributionsArray[].SourceId.Id`.
async fn persona_of(t: &OwaTransport, folder: &FolderId, item_id: &ItemId) -> aiowa_transport::Result<Option<String>> {
    for page in 0..PERSONA_PAGES {
        let v = t.call("FindPeople", protocol::V_2017, protocol::find_folder_personas_body(folder, page * PERSONA_PAGE, PERSONA_PAGE)).await?;
        let people = protocol::find_people_array(&v).cloned().unwrap_or_default();
        let hit = people.iter().find(|p| p.get("AttributionsArray").and_then(serde_json::Value::as_array).is_some_and(|a| a.iter().any(|x| x.pointer("/SourceId/Id").and_then(serde_json::Value::as_str) == Some(item_id.id.as_str()))));
        if let Some(id) = hit.and_then(|p| p.pointer("/PersonaId/Id")).and_then(serde_json::Value::as_str) {
            return Ok(Some(id.to_string()));
        }
        if people.len() < PERSONA_PAGE {
            break;
        }
    }
    Ok(None)
}

/// Событие окну после записи из окна — то же, что после синхронизации.
fn emit_changed(app: &AppHandle, created: usize, updated: usize, deleted: usize) {
    #[derive(Clone, Serialize)]
    struct ContactsChanged {
        created: usize,
        updated: usize,
        deleted: usize,
    }
    if let Err(e) = app.emit("contacts-changed", ContactsChanged { created, updated, deleted }) {
        warn!(error = %e, "событие contacts-changed не отправлено");
    }
}

async fn timed<T>(fut: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::time::timeout(WRITE_TIMEOUT, fut).await.unwrap_or_else(|_| Err(timed_out(WRITE_TIMEOUT)))
}

/// Создать (`contact_pk` нет) или изменить личный контакт. Новый — в папку «Контакты». Перед
/// правкой поля перечитываются с сервера (прежние значения `UpdatePersona` — серверные), после
/// записи контакт перечитывается `GetItem` и ложится в базу. Возвращает контакт из базы.
#[tauri::command]
pub(crate) async fn contact_save(contact_pk: Option<i64>, form: ContactFormDto, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, ws: State<'_, WriteState>) -> CmdResult<ContactDto> {
    check_form(&form)?;
    let (store, account) = (state.store.clone(), state.account);
    let _lock = ws.lock().await;
    let existing = match contact_pk {
        Some(pk) => Some(store.contact(account, pk).map_err(err)?.ok_or("Контакта уже нет")?),
        None => None,
    };
    let (folder_pk, folder) = match &existing {
        Some(row) => {
            let f = store.folder_by_pk(account, row.folder_pk).map_err(err)?.ok_or("Папки контакта уже нет")?;
            (f.folder_pk, FolderId { id: f.server_id, change_key: None })
        }
        None => {
            let (pk, id) = store.folder_by_distinguished(account, "contacts").map_err(err)?.ok_or("Папки «Контакты» нет: дождитесь синхронизации")?;
            (pk, FolderId { id, change_key: None })
        }
    };
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let t = &transport;
    let started = Instant::now();
    let item_id = timed(async {
        let fail = |e: aiowa_transport::TransportError| describe_transport(&e);
        match &existing {
            None => {
                let v = t.persona_call("CreatePersona", protocol::create_persona_body(&folder, PERSONA_PERSON, persona_updates(None, &form))).await.map_err(fail)?;
                protocol::parse_persona_written(&v).and_then(|(_, id)| id).ok_or_else(|| "Сервер создал контакт, но не вернул его: он появится после синхронизации".to_string())
            }
            Some(row) => {
                let id = row.item.item_id.clone();
                let fresh = t.get_contact_items(std::slice::from_ref(&id)).await.map_err(fail)?.into_iter().next().flatten().ok_or("Контакта на сервере нет: он удалён в другом месте")?;
                let updates = persona_updates(Some(&fresh), &form);
                if updates.is_empty() {
                    return Ok(id);
                }
                let persona = persona_of(t, &folder, &id).await.map_err(fail)?.ok_or("Контакт не найден среди людей папки: правьте его в OWA")?;
                t.persona_call("UpdatePersona", protocol::update_persona_body(&persona, PERSONA_PERSON, updates)).await.map_err(fail)?;
                Ok(id)
            }
        }
    })
    .await?;
    let saved = timed(async { t.get_contact_items(std::slice::from_ref(&item_id)).await.map_err(|e| describe_transport(&e)) })
        .await
        .ok()
        .and_then(|v| v.into_iter().next().flatten());
    let Some(item) = saved else {
        // Запись прошла, а перечитать не вышло: контакт придёт синхронизацией.
        info!(created = existing.is_none(), ms = started.elapsed().as_millis() as u64, "контакт записан, перечитать не вышло");
        sync.inner().request(app.clone(), None);
        return Err("Контакт сохранён на сервере; в списке он появится после синхронизации".into());
    };
    let pk = {
        let store = store.clone();
        tauri::async_runtime::spawn_blocking(move || store.put_contact(account, folder_pk, &item)).await.map_err(err)?.map_err(err)?
    };
    info!(created = existing.is_none(), ms = started.elapsed().as_millis() as u64, "контакт записан");
    emit_changed(&app, usize::from(existing.is_none()), usize::from(existing.is_some()), 0);
    let row = store.contact(account, pk).map_err(err)?.ok_or("Контакт не записан в базу")?;
    Ok(ContactDto::from(row))
}

/// Удалить личный контакт: `DeletePersonas` (контакт уходит в «Удалённые»), строка — из базы сразу.
#[tauri::command]
pub(crate) async fn contact_delete(contact_pk: i64, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, ws: State<'_, WriteState>) -> CmdResult<()> {
    let (store, account) = (state.store.clone(), state.account);
    let _lock = ws.lock().await;
    let row = store.contact(account, contact_pk).map_err(err)?.ok_or("Контакта уже нет")?;
    let folder = store.folder_by_pk(account, row.folder_pk).map_err(err)?.ok_or("Папки контакта уже нет")?;
    let folder = FolderId { id: folder.server_id, change_key: None };
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let t = &transport;
    timed(async {
        let fail = |e: aiowa_transport::TransportError| describe_transport(&e);
        let persona = persona_of(t, &folder, &row.item.item_id).await.map_err(fail)?.ok_or("Контакт не найден среди людей папки: удалите его в OWA")?;
        t.delete_personas(&[persona]).await.map_err(fail)?;
        Ok(())
    })
    .await?;
    let removed = {
        let store = store.clone();
        tauri::async_runtime::spawn_blocking(move || store.remove_contact(account, contact_pk)).await.map_err(err)?.map_err(err)?
    };
    info!(removed, "контакт удалён");
    emit_changed(&app, 0, 0, 1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{ContactEmail, ContactPhone};

    fn form() -> ContactFormDto {
        ContactFormDto { given_name: " Анна ".into(), surname: "Смирнова".into(), email: "anna@example.com".into(), mobile_phone: "+7 916 000-00-00".into(), ..Default::default() }
    }

    fn fields(v: &[serde_json::Value]) -> Vec<(String, Option<String>, Option<String>)> {
        v.iter()
            .map(|u| {
                let s = |k: &str| u.get(k).and_then(serde_json::Value::as_str).map(String::from);
                (u.pointer("/Path/FieldURI").and_then(serde_json::Value::as_str).unwrap_or("").to_string(), s("OldValue"), s("NewValue"))
            })
            .collect()
    }

    #[test]
    fn new_contact_sends_filled_fields_and_email_as_owa() {
        let got = fields(&persona_updates(None, &form()));
        assert_eq!(
            got,
            vec![
                ("PersonaGivenNames".into(), Some(String::new()), Some("Анна".into())),
                ("PersonaSurnames".into(), Some(String::new()), Some("Смирнова".into())),
                ("PersonaMobilePhones".into(), Some(String::new()), Some("+7 916 000-00-00".into())),
                ("PersonaEmails1".into(), None, Some("anna@example.com".into())),
                ("PersonaEmails1OriginalDisplayNames".into(), None, Some("anna@example.com".into())),
                ("PersonaEmails1DisplayNames".into(), None, None),
            ]
        );
    }

    #[test]
    fn edit_sends_only_changes_with_server_values() {
        let mut c = ContactItem::new(ItemId { id: "C1".into(), change_key: None });
        c.given_name = Some("Анна".into());
        c.surname = Some("Смирнова".into());
        c.company = Some("Старая".into());
        c.emails = vec![ContactEmail { key: Some("EmailAddress1".into()), email: "Anna@Example.com".into(), name: None }];
        c.phones = vec![ContactPhone { key: Some("MobilePhone".into()), number: "+7 916 000-00-00".into() }, ContactPhone { key: Some("BusinessPhone".into()), number: "100".into() }];
        let mut f = form();
        f.business_phone = String::new();
        f.company = "Новая".into();
        let got = fields(&persona_updates(Some(&c), &f));
        assert_eq!(
            got,
            vec![
                ("PersonaBusinessPhoneNumbers".into(), Some("100".into()), Some(String::new())),
                ("PersonaCompanyNames".into(), Some("Старая".into()), Some("Новая".into())),
            ],
            "адрес отличается только регистром — не меняется; очищенный телефон — пустым значением"
        );
        f.email = "anna.s@example.com".into();
        let got = fields(&persona_updates(Some(&c), &f));
        assert!(got.contains(&("PersonaEmails1".into(), Some("Anna@Example.com".into()), Some("anna.s@example.com".into()))));
        assert!(got.contains(&("PersonaEmails1OriginalDisplayNames".into(), Some("Anna@Example.com".into()), Some("anna.s@example.com".into()))));
        assert!(persona_updates(Some(&c), &ContactFormDto { business_phone: "100".into(), company: "Старая".into(), ..form() }).is_empty(), "без изменений — ничего");
    }

    #[test]
    fn form_needs_a_name_and_a_sane_email() {
        assert!(check_form(&form()).is_ok());
        assert!(check_form(&ContactFormDto { company: "Компания".into(), ..Default::default() }).is_ok());
        assert!(check_form(&ContactFormDto::default()).is_err());
        assert_eq!(check_form(&ContactFormDto { given_name: "А".into(), email: "не адрес".into(), ..Default::default() }), Err("Адрес почты не похож на адрес".into()));
    }
}
