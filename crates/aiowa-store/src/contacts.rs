//! Личные контакты ящика (docs/obkatka.md п. 8): папки класса ровно `IPF.Contact` — «Контакты» и
//! созданные пользователем; служебные `IPF.Contact.*` (кэш получателей, контакты Skype,
//! справочника) не трогаются. Таблица `contacts` хранит разобранный контакт (`ContactItem`) как
//! JSON и колонки для списка, поиска и подсказок адресатов; состояние синхронизации папки — в
//! `folders.contact_sync_state`, отдельно от почтового `sync_state`. Полная выгрузка папки (без
//! состояния) по концу убирает контакты, которых сервер не вернул (`stale`). Запись на сервер
//! (создание, правка) — отдельный этап.
//! Персональные данные в журнал не пишутся: только счётчики.

use crate::{now_ms, text, word_prefix_match, Result, Store, StoreError, SyncFolder};
use aiowa_core::{AccountId, ContactChange, ContactItem, SyncState};
use rusqlite::{params, OptionalExtension};
use std::sync::atomic::Ordering;

/// Итог применения пачки изменений контактов.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContactApplyStats {
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
}

/// Контакт из базы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactRow {
    pub contact_pk: i64,
    pub folder_pk: i64,
    pub item: ContactItem,
}

/// Папка контактов для раздела «Люди»: сколько в ней контактов в базе и на сервере, когда
/// синхронизирована.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactFolderInfo {
    pub folder_pk: i64,
    pub name: String,
    pub distinguished: Option<String>,
    pub contacts: i64,
    pub total_count: i64,
    pub last_synced_at: Option<i64>,
    pub sync_complete: bool,
}

/// Личный контакт для подсказок адресатов: имя и адрес (по записи на каждый адрес контакта).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactRecipient {
    pub name: Option<String>,
    /// В нижнем регистре.
    pub email: String,
}

const CONTACT_SELECT: &str = "SELECT contact_pk, folder_pk, json, search_text FROM contacts";
/// Контакты папок, которых на сервере больше нет, в списки не идут (строки остаются до следующей
/// синхронизации папки, если она вернётся).
const LIVE_FOLDER: &str = "folder_pk IN (SELECT folder_pk FROM folders WHERE deleted = 0)";

