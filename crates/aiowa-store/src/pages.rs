//! Текст вложения по страницам (`attachment_pages`, схема v19): что извлечено из файла и что
//! распознано на сканах (OCR). По этой таблице инструмент агента отдаёт текст с номерами страниц и
//! не распознаёт одну страницу дважды; текст вложения целиком (`attachments.text`), по которому
//! идёт поиск, после распознавания собирается из страниц.

use crate::{fts_delete, fts_insert, Result, Store, TEXT_OK};
use rusqlite::{params, OptionalExtension};

/// Текст страницы взят из самого файла (текстовый слой PDF).
pub const PAGE_EXTRACTED: i64 = 1;
/// Текст страницы распознан по её изображению.
pub const PAGE_OCR: i64 = 2;

/// Текст одной страницы вложения; у страницы может быть и извлечённый, и распознанный текст.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentPage {
    /// Номер страницы с единицы.
    pub page: i64,
    /// `PAGE_EXTRACTED` или `PAGE_OCR`.
    pub source: i64,
    pub text: String,
}

impl Store {
    /// Число страниц вложения (`None` — ещё не считали) и текст известных страниц по порядку.
    pub fn attachment_pages(&self, attachment_pk: i64) -> Result<(Option<i64>, Vec<AttachmentPage>)> {
        self.with_conn(|c| {
            let total: Option<i64> = c.query_row("SELECT pages_total FROM attachments WHERE attachment_pk = ?1", params![attachment_pk], |r| r.get(0)).optional()?.flatten();
            let mut st = c.prepare_cached("SELECT page, source, text FROM attachment_pages WHERE attachment_pk = ?1 ORDER BY page, source")?;
            let rows = st.query_map(params![attachment_pk], |r| Ok(AttachmentPage { page: r.get(0)?, source: r.get(1)?, text: r.get(2)? }))?;
            Ok((total, rows.collect::<rusqlite::Result<Vec<_>>>()?))
        })
    }

