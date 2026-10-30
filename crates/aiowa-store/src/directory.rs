//! Кэш справочника организации (`directory_people`, схема версии 5): записи `FindPeople`,
//! `ResolveNames` и `ExpandDL` по SMTP-адресу в нижнем регистре. Сервер — источник истины; кэш
//! даёт мгновенные подсказки и карточку контакта без сети. Здесь же счётчики переписки с адресом
//! по локальной базе писем (`contact_local_stats`).

use crate::{now_ms, text, word_prefix_match, Result, Store};
use aiowa_core::{AccountId, DirectoryPerson, PersonKind};
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::{Map, Value};
use tracing::debug;

/// Переписка с адресом по локальной базе: письма от него (`from`), письма владельца ящика ему
/// (получатель «Кому», «Копия» или «Скрытая копия» в письме из «Отправленных» или от собственного
/// адреса) и момент самого свежего из них, миллисекунды UTC. Получатели известны только у писем с
/// загруженным телом, поэтому `messages_to` — нижняя оценка.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContactLocalStats {
    pub messages_from: i64,
    pub messages_to: i64,
    pub last_ms: Option<i64>,
}

/// Поля записи, кроме адреса, имени и вида, — объект колонки `json` (пустые не пишутся).
fn extra_json(p: &DirectoryPerson) -> String {
    let mut m = Map::new();
    for (key, value) in [
        ("title", &p.title),
        ("department", &p.department),
        ("company", &p.company),
        ("office", &p.office),
        ("business_phone", &p.business_phone),
        ("mobile_phone", &p.mobile_phone),
        ("alias", &p.alias),
        ("address", &p.address),
        ("notes", &p.notes),
    ] {
        if let Some(v) = value {
            m.insert(key.to_string(), Value::String(v.clone()));
        }
    }
    // Оргструктура: руководитель и подчинённые (ключ `direct_reports` есть только у записи с
    // полными данными — по нему карточка отличает старый кэш от «подчинённых нет»).
    if let Some(mgr) = &p.manager {
        m.insert("manager".into(), serde_json::to_value(mgr).unwrap_or(Value::Null));
    }
    if let Some(reports) = &p.direct_reports {
        m.insert("direct_reports".into(), serde_json::to_value(reports).unwrap_or(Value::Array(Vec::new())));
    }
    Value::Object(m).to_string()
}

/// Запись из строки таблицы; испорченный `json` даёт запись без дополнительных полей.
fn person_from_row(email: String, display_name: Option<String>, kind: &str, json: &str) -> DirectoryPerson {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let get = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    DirectoryPerson {
        display_name,
        email: Some(email),
        kind: PersonKind::from_key(kind),
        title: get("title"),
        department: get("department"),
        company: get("company"),
        office: get("office"),
        business_phone: get("business_phone"),
        mobile_phone: get("mobile_phone"),
        alias: get("alias"),
        manager: v.get("manager").and_then(|m| serde_json::from_value(m.clone()).ok()),
        direct_reports: v.get("direct_reports").and_then(|d| serde_json::from_value(d.clone()).ok()),
        address: get("address"),
        notes: get("notes"),
    }
}

fn select_person(tx: &Transaction, account: AccountId, key: &str) -> Result<Option<DirectoryPerson>> {
    let mut st = tx.prepare_cached("SELECT email, display_name, kind, json FROM directory_people WHERE account_id = ?1 AND email = ?2")?;
    Ok(st.query_row(params![account.0, key], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))).optional()?.map(|(e, n, k, j)| person_from_row(e, n, &k, &j)))
}

fn write_person(tx: &Transaction, account: AccountId, key: &str, p: &DirectoryPerson, now: i64) -> Result<()> {
    let mut st = tx.prepare_cached(
        "INSERT INTO directory_people(account_id, email, display_name, kind, json, fetched_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account_id, email) DO UPDATE SET display_name = excluded.display_name, kind = excluded.kind, json = excluded.json, fetched_at = excluded.fetched_at",
    )?;
    st.execute(params![account.0, key, p.display_name, p.kind.key(), extra_json(p), now])?;
    Ok(())
}