impl Store {
    /// Папки личных контактов: класс ровно `IPF.Contact`, не удалённые; стандартная «Контакты»
    /// (`contacts`) первой.
    pub fn contact_folders(&self, account: AccountId) -> Result<Vec<SyncFolder>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT folder_pk, server_id, display_name, distinguished, total_count FROM folders
                  WHERE account_id = ?1 AND deleted = 0 AND folder_class = 'IPF.Contact'
                  ORDER BY CASE WHEN distinguished = 'contacts' THEN 0 ELSE 1 END, folder_pk",
            )?;
            let rows = st
                .query_map(params![account.0], |r| Ok(SyncFolder { folder_pk: r.get(0)?, server_id: r.get(1)?, name: r.get(2)?, distinguished: r.get(3)?, total_count: r.get(4)? }))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Состояние синхронизации папки контактов (`None` — нужна полная выгрузка).
    pub fn contact_sync_state(&self, folder_pk: i64) -> Result<Option<SyncState>> {
        self.with_conn(|c| Ok(c.query_row("SELECT contact_sync_state FROM folders WHERE folder_pk = ?1", params![folder_pk], |r| r.get::<_, Option<String>>(0)).optional()?.flatten().map(SyncState)))
    }

    /// Начало полной выгрузки папки контактов (состояния нет или сервер его отверг): состояние
    /// сбрасывается, контакты папки помечаются неподтверждёнными. Выгрузка снимает пометку с
    /// каждого, кого вернёт сервер, а по её концу `apply_contact_batch` удаляет остальных. До конца
    /// выгрузки (и если она растянется на несколько прогонов) контакты видны как были.
    pub fn begin_contact_full_sync(&self, folder_pk: i64) -> Result<()> {
        self.with_tx(|tx| {
            tx.execute("UPDATE folders SET contact_sync_state = NULL, sync_complete = 0 WHERE folder_pk = ?1", params![folder_pk])?;
            tx.execute("UPDATE contacts SET stale = 1 WHERE folder_pk = ?1", params![folder_pk])?;
            Ok(())
        })
    }

    /// Применяет пачку изменений папки контактов и записывает её состояние синхронизации
    /// (`complete` — сервер сообщил, что изменений больше нет; тогда же удаляются контакты папки,
    /// которых полная выгрузка не подтвердила). Заметки в разметке сводятся к тексту.
    pub fn apply_contact_batch(&self, account: AccountId, folder_pk: i64, changes: &[ContactChange], state: &SyncState, complete: bool) -> Result<ContactApplyStats> {
        let now = now_ms();
        let stats = self.with_tx(|tx| {
            let mut stats = ContactApplyStats::default();
            for ch in changes {
                match ch {
                    ContactChange::Create(c) | ContactChange::Update(c) => {
                        let item = plain_notes(c);
                        let json = serde_json::to_string(&item).map_err(|e| StoreError::Other(format!("контакт не сериализован: {e}")))?;
                        let existed: Option<i64> = tx.query_row("SELECT contact_pk FROM contacts WHERE account_id = ?1 AND server_id = ?2", params![account.0, item.item_id.id], |r| r.get(0)).optional()?;
                        tx.execute(
                            "INSERT INTO contacts(account_id, folder_pk, server_id, change_key, item_class, is_group, display_name, email, company, job_title, sort_key, search_text, json, modified_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                             ON CONFLICT(account_id, server_id) DO UPDATE SET
                               folder_pk = excluded.folder_pk, change_key = excluded.change_key, item_class = excluded.item_class, is_group = excluded.is_group,
                               display_name = excluded.display_name, email = excluded.email, company = excluded.company, job_title = excluded.job_title,
                               sort_key = excluded.sort_key, search_text = excluded.search_text, json = excluded.json, modified_at = excluded.modified_at,
                               updated_at = excluded.updated_at, stale = 0",
                            params![
                                account.0,
                                folder_pk,
                                item.item_id.id,
                                item.item_id.change_key,
                                item.item_class,
                                item.is_group() as i64,
                                item.list_name(),
                                item.primary_email(),
                                item.company,
                                item.job_title,
                                sort_key(&item),
                                search_text(&item),
                                json,
                                item.modified_at.map(|t| t.as_millisecond()),
                                now
                            ],
                        )?;
                        if existed.is_some() {
                            stats.updated += 1;
                        } else {
                            stats.created += 1;
                        }
                    }
                    ContactChange::Delete(id) => {
                        stats.deleted += tx.execute("DELETE FROM contacts WHERE account_id = ?1 AND server_id = ?2", params![account.0, id.id])?;
                    }
                }
            }
            if complete {
                stats.deleted += tx.execute("DELETE FROM contacts WHERE folder_pk = ?1 AND stale = 1", params![folder_pk])?;
            }
            tx.execute("UPDATE folders SET contact_sync_state = ?2, sync_complete = ?3, last_synced_at = ?4 WHERE folder_pk = ?1", params![folder_pk, state.0, complete as i64, now])?;
            Ok(stats)
        })?;
        if stats.created + stats.updated + stats.deleted > 0 {
            // Подсказки адресатов берут личные контакты из кэша: он перестроится.
            self.write_generation.fetch_add(1, Ordering::Relaxed);
        }
        Ok(stats)
    }

    /// Контакты и группы ящика: группы после контактов, по имени списка; `query` — слова, каждое
    /// должно начинать слово имени, адреса, компании, должности или цифр телефона. `limit` — предел
    /// на каждый вид отдельно: группы не вытесняются контактами.
    pub fn contacts_list(&self, account: AccountId, query: &str, limit: usize) -> Result<Vec<ContactRow>> {
        let words: Vec<String> = text::norm_lower(query).split_whitespace().map(String::from).collect();
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT contact_pk, folder_pk, json, search_text, is_group FROM contacts WHERE account_id = ?1 AND {LIVE_FOLDER} ORDER BY is_group, sort_key, contact_pk"))?;
            let rows = st.query_map(params![account.0], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)? != 0)))?;
            let mut out = Vec::new();
            let (mut people, mut groups) = (0usize, 0usize);
            for r in rows {
                let (contact_pk, folder_pk, json, search, is_group) = r?;
                let taken = if is_group { &mut groups } else { &mut people };
                if *taken >= limit || !words.iter().all(|w| word_prefix_match(&search, w)) {
                    continue;
                }
                let Some(item) = row_item(&json) else { continue };
                out.push(ContactRow { contact_pk, folder_pk, item });
                *taken += 1;
                if groups >= limit {
                    break;
                }
            }
            Ok(out)
        })
    }

    /// Контакт по ключу базы.
    pub fn contact(&self, account: AccountId, contact_pk: i64) -> Result<Option<ContactRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("{CONTACT_SELECT} WHERE account_id = ?1 AND contact_pk = ?2"))?;
            let row = st.query_row(params![account.0, contact_pk], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))).optional()?;
            Ok(row.and_then(|(contact_pk, folder_pk, json)| row_item(&json).map(|item| ContactRow { contact_pk, folder_pk, item })))
        })
    }

    /// Личный контакт с таким адресом (любым из его адресов, без учёта регистра); группы — нет.
    pub fn contact_by_email(&self, account: AccountId, email: &str) -> Result<Option<ContactRow>> {
        let key = email.trim().to_lowercase();
        if key.is_empty() {
            return Ok(None);
        }
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!(
                "{CONTACT_SELECT} WHERE account_id = ?1 AND is_group = 0 AND {LIVE_FOLDER} AND (email = ?2 OR instr(search_text, ?2) > 0)
                  ORDER BY CASE WHEN email = ?2 THEN 0 ELSE 1 END, contact_pk"
            ))?;
            let rows = st.query_map(params![account.0, key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?;
            for r in rows {
                let (contact_pk, folder_pk, json) = r?;
                let Some(item) = row_item(&json) else { continue };
                if item.emails.iter().any(|e| e.email.trim().eq_ignore_ascii_case(&key)) {
                    return Ok(Some(ContactRow { contact_pk, folder_pk, item }));
                }
            }
            Ok(None)
        })
    }

    /// Папки контактов с числом контактов в базе (для строки состояния раздела «Люди»).
    pub fn contact_folder_infos(&self, account: AccountId) -> Result<Vec<ContactFolderInfo>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT f.folder_pk, f.display_name, f.distinguished, f.total_count, f.last_synced_at, f.sync_complete,
                        (SELECT count(*) FROM contacts c WHERE c.folder_pk = f.folder_pk)
                   FROM folders f
                  WHERE f.account_id = ?1 AND f.deleted = 0 AND f.folder_class = 'IPF.Contact'
                  ORDER BY CASE WHEN f.distinguished = 'contacts' THEN 0 ELSE 1 END, f.folder_pk",
            )?;
            let rows = st
                .query_map(params![account.0], |r| {
                    Ok(ContactFolderInfo {
                        folder_pk: r.get(0)?,
                        name: r.get(1)?,
                        distinguished: r.get(2)?,
                        total_count: r.get(3)?,
                        last_synced_at: r.get(4)?,
                        sync_complete: r.get::<_, i64>(5)? != 0,
                        contacts: r.get(6)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Сколько контактов и групп в базе (в живых папках).
    pub fn contacts_count(&self, account: AccountId) -> Result<i64> {
        self.with_conn(|c| Ok(c.query_row(&format!("SELECT count(*) FROM contacts WHERE account_id = ?1 AND {LIVE_FOLDER}"), params![account.0], |r| r.get(0))?))
    }

    /// Адреса личных контактов (без групп) для подсказок адресатов, по имени списка.
    pub fn contact_recipients(&self, account: AccountId) -> Result<Vec<ContactRecipient>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT json FROM contacts WHERE account_id = ?1 AND is_group = 0 AND {LIVE_FOLDER} ORDER BY sort_key, contact_pk"))?;
            let rows = st.query_map(params![account.0], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                let Some(item) = row_item(&r?) else { continue };
                let name = item.list_name();
                for e in &item.emails {
                    let email = e.email.trim().to_lowercase();
                    if email.split_once('@').is_some_and(|(l, d)| !l.is_empty() && !d.is_empty()) {
                        out.push(ContactRecipient { name: name.clone(), email });
                    }
                }
            }
            Ok(out)
        })
    }
}

