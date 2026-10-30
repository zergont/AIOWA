//! Локальные черновики композера: текст письма, который живёт в базе учётной записи, пока окно
//! композера открыто, независимо от связи с сервером. Окно пишет его раз в одну-две секунды после
//! правок, убирает после отправки, сохранения на сервере или отказа от письма; оставшиеся после
//! падения или закрытия окна черновики предлагаются при следующем запуске (docs/obkatka.md п. 22).
//! Содержимое — то же, что уходит в `ComposeRequest`: получатели и вложения как JSON окна.

use crate::{now_ms, Result, Store};
use aiowa_core::AccountId;
use rusqlite::{params, OptionalExtension};

/// Локальный черновик. Получатели и вложения — JSON в том виде, в каком их держит окно.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDraft {
    /// Идентификатор экземпляра композера (окно создаёт его само, UUID).
    pub draft_id: String,
    /// Вид письма: `new`, `reply`, `reply_all`, `forward`.
    pub kind: String,
    pub reference_item_pk: Option<i64>,
    /// Черновик на сервере, если он уже создавался: следующее сохранение обновит его.
    pub draft_server_id: Option<String>,
    pub subject: String,
    pub to_json: String,
    pub cc_json: String,
    pub bcc_json: String,
    pub body_html: String,
    /// Файлы-вложения окна (путь, имя, размер) как JSON.
    pub attachments_json: String,
    /// Убранные серверные вложения (`AttachmentId`) как JSON.
    pub removed_json: String,
    pub created_at: i64,
    pub updated_at: i64,
}