    /// Записать число страниц и текст страниц (прежний текст той же страницы и того же источника
    /// заменяется). `whole_text` — текст вложения целиком, собранный из страниц: он заменяет
    /// `attachments.text`, и письмо начинает находиться поиском по словам из скана.
    pub fn save_attachment_pages(&self, attachment_pk: i64, pages_total: i64, pages: &[AttachmentPage], whole_text: Option<&str>) -> Result<()> {
        self.with_tx(|tx| {
            let Some(item_pk) = tx.query_row("SELECT item_pk FROM attachments WHERE attachment_pk = ?1", params![attachment_pk], |r| r.get::<_, i64>(0)).optional()? else { return Ok(()) };
            {
                let mut st = tx.prepare_cached("INSERT OR REPLACE INTO attachment_pages(attachment_pk, page, source, text) VALUES (?1, ?2, ?3, ?4)")?;
                for p in pages {
                    st.execute(params![attachment_pk, p.page, p.source, p.text])?;
                }
            }
            tx.execute("UPDATE attachments SET pages_total = ?2 WHERE attachment_pk = ?1", params![attachment_pk, pages_total])?;
            if let Some(text) = whole_text {
                fts_delete(tx, item_pk)?;
                tx.execute("UPDATE attachments SET text = ?2, text_state = ?3 WHERE attachment_pk = ?1", params![attachment_pk, text, TEXT_OK])?;
                fts_insert(tx, item_pk)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{AttachmentMeta, BodyType, FlagStatus, Folder, FolderId, Importance, ItemId, MessageBody, MessageFull, MessageMeta, Participant};

    /// База с одним письмом во «Входящих» и одним вложением-сканом без текста.
    fn store_with_scan(tag: &str) -> (Store, aiowa_core::AccountId, i64, i64) {
        let dir = std::env::temp_dir().join(format!("aiowa-pages-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("mail.db")).unwrap();
        let acc = store.account("h", "u").unwrap();
        let inbox = Folder { id: FolderId { id: "I".into(), change_key: None }, parent_id: None, display_name: "Входящие".into(), distinguished: Some("inbox".into()), folder_class: Some("IPF.Note".into()), total_count: 1, unread_count: 0, child_folder_count: 0 };
        store.upsert_folders(acc, &[inbox]).unwrap();
        let (folder_pk, _) = store.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let when: jiff::Timestamp = "2026-11-20T08:00:00Z".parse().unwrap();
        let message = MessageFull {
            meta: MessageMeta {
                item_id: ItemId { id: "A".into(), change_key: Some("ck".into()) },
                parent_folder_id: None,
                conversation_id: None,
                internet_message_id: None,
                item_class: Some("IPM.Note".into()),
                subject: Some("Уведомление".into()),
                from: Some(Participant { name: Some("Надзор".into()), email: Some("n@x.ru".into()), routing_type: Some("SMTP".into()) }),
                sender: None,
                display_to: None,
                received_at: Some(when),
                sent_at: Some(when),
                last_modified_at: None,
                is_read: true,
                is_draft: false,
                has_attachments: true,
                importance: Importance::Normal,
                flag: FlagStatus::NotFlagged,
                size: Some(1000),
                preview: None,
                last_verb: None,
                last_verb_at: None,
            },
            to: vec![],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            body: Some(MessageBody { body_type: BodyType::Html, unique_body: Some("<p>См. вложение.</p>".into()), full_body: None, is_truncated: false }),
            attachments: vec![AttachmentMeta { id: "att".into(), name: Some("скан.pdf".into()), content_type: Some("application/pdf".into()), size: Some(100), is_inline: false, content_id: None }],
        };
        store.store_full_messages(acc, folder_pk, &[message]).unwrap();
        let item_pk = store.search(acc, "уведомление", 5).unwrap()[0].item_pk;
        let attachment_pk = store.attachments_of_item(item_pk).unwrap()[0].attachment_pk;
        (store, acc, item_pk, attachment_pk)
    }

    #[test]
    fn pages_are_kept_replaced_and_feed_the_search() {
        let (store, acc, item_pk, pk) = store_with_scan("basic");
        assert_eq!(store.attachment_pages(pk).unwrap(), (None, vec![]));
        assert!(store.search(acc, "дизель", 5).unwrap().is_empty());

        // Первая страница — текст из файла, вторая — распознана; текст целиком идёт в поиск.
        let pages = [AttachmentPage { page: 1, source: PAGE_EXTRACTED, text: "Титульный лист".into() }, AttachmentPage { page: 2, source: PAGE_OCR, text: "Проверка дизель-генератора".into() }];
        store.save_attachment_pages(pk, 3, &pages, Some("Титульный лист\n\nПроверка дизель-генератора")).unwrap();
        let (total, saved) = store.attachment_pages(pk).unwrap();
        assert_eq!((total, saved.as_slice()), (Some(3), pages.as_slice()));
        let row = store.attachment(pk).unwrap().unwrap();
        assert_eq!((row.text_state, row.text_len > 0), (TEXT_OK, true));
        assert_eq!(store.search(acc, "дизель", 5).unwrap().iter().map(|h| h.item_pk).collect::<Vec<_>>(), [item_pk], "письмо находится по слову из скана");

        // Та же страница и тот же источник — замена; другой источник той же страницы — рядом.
        store.save_attachment_pages(pk, 3, &[AttachmentPage { page: 2, source: PAGE_OCR, text: "Исправлено".into() }, AttachmentPage { page: 1, source: PAGE_OCR, text: "Титул распознан".into() }], None).unwrap();
        let (_, saved) = store.attachment_pages(pk).unwrap();
        assert_eq!(saved.iter().map(|p| (p.page, p.source, p.text.as_str())).collect::<Vec<_>>(), [(1, PAGE_EXTRACTED, "Титульный лист"), (1, PAGE_OCR, "Титул распознан"), (2, PAGE_OCR, "Исправлено")]);
        assert_eq!(store.attachment_text(pk).unwrap().as_deref(), Some("Титульный лист\n\nПроверка дизель-генератора"), "без нового текста целиком прежний не трогается");

        // Несуществующее вложение — не ошибка и не запись.
        store.save_attachment_pages(999_999, 1, &pages, Some("x")).unwrap();
        assert_eq!(store.attachment_pages(999_999).unwrap(), (None, vec![]));
    }
}