/// Контакт с заметками текстом: разметку (`notes_is_html`) сводит `html_to_text`.
fn plain_notes(c: &ContactItem) -> ContactItem {
    let mut c = c.clone();
    if c.notes_is_html {
        c.notes = c.notes.as_deref().map(text::html_to_text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        c.notes_is_html = false;
    }
    c
}

fn sort_key(c: &ContactItem) -> String {
    text::norm_lower(&c.list_name().unwrap_or_default())
}

/// Текст для поиска: имя, адреса, компания, должность, отдел, прозвище, телефоны (как есть и
/// только цифры), участники группы — через ` | `, в нижнем регистре.
fn search_text(c: &ContactItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |s: &Option<String>| {
        if let Some(v) = s.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            parts.push(v.to_string());
        }
    };
    push(&c.display_name);
    push(&c.file_as);
    push(&c.given_name);
    push(&c.surname);
    push(&c.middle_name);
    push(&c.nickname);
    push(&c.company);
    push(&c.job_title);
    push(&c.department);
    for m in &c.members {
        push(&m.name);
        push(&m.email);
    }
    for e in &c.emails {
        parts.push(e.email.clone());
        if let Some(n) = &e.name {
            parts.push(n.clone());
        }
    }
    for p in &c.phones {
        parts.push(p.number.clone());
        let digits: String = p.number.chars().filter(|ch| ch.is_ascii_digit()).collect();
        if !digits.is_empty() {
            parts.push(digits);
        }
    }
    text::norm_lower(&parts.join(" | "))
}