/// Ранг совпадения для порядка выдачи: 0 — имя начинается с запроса, 1 — с запроса начинается
/// другое слово имени, 2 — совпал только адрес; `None` — не совпало.
fn match_rank(name_norm: &str, email: &str, needle: &str) -> Option<u8> {
    if name_norm.starts_with(needle) {
        Some(0)
    } else if word_prefix_match(name_norm, needle) {
        Some(1)
    } else if word_prefix_match(email, needle) {
        Some(2)
    } else {
        None
    }
}

/// Нашёл бы поиск по кэшу (`Store::directory_search_cached`) эту запись по запросу: начало слова
/// имени или адреса без учёта регистра и ё. Запись без SMTP-адреса в кэш не попадает — `false`.
pub fn directory_person_matches(person: &DirectoryPerson, query: &str) -> bool {
    let needle = text::norm_lower(query.trim());
    let Some(email) = person.email_key() else { return false };
    !needle.is_empty() && match_rank(&text::norm_lower(person.display_name.as_deref().unwrap_or_default()), &email, &needle).is_some()
}

impl Store {
    /// Записывает записи справочника в кэш и возвращает, сколько записано. Ключ — адрес в нижнем
    /// регистре (`DirectoryPerson::email_key`); записи без SMTP-адреса (устаревший `EX`) пропускаются.
    /// Существующая запись обновляется слиянием (`DirectoryPerson::merged_with`): заполненные поля
    /// новой побеждают, пустые берутся из старой — так выдача `FindPeople` без телефонов не стирает
    /// телефоны, полученные `ResolveNames`. Полная замена — `replace_directory_person`.
    pub fn upsert_directory_people(&self, account: AccountId, people: &[DirectoryPerson]) -> Result<usize> {
        if people.is_empty() {
            return Ok(0);
        }
        let now = now_ms();
        let written = self.with_tx(|tx| {
            let mut n = 0;
            for p in people {
                let Some(key) = p.email_key() else { continue };
                let merged = match select_person(tx, account, &key)? {
                    Some(old) => p.clone().merged_with(&old),
                    None => p.clone(),
                };
                write_person(tx, account, &key, &merged, now)?;
                n += 1;
            }
            Ok(n)
        })?;
        debug!(given = people.len(), written, "кэш справочника обновлён");
        Ok(written)
    }

    /// Заменяет запись справочника целиком (без слияния со старой): для полных данных контакта
    /// (`ResolveNames` с `ReturnFullContactData`), где отсутствие поля значит, что его больше нет.
    /// Без SMTP-адреса — `false`, ничего не записано.
    pub fn replace_directory_person(&self, account: AccountId, person: &DirectoryPerson) -> Result<bool> {
        let Some(key) = person.email_key() else { return Ok(false) };
        let now = now_ms();
        self.with_tx(|tx| write_person(tx, account, &key, person, now))?;
        Ok(true)
    }