const COLUMNS: &str = "draft_id, kind, reference_item_pk, draft_server_id, subject, to_json, cc_json, bcc_json, body_html, attachments_json, removed_json, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LocalDraft> {
    Ok(LocalDraft {
        draft_id: r.get(0)?,
        kind: r.get(1)?,
        reference_item_pk: r.get(2)?,
        draft_server_id: r.get(3)?,
        subject: r.get(4)?,
        to_json: r.get(5)?,
        cc_json: r.get(6)?,
        bcc_json: r.get(7)?,
        body_html: r.get(8)?,
        attachments_json: r.get(9)?,
        removed_json: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

impl Store {
    /// Записать или обновить локальный черновик (`created_at` при обновлении не меняется).
    /// Известный черновик на сервере пустым `draft_server_id` не стирается: его записывает и
    /// отправка перед `SendItem`, а окно, пока она идёт, может прислать запись без него.
    pub fn local_draft_save(&self, account: AccountId, d: &LocalDraft) -> Result<()> {
        let now = now_ms();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO local_drafts(account_id, draft_id, kind, reference_item_pk, draft_server_id, subject, to_json, cc_json, bcc_json, body_html, attachments_json, removed_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)
                 ON CONFLICT(draft_id) DO UPDATE SET kind = excluded.kind, reference_item_pk = excluded.reference_item_pk, draft_server_id = COALESCE(excluded.draft_server_id, local_drafts.draft_server_id),
                   subject = excluded.subject, to_json = excluded.to_json, cc_json = excluded.cc_json, bcc_json = excluded.bcc_json, body_html = excluded.body_html,
                   attachments_json = excluded.attachments_json, removed_json = excluded.removed_json, updated_at = excluded.updated_at",
                params![account.0, d.draft_id, d.kind, d.reference_item_pk, d.draft_server_id, d.subject, d.to_json, d.cc_json, d.bcc_json, d.body_html, d.attachments_json, d.removed_json, now],
            )?;
            Ok(())
        })
    }

    /// Убрать локальный черновик; отсутствующий — не ошибка.
    pub fn local_draft_drop(&self, draft_id: &str) -> Result<bool> {
        self.with_conn(|c| Ok(c.execute("DELETE FROM local_drafts WHERE draft_id = ?1", params![draft_id])? > 0))
    }

    pub fn local_draft(&self, draft_id: &str) -> Result<Option<LocalDraft>> {
        self.with_conn(|c| Ok(c.query_row(&format!("SELECT {COLUMNS} FROM local_drafts WHERE draft_id = ?1"), params![draft_id], row).optional()?))
    }

    /// Локальные черновики учётной записи, свежие первыми.
    pub fn local_drafts(&self, account: AccountId) -> Result<Vec<LocalDraft>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {COLUMNS} FROM local_drafts WHERE account_id = ?1 ORDER BY updated_at DESC"))?;
            let rows = st.query_map(params![account.0], row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }

    /// Черновики на сервере, на которые ссылаются локальные черновики учётной записи, кроме
    /// `except` (свой локальный черновик композера): черновик, найденный после оборванного
    /// `CreateItem`, не должен оказаться письмом другого композера.
    pub fn local_draft_server_ids(&self, account: AccountId, except: Option<&str>) -> Result<Vec<String>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached("SELECT draft_server_id FROM local_drafts WHERE account_id = ?1 AND draft_server_id IS NOT NULL AND draft_id <> ?2")?;
            let rows = st.query_map(params![account.0, except.unwrap_or("")], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Store {
        let path = std::env::temp_dir().join(format!("aiowa-drafts-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Store::open(&path).unwrap()
    }

    fn draft(id: &str, subject: &str) -> LocalDraft {
        LocalDraft {
            draft_id: id.into(),
            kind: "reply".into(),
            reference_item_pk: Some(7),
            draft_server_id: None,
            subject: subject.into(),
            to_json: r#"[{"name":null,"email":"a@x.ru"}]"#.into(),
            cc_json: "[]".into(),
            bcc_json: "[]".into(),
            body_html: "<p>текст</p>".into(),
            attachments_json: "[]".into(),
            removed_json: "[]".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn save_update_list_drop() {
        let s = store("basic");
        let acc = s.account("h", "u").unwrap();
        s.local_draft_save(acc, &draft("d1", "первый")).unwrap();
        s.local_draft_save(acc, &draft("d2", "второй")).unwrap();
        let first = s.local_draft("d1").unwrap().unwrap();
        assert_eq!((first.subject.as_str(), first.reference_item_pk, first.kind.as_str()), ("первый", Some(7), "reply"));
        assert!(first.created_at > 0 && first.created_at == first.updated_at);

        // Обновление: created_at прежний, updated_at не раньше прежнего, поля заменены.
        let mut d1 = draft("d1", "первый, правка");
        d1.draft_server_id = Some("SRV".into());
        d1.attachments_json = r#"[{"path":"C:/a.pdf","name":"a.pdf","size":3}]"#.into();
        s.local_draft_save(acc, &d1).unwrap();
        let again = s.local_draft("d1").unwrap().unwrap();
        assert_eq!(again.created_at, first.created_at);
        assert!(again.updated_at >= first.updated_at);
        assert_eq!((again.subject.as_str(), again.draft_server_id.as_deref()), ("первый, правка", Some("SRV")));
        assert!(again.attachments_json.contains("a.pdf"));

        let all = s.local_drafts(acc).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].draft_id, "d1", "свежий первым: {all:?}");
        let other = s.account("h", "v").unwrap();
        assert!(s.local_drafts(other).unwrap().is_empty(), "черновики другой учётной записи не видны");

        assert!(s.local_draft_drop("d1").unwrap());
        assert!(!s.local_draft_drop("d1").unwrap(), "повторное удаление — не ошибка");
        assert!(s.local_draft("d1").unwrap().is_none());
        assert_eq!(s.local_drafts(acc).unwrap().len(), 1);
    }

    #[test]
    fn server_id_is_not_erased_by_a_save_without_it() {
        let s = store("server-id");
        let acc = s.account("h", "u").unwrap();
        let mut d = draft("d1", "тема");
        d.draft_server_id = Some("SRV".into());
        s.local_draft_save(acc, &d).unwrap();
        // Окно, пока шла отправка, прислало запись без черновика на сервере: ссылка на него остаётся, текст обновляется.
        let mut later = draft("d1", "тема, правка");
        later.body_html = "<p>новый текст</p>".into();
        s.local_draft_save(acc, &later).unwrap();
        let got = s.local_draft("d1").unwrap().unwrap();
        assert_eq!((got.draft_server_id.as_deref(), got.subject.as_str(), got.body_html.as_str()), (Some("SRV"), "тема, правка", "<p>новый текст</p>"));
        // Другой черновик на сервере заменяет прежний.
        let mut other = draft("d1", "тема");
        other.draft_server_id = Some("SRV2".into());
        s.local_draft_save(acc, &other).unwrap();
        assert_eq!(s.local_draft("d1").unwrap().unwrap().draft_server_id.as_deref(), Some("SRV2"));
    }

    #[test]
    fn server_ids_of_other_local_drafts() {
        let s = store("server-ids");
        let acc = s.account("h", "u").unwrap();
        let with = |id: &str, srv: Option<&str>| {
            let mut d = draft(id, "тема");
            d.draft_server_id = srv.map(String::from);
            d
        };
        s.local_draft_save(acc, &with("mine", Some("SRV-MINE"))).unwrap();
        s.local_draft_save(acc, &with("other", Some("SRV-OTHER"))).unwrap();
        s.local_draft_save(acc, &with("none", None)).unwrap();
        let foreign = s.account("h", "v").unwrap();
        s.local_draft_save(foreign, &with("foreign", Some("SRV-FOREIGN"))).unwrap();
        assert_eq!(s.local_draft_server_ids(acc, Some("mine")).unwrap(), vec!["SRV-OTHER".to_string()], "свой, без черновика на сервере и чужой учётной записи не в счёт");
        let mut all = s.local_draft_server_ids(acc, None).unwrap();
        all.sort();
        assert_eq!(all, vec!["SRV-MINE".to_string(), "SRV-OTHER".to_string()]);
    }
}