fn row_item(json: &str) -> Option<ContactItem> {
    serde_json::from_str(json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{ContactEmail, ContactPhone, Folder, FolderId, ItemId, Participant};

    fn store_with_contacts_folder(tag: &str) -> (Store, AccountId, i64) {
        let path = std::env::temp_dir().join(format!("aiowa-contacts-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let account = store.account("h", "l").unwrap();
        let folder = Folder {
            id: FolderId { id: "F-C".into(), change_key: None },
            parent_id: None,
            display_name: "Контакты".into(),
            distinguished: Some("contacts".into()),
            folder_class: Some("IPF.Contact".into()),
            total_count: 2,
            unread_count: 0,
            child_folder_count: 0,
        };
        let cache = Folder { id: FolderId { id: "F-R".into(), change_key: None }, distinguished: None, display_name: "Recipient Cache".into(), folder_class: Some("IPF.Contact.RecipientCache".into()), ..folder.clone() };
        store.upsert_folders(account, &[folder, cache]).unwrap();
        let folders = store.contact_folders(account).unwrap();
        assert_eq!(folders.len(), 1, "служебная папка IPF.Contact.RecipientCache — не папка контактов");
        (store, account, folders[0].folder_pk)
    }

    fn contact(id: &str, name: &str, email: &str) -> ContactItem {
        let mut c = ContactItem::new(ItemId { id: id.into(), change_key: Some("ck".into()) });
        c.item_class = Some("IPM.Contact".into());
        c.display_name = Some(name.into());
        c.emails = vec![ContactEmail { key: Some("EmailAddress1".into()), email: email.into(), name: Some(name.into()) }];
        c.phones = vec![ContactPhone { key: Some("MobilePhone".into()), number: "+7 (916) 123-45-67".into() }];
        c
    }

    #[test]
    fn apply_list_search_and_delete() {
        let (store, account, folder_pk) = store_with_contacts_folder("apply");
        let mut html = contact("C2", "Пётр Сидоров", "P.Sidorov@Example.com");
        html.notes = Some("<p>Заметка <b>жирная</b></p>".into());
        html.notes_is_html = true;
        let mut group = ContactItem::new(ItemId { id: "G1".into(), change_key: None });
        group.item_class = Some("IPM.DistList".into());
        group.display_name = Some("Семья".into());
        group.members = vec![Participant { name: Some("Анна".into()), email: Some("anna@example.com".into()), routing_type: Some("SMTP".into()) }];
        let stats = store
            .apply_contact_batch(account, folder_pk, &[ContactChange::Create(contact("C1", "Иван Петров", "ivan@example.com")), ContactChange::Create(html), ContactChange::Create(group)], &SyncState("s1".into()), true)
            .unwrap();
        assert_eq!(stats, ContactApplyStats { created: 3, updated: 0, deleted: 0 });
        assert_eq!(store.contacts_count(account).unwrap(), 3);

        let all = store.contacts_list(account, "", 100).unwrap();
        assert_eq!(all.iter().map(|r| r.item.display_name.clone().unwrap()).collect::<Vec<_>>(), ["Иван Петров", "Пётр Сидоров", "Семья"], "группы после контактов, контакты по имени");
        assert_eq!(all[1].item.notes.as_deref(), Some("Заметка жирная"), "заметки в разметке сведены к тексту");
        assert!(!all[1].item.notes_is_html);
        assert!(all[2].item.is_group());

        let by_word = store.contacts_list(account, "сидор", 10).unwrap();
        assert_eq!(by_word.len(), 1);
        assert_eq!(by_word[0].contact_pk, all[1].contact_pk);
        assert_eq!(store.contacts_list(account, "петр сид", 10).unwrap().len(), 1, "все слова запроса; ё и е равны");
        assert_eq!(store.contacts_list(account, "916", 10).unwrap().len(), 2, "по части номера в скобках");
        assert_eq!(store.contacts_list(account, "7916", 10).unwrap().len(), 2, "по цифрам телефона без разделителей");
        assert_eq!(store.contacts_list(account, "example", 10).unwrap().len(), 3, "по домену адреса и участнику группы");
        assert_eq!(store.contacts_list(account, "anna", 10).unwrap().len(), 1, "участник группы ищется");

        let found = store.contact_by_email(account, " p.sidorov@example.COM ").unwrap().expect("по адресу без учёта регистра");
        assert_eq!(found.item.item_id.id, "C2");
        assert!(store.contact_by_email(account, "anna@example.com").unwrap().is_none(), "адрес участника группы — не контакт");
        assert_eq!(store.contact(account, found.contact_pk).unwrap().unwrap().item, found.item);

        let recipients = store.contact_recipients(account).unwrap();
        assert_eq!(recipients.iter().map(|r| r.email.as_str()).collect::<Vec<_>>(), ["ivan@example.com", "p.sidorov@example.com"], "группы в подсказки не идут, адреса в нижнем регистре");

        let infos = store.contact_folder_infos(account).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!((infos[0].contacts, infos[0].sync_complete), (3, true));
        assert_eq!(store.contact_sync_state(folder_pk).unwrap(), Some(SyncState("s1".into())));

        let mut renamed = contact("C1", "Иван Петров-Водкин", "ivan@example.com");
        renamed.item_id.change_key = Some("ck2".into());
        let stats = store.apply_contact_batch(account, folder_pk, &[ContactChange::Update(renamed), ContactChange::Delete(ItemId { id: "G1".into(), change_key: None })], &SyncState("s2".into()), true).unwrap();
        assert_eq!(stats, ContactApplyStats { created: 0, updated: 1, deleted: 1 });
        let all = store.contacts_list(account, "", 100).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].item.display_name.as_deref(), Some("Иван Петров-Водкин"));
        assert_eq!(all[0].item.item_id.change_key.as_deref(), Some("ck2"));
    }

    #[test]
    fn personal_contacts_come_first_in_recipient_suggestions() {
        let (store, account, folder_pk) = store_with_contacts_folder("suggest");
        store.apply_contact_batch(account, folder_pk, &[ContactChange::Create(contact("C1", "Зоя Личная", "zoya@example.com"))], &SyncState("s1".into()), true).unwrap();
        let got = store.suggest_recipients(account, "зо", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].name.as_deref(), got[0].email.as_str()), (Some("Зоя Личная"), "zoya@example.com"));
        assert!(got[0].weight >= crate::CONTACT_WEIGHT);
        assert!(store.suggest_recipients(account, "нет", 10).unwrap().is_empty());
    }

    #[test]
    fn contact_sync_state_is_separate_from_mail_sync_state() {
        // Курсор, сдвинутый синхронизацией писем по этой папке, контакты не продолжают, и наоборот.
        let (store, account, folder_pk) = store_with_contacts_folder("cursor");
        store.set_folder_sync_state(folder_pk, Some(&SyncState("mail".into())), true).unwrap();
        assert_eq!(store.contact_sync_state(folder_pk).unwrap(), None, "почтовый курсор — не курсор контактов");
        store.apply_contact_batch(account, folder_pk, &[ContactChange::Create(contact("C1", "Иван Петров", "ivan@example.com"))], &SyncState("contacts".into()), true).unwrap();
        assert_eq!(store.contact_sync_state(folder_pk).unwrap(), Some(SyncState("contacts".into())));
        assert_eq!(store.folder_sync_state(folder_pk).unwrap(), Some(SyncState("mail".into())), "почтовый курсор не тронут");
    }

    #[test]
    fn full_resync_removes_contacts_the_server_did_not_return() {
        let (store, account, folder_pk) = store_with_contacts_folder("resync");
        let other = Folder {
            id: FolderId { id: "F-C2".into(), change_key: None },
            parent_id: None,
            display_name: "Работа".into(),
            distinguished: None,
            folder_class: Some("IPF.Contact".into()),
            total_count: 1,
            unread_count: 0,
            child_folder_count: 0,
        };
        store.upsert_folders(account, &[other]).unwrap();
        let other_pk = store.contact_folders(account).unwrap().iter().find(|f| f.folder_pk != folder_pk).unwrap().folder_pk;
        let all = |s: &Store| s.contacts_list(account, "", 100).unwrap().into_iter().map(|r| r.item.item_id.id).collect::<Vec<_>>();
        store
            .apply_contact_batch(
                account,
                folder_pk,
                &[ContactChange::Create(contact("C1", "Анна", "a@example.com")), ContactChange::Create(contact("C2", "Борис", "b@example.com")), ContactChange::Create(contact("C3", "Вера", "v@example.com"))],
                &SyncState("s1".into()),
                true,
            )
            .unwrap();
        store.apply_contact_batch(account, other_pk, &[ContactChange::Create(contact("D1", "Глеб", "g@example.com"))], &SyncState("o1".into()), true).unwrap();

        // Сервер отверг состояние: полная выгрузка; Бориса на сервере уже нет, Delete о нём не придёт.
        store.begin_contact_full_sync(folder_pk).unwrap();
        assert_eq!(store.contact_sync_state(folder_pk).unwrap(), None);
        assert_eq!(all(&store), ["C1", "C2", "C3", "D1"], "до конца выгрузки контакты видны как были");
        let stats = store.apply_contact_batch(account, folder_pk, &[ContactChange::Create(contact("C1", "Анна", "a@example.com"))], &SyncState("p1".into()), false).unwrap();
        assert_eq!(stats, ContactApplyStats { created: 0, updated: 1, deleted: 0 });
        assert_eq!(all(&store).len(), 4, "выгрузка не закончена — никто не удалён");
        let stats = store.apply_contact_batch(account, folder_pk, &[ContactChange::Create(contact("C3", "Вера", "v@example.com"))], &SyncState("p2".into()), true).unwrap();
        assert_eq!(stats, ContactApplyStats { created: 0, updated: 1, deleted: 1 });
        assert_eq!(all(&store), ["C1", "C3", "D1"], "неподтверждённый контакт удалён, другая папка не тронута");
        assert!(store.contact_by_email(account, "b@example.com").unwrap().is_none());
        assert!(store.contact_recipients(account).unwrap().iter().all(|r| r.email != "b@example.com"), "и из подсказок адресатов");

        // Обычная инкрементальная пачка с признаком конца никого не удаляет.
        let stats = store.apply_contact_batch(account, folder_pk, &[], &SyncState("p3".into()), true).unwrap();
        assert_eq!(stats, ContactApplyStats::default());
        assert_eq!(all(&store).len(), 3);
    }

    #[test]
    fn list_limit_applies_to_contacts_and_groups_separately() {
        // Контактов больше предела: группы всё равно в списке, хвост контактов — нет.
        let (store, account, folder_pk) = store_with_contacts_folder("limit");
        let mut group = ContactItem::new(ItemId { id: "G1".into(), change_key: None });
        group.item_class = Some("IPM.DistList".into());
        group.display_name = Some("Альфа-группа".into());
        store
            .apply_contact_batch(
                account,
                folder_pk,
                &[ContactChange::Create(contact("C1", "Анна", "a@example.com")), ContactChange::Create(contact("C2", "Борис", "b@example.com")), ContactChange::Create(contact("C3", "Вера", "v@example.com")), ContactChange::Create(group)],
                &SyncState("s1".into()),
                true,
            )
            .unwrap();
        let got = store.contacts_list(account, "", 2).unwrap();
        assert_eq!(got.iter().map(|r| r.item.item_id.id.as_str()).collect::<Vec<_>>(), ["C1", "C2", "G1"]);
        assert_eq!(store.contacts_list(account, "", 100).unwrap().len(), 4);
    }
}