    /// Запись справочника из кэша по адресу (без учёта регистра и пробелов по краям).
    pub fn directory_person(&self, account: AccountId, email: &str) -> Result<Option<DirectoryPerson>> {
        let key = email.trim().to_lowercase();
        if key.is_empty() {
            return Ok(None);
        }
        self.with_conn(|c| {
            let mut st = c.prepare_cached("SELECT email, display_name, kind, json FROM directory_people WHERE account_id = ?1 AND email = ?2")?;
            Ok(st.query_row(params![account.0, key], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))).optional()?.map(|(e, n, k, j)| person_from_row(e, n, &k, &j)))
        })
    }

    /// Записать фото записи справочника (или отметку «фото нет»: `None` с временем запроса).
    /// Записи с таким адресом нет — `false`, ничего не записано.
    pub fn set_directory_photo(&self, account: AccountId, email: &str, photo: Option<&[u8]>) -> Result<bool> {
        let key = email.trim().to_lowercase();
        let now = now_ms();
        self.with_conn(|c| Ok(c.execute("UPDATE directory_people SET photo = ?3, photo_at = ?4 WHERE account_id = ?1 AND email = ?2", params![account.0, key, photo, now])? > 0))
    }

    /// Фото записи из кэша: `Some((фото или None, когда запрашивалось))`, если фото запрашивалось;
    /// запись есть, но фото не запрашивалось, или записи нет — `None`.
    pub fn directory_photo(&self, account: AccountId, email: &str) -> Result<Option<(Option<Vec<u8>>, i64)>> {
        let key = email.trim().to_lowercase();
        self.with_conn(|c| {
            Ok(c.query_row("SELECT photo, photo_at FROM directory_people WHERE account_id = ?1 AND email = ?2 AND photo_at IS NOT NULL", params![account.0, key], |r| Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, i64>(1)?))).optional()?)
        })
    }

    /// Фото из кэша по списку адресов (только те, у кого фото есть): адрес в нижнем регистре → байты.
    pub fn directory_photos(&self, account: AccountId, emails: &[String]) -> Result<Vec<(String, Vec<u8>)>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached("SELECT photo FROM directory_people WHERE account_id = ?1 AND email = ?2 AND photo IS NOT NULL")?;
            let mut out = Vec::new();
            for e in emails {
                let key = e.trim().to_lowercase();
                if key.is_empty() {
                    continue;
                }
                if let Some(bytes) = st.query_row(params![account.0, key], |r| r.get::<_, Vec<u8>>(0)).optional()? {
                    out.push((key, bytes));
                }
            }
            Ok(out)
        })
    }

    /// Момент последней записи адреса в кэш справочника (миллисекунды UTC), если запись есть.
    pub fn directory_person_fetched_at(&self, account: AccountId, email: &str) -> Result<Option<i64>> {
        let key = email.trim().to_lowercase();
        self.with_conn(|c| Ok(c.query_row("SELECT fetched_at FROM directory_people WHERE account_id = ?1 AND email = ?2", params![account.0, key], |r| r.get::<_, i64>(0)).optional()?))
    }

    /// Поиск в кэше справочника по началу слов имени или адреса (без учёта регистра и ё), до
    /// `limit` записей: сначала имена, начинающиеся с запроса, затем совпадения по другому слову
    /// имени, затем по адресу; внутри — по имени. Пустой запрос — пусто.
    pub fn directory_search_cached(&self, account: AccountId, prefix: &str, limit: usize) -> Result<Vec<DirectoryPerson>> {
        let needle = text::norm_lower(prefix.trim());
        if needle.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // Грубый отбор подстрокой в SQL (`norm_lower` зарегистрирована на соединении), точный —
        // по началу слов в Rust.
        let rows = self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT email, display_name, kind, json, norm_lower(COALESCE(display_name, '')) FROM directory_people
                  WHERE account_id = ?1 AND (instr(norm_lower(COALESCE(display_name, '')), ?2) > 0 OR instr(email, ?2) > 0)",
            )?;
            let rows = st
                .query_map(params![account.0, needle], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut hits: Vec<(u8, String, DirectoryPerson)> = rows
            .into_iter()
            .filter_map(|(email, name, kind, json, name_norm)| {
                let rank = match_rank(&name_norm, &email, &needle)?;
                Some((rank, name_norm, person_from_row(email, name, &kind, &json)))
            })
            .collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.email.cmp(&b.2.email)));
        Ok(hits.into_iter().take(limit).map(|(_, _, p)| p).collect())
    }

    /// Имя владельца ящика: самое частое имя отправителя писем «Отправленных». `None`, если папка
    /// или письма не загружены.
    pub fn own_display_name(&self, account: AccountId) -> Result<Option<String>> {
        let Some((sent_pk, _)) = self.folder_by_distinguished(account, "sentitems")? else { return Ok(None) };
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT from_name, COUNT(*) AS n FROM items WHERE folder_pk = ?1 AND deleted = 0 AND from_name IS NOT NULL AND trim(from_name) != ''
                  GROUP BY from_name ORDER BY n DESC, from_name LIMIT 1",
                params![sent_pk],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
    }

    /// Счётчики переписки с адресом по локальной базе (`ContactLocalStats`): удалённые и черновики
    /// не считаются. Адрес сравнивается без учёта регистра (индексы версии 5).
    pub fn contact_local_stats(&self, account: AccountId, email: &str) -> Result<ContactLocalStats> {
        let key = email.trim().to_lowercase();
        if key.is_empty() {
            return Ok(ContactLocalStats::default());
        }
        let own = serde_json::to_string(&self.own_addresses(account)?).unwrap_or_else(|_| "[]".into());
        let sent_pk = self.folder_by_distinguished(account, "sentitems")?.map(|(pk, _)| pk).unwrap_or(-1);
        self.with_conn(|c| {
            let (messages_from, from_last): (i64, Option<i64>) = c.query_row(
                "SELECT COUNT(*), MAX(COALESCE(received_at, sent_at)) FROM items
                  WHERE account_id = ?1 AND lower(from_email) = ?2 AND deleted = 0 AND is_draft = 0",
                params![account.0, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let (messages_to, to_last): (i64, Option<i64>) = c.query_row(
                "SELECT COUNT(DISTINCT i.item_pk), MAX(COALESCE(i.sent_at, i.received_at)) FROM recipients r JOIN items i ON i.item_pk = r.item_pk
                  WHERE lower(r.email) = ?2 AND r.kind IN (1, 2, 3) AND i.account_id = ?1 AND i.deleted = 0 AND i.is_draft = 0
                    AND (i.folder_pk = ?3 OR lower(i.from_email) IN (SELECT value FROM json_each(?4)))",
                params![account.0, key, sent_pk, own],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(ContactLocalStats { messages_from, messages_to, last_ms: from_last.max(to_last) })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_photo_cache_round_trip() {
        let path = std::env::temp_dir().join(format!("aiowa-dir-photo-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let s = Store::open(&path).unwrap();
        let acc = s.account("h", "u").unwrap();
        let person = DirectoryPerson { display_name: Some("Имя".into()), email: Some("A@x.ru".into()), kind: PersonKind::Mailbox, title: None, department: None, company: None, office: None, business_phone: None, mobile_phone: None, alias: None, ..Default::default() };
        assert!(!s.set_directory_photo(acc, "a@x.ru", Some(&[1, 2, 3])).unwrap(), "записи ещё нет");
        assert!(s.replace_directory_person(acc, &person).unwrap());
        assert_eq!(s.directory_photo(acc, "a@x.ru").unwrap(), None, "фото не запрашивалось");
        assert!(s.set_directory_photo(acc, "A@x.ru", Some(&[1, 2, 3])).unwrap());
        let (photo, at) = s.directory_photo(acc, "a@x.ru").unwrap().unwrap();
        assert_eq!((photo.as_deref(), at > 0), (Some(&[1u8, 2, 3][..]), true));
        assert_eq!(s.directory_photos(acc, &["a@x.ru".into(), "b@x.ru".into()]).unwrap(), vec![("a@x.ru".to_string(), vec![1, 2, 3])]);
        // Повторная запись записи справочника фото не стирает; отметка «нет фото» — photo NULL, photo_at есть.
        assert!(s.replace_directory_person(acc, &person).unwrap());
        assert!(s.directory_photo(acc, "a@x.ru").unwrap().unwrap().0.is_some());
        assert!(s.set_directory_photo(acc, "a@x.ru", None).unwrap());
        assert_eq!(s.directory_photo(acc, "a@x.ru").unwrap().map(|(p, _)| p.is_none()), Some(true));
        assert!(s.directory_photos(acc, &["a@x.ru".into()]).unwrap().is_empty());
    }
    use aiowa_core::{DistinguishedFolder, Folder, FolderId, ItemChange, ItemId, MessageFull, MessageMeta, Participant, SyncState};
    use jiff::Timestamp;

    fn temp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("aiowa-directory-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(dir.join("mail.db")).unwrap()
    }

    fn person(name: &str, email: &str, kind: PersonKind) -> DirectoryPerson {
        DirectoryPerson { display_name: Some(name.into()), email: Some(email.into()), kind, ..Default::default() }
    }

    #[test]
    fn upsert_merges_and_lookup_ignores_case() {
        let store = temp_store("upsert");
        let acc = store.account("h", "u").unwrap();
        let full = DirectoryPerson { title: Some("Инженер".into()), mobile_phone: Some("+7 900".into()), alias: Some("ipetrov".into()), ..person("Петров Иван", "Ivan.Petrov@X.ru", PersonKind::Mailbox) };
        assert_eq!(store.upsert_directory_people(acc, &[full, person("Без адреса", "/o=Org/cn=ex", PersonKind::Mailbox)]).unwrap(), 1, "запись без SMTP пропущена");
        let p = store.directory_person(acc, " IVAN.petrov@x.RU ").unwrap().unwrap();
        assert_eq!(p.email.as_deref(), Some("ivan.petrov@x.ru"), "ключ в нижнем регистре");
        assert_eq!((p.title.as_deref(), p.mobile_phone.as_deref(), p.alias.as_deref()), (Some("Инженер"), Some("+7 900"), Some("ipetrov")));
        let first_at = store.directory_person_fetched_at(acc, "ivan.petrov@x.ru").unwrap().unwrap();
        // Выдача FindPeople без телефонов: имя и должность обновляются, телефон остаётся.
        let short = DirectoryPerson { title: Some("Ведущий инженер".into()), ..person("Петров Иван Сергеевич", "ivan.petrov@x.ru", PersonKind::Other) };
        assert_eq!(store.upsert_directory_people(acc, &[short]).unwrap(), 1);
        let p = store.directory_person(acc, "ivan.petrov@x.ru").unwrap().unwrap();
        assert_eq!(p.display_name.as_deref(), Some("Петров Иван Сергеевич"));
        assert_eq!(p.title.as_deref(), Some("Ведущий инженер"));
        assert_eq!(p.mobile_phone.as_deref(), Some("+7 900"), "upsert не стирает поля, которых нет в новой записи");
        assert_eq!(p.kind, PersonKind::Mailbox, "вид Other не затирает известный");
        assert!(store.directory_person_fetched_at(acc, "ivan.petrov@x.ru").unwrap().unwrap() >= first_at);
        // Полная замена: телефона больше нет.
        assert!(store.replace_directory_person(acc, &person("Петров Иван", "ivan.petrov@x.ru", PersonKind::Mailbox)).unwrap());
        let p = store.directory_person(acc, "ivan.petrov@x.ru").unwrap().unwrap();
        assert_eq!((p.title, p.mobile_phone), (None, None));
        assert!(!store.replace_directory_person(acc, &person("EX", "ex-address", PersonKind::Mailbox)).unwrap());
        assert_eq!(store.directory_person(acc, "nobody@x.ru").unwrap(), None);
        assert_eq!(store.directory_person(acc, "  ").unwrap(), None);
        // Записи другого аккаунта не видны.
        let other = store.account("h", "other").unwrap();
        assert_eq!(store.directory_person(other, "ivan.petrov@x.ru").unwrap(), None);
    }

    #[test]
    fn cached_search_by_word_prefix_with_yo() {
        let store = temp_store("search");
        let acc = store.account("h", "u").unwrap();
        store
            .upsert_directory_people(
                acc,
                &[
                    person("Семёнов Пётр", "p.semenov@x.ru", PersonKind::Mailbox),
                    person("Аксёнова Анна", "anna@x.ru", PersonKind::Mailbox),
                    person("Все сотрудники", "all-staff@x.ru", PersonKind::DistributionList),
                    person("Переговорная Семь", "room7@x.ru", PersonKind::Room),
                ],
            )
            .unwrap();
        let names = |q: &str| store.directory_search_cached(acc, q, 10).unwrap().into_iter().map(|p| p.display_name.unwrap_or_default()).collect::<Vec<_>>();
        assert_eq!(names("сем"), vec!["Семёнов Пётр", "Переговорная Семь"], "без регистра; начало имени раньше второго слова");
        assert_eq!(names("семе"), vec!["Семёнов Пётр"], "ё в имени = е в запросе");
        assert_eq!(names("СЕМЁ"), vec!["Семёнов Пётр"], "запрос с ё и в верхнем регистре");
        assert_eq!(names("петр"), vec!["Семёнов Пётр"], "по второму слову");
        assert_eq!(names("ксен"), Vec::<String>::new(), "середина слова не совпадает");
        assert_eq!(names("staff"), vec!["Все сотрудники"], "по слову адреса после дефиса");
        assert_eq!(names("semenov"), vec!["Семёнов Пётр"], "по слову адреса после точки");
        assert_eq!(names("anna@"), vec!["Аксёнова Анна"]);
        assert!(names("").is_empty() && names("   ").is_empty(), "пустой запрос — пусто");
        let hits = store.directory_search_cached(acc, "все", 10).unwrap();
        assert_eq!(hits[0].kind, PersonKind::DistributionList);
        assert_eq!(store.directory_search_cached(acc, "с", 1).unwrap().len(), 1, "limit соблюдается");

        // Проверка одной записи совпадает с поиском по кэшу.
        let semenov = person("Семёнов Пётр", "P.Semenov@x.ru", PersonKind::Mailbox);
        for (q, want) in [("сем", true), ("СЕМЁ", true), ("петр", true), ("semenov", true), ("ксен", false), ("ёнов", false), ("  ", false)] {
            assert_eq!(directory_person_matches(&semenov, q), want, "{q}");
        }
        assert!(!directory_person_matches(&person("EX", "/o=Org/cn=ex", PersonKind::Mailbox), "ex"), "без SMTP-адреса в кэше записи нет");
    }

    fn meta(id: &str, from: &str, at: i64) -> MessageMeta {
        MessageMeta {
            item_id: ItemId { id: id.into(), change_key: None },
            parent_folder_id: None,
            conversation_id: None,
            internet_message_id: None,
            item_class: Some("IPM.Note".into()),
            subject: Some("тема".into()),
            from: Some(Participant { name: Some(format!("Имя {from}")), email: Some(from.into()), routing_type: None }),
            sender: None,
            display_to: None,
            received_at: Timestamp::from_millisecond(at).ok(),
            sent_at: Timestamp::from_millisecond(at).ok(),
            last_modified_at: None,
            is_read: true,
            is_draft: false,
            has_attachments: false,
            importance: Default::default(),
            flag: Default::default(),
            size: None,
            preview: None,
            last_verb: None,
            last_verb_at: None,
        }
    }

    fn folder(id: &str, dist: Option<DistinguishedFolder>) -> Folder {
        Folder {
            id: FolderId { id: id.into(), change_key: None },
            parent_id: None,
            display_name: id.into(),
            distinguished: dist.map(|d| d.as_str().to_string()),
            folder_class: Some("IPF.Note".into()),
            total_count: 0,
            unread_count: 0,
            child_folder_count: 0,
        }
    }

    #[test]
    fn local_stats_count_mail_from_and_to_address() {
        let store = temp_store("stats");
        let acc = store.account("h", "u").unwrap();
        store.upsert_folders(acc, &[folder("INBOX", Some(DistinguishedFolder::Inbox)), folder("SENT", Some(DistinguishedFolder::SentItems))]).unwrap();
        let inbox = store.folder_pk(acc, "INBOX").unwrap().unwrap();
        let sent = store.folder_pk(acc, "SENT").unwrap().unwrap();
        // Входящие: два письма от Ивана (адрес в разном регистре), одно от третьего лица Ивану в копию.
        let state = SyncState("s".into());
        store.apply_item_batch(acc, inbox, &[ItemChange::Create(meta("i1", "Ivan@X.ru", 1_000)), ItemChange::Create(meta("i2", "ivan@x.ru", 3_000)), ItemChange::Create(meta("i3", "boss@x.ru", 4_000))], &state, true).unwrap();
        // Отправленные: письмо владельца Ивану.
        store.apply_item_batch(acc, sent, &[ItemChange::Create(meta("s1", "me@x.ru", 2_000))], &state, true).unwrap();
        let full = |id: &str, from: &str, at: i64, to: &[&str], cc: &[&str]| MessageFull {
            meta: meta(id, from, at),
            to: to.iter().map(|e| Participant { name: None, email: Some((*e).into()), routing_type: None }).collect(),
            cc: cc.iter().map(|e| Participant { name: None, email: Some((*e).into()), routing_type: None }).collect(),
            bcc: vec![],
            reply_to: vec![],
            body: None,
            attachments: vec![],
        };
        store.store_full_messages(acc, sent, &[full("s1", "me@x.ru", 2_000, &["IVAN@x.ru"], &[])]).unwrap();
        store.store_full_messages(acc, inbox, &[full("i3", "boss@x.ru", 4_000, &["me@x.ru"], &["ivan@x.ru"])]).unwrap();
        let s = store.contact_local_stats(acc, " IVAN@X.RU ").unwrap();
        assert_eq!(s, ContactLocalStats { messages_from: 2, messages_to: 1, last_ms: Some(3_000) }, "письмо третьего лица Ивану в копию не считается письмом ему от владельца");
        let s = store.contact_local_stats(acc, "boss@x.ru").unwrap();
        assert_eq!((s.messages_from, s.messages_to, s.last_ms), (1, 0, Some(4_000)));
        assert_eq!(store.contact_local_stats(acc, "nobody@x.ru").unwrap(), ContactLocalStats::default());
        assert_eq!(store.contact_local_stats(acc, "").unwrap(), ContactLocalStats::default());
        assert_eq!(store.own_display_name(acc).unwrap().as_deref(), Some("Имя me@x.ru"));
        let empty = store.account("h", "empty").unwrap();
        assert_eq!(store.own_display_name(empty).unwrap(), None, "без «Отправленных» имени нет");
    }
}
