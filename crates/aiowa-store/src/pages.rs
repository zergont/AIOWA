//! Текст вложения по страницам (`attachment_pages`, схема v19): что извлечено из файла и что
//! распознано на сканах (OCR). По этой таблице инструмент агента отдаёт текст с номерами страниц и
//! не распознаёт одну страницу дважды; текст вложения целиком (`attachments.text`), по которому
//! идёт поиск, после распознавания собирается из страниц.
//!
//! Здесь же очередь фонового распознавания (схема v21): какие скачанные вложения окно ещё не
//! разбирало (`attachments.ocr_at`).

use crate::{fts_delete, fts_insert, now_ms, Result, Store, TEXT_OK};
use aiowa_core::AccountId;
use rusqlite::{params, OptionalExtension};

/// Текст страницы взят из самого файла (текстовый слой PDF).
pub const PAGE_EXTRACTED: i64 = 1;
/// Текст страницы распознан по её изображению.
pub const PAGE_OCR: i64 = 2;

/// Что берёт фоновая очередь распознавания: скачанные вложения неудалённых писем не старше `?2`,
/// не встроенные в текст письма, PDF и картинки (по расширению имени или по типу, как
/// `aiowa_ocr::source_kind`), которых очередь ещё не разбирала. `LIKE` в SQLite не различает регистр
/// латиницы, а в расширениях и типах другой нет.
const OCR_QUEUE: &str = "FROM attachments a JOIN items i ON i.item_pk = a.item_pk
    WHERE i.account_id = ?1 AND i.deleted = 0 AND a.is_inline = 0 AND a.local_path IS NOT NULL AND a.ocr_at IS NULL
      AND COALESCE(i.received_at, i.sent_at, 0) >= ?2
      AND (a.name LIKE '%.pdf' OR a.name LIKE '%.jpg' OR a.name LIKE '%.jpeg' OR a.name LIKE '%.jpe' OR a.name LIKE '%.png'
        OR a.name LIKE '%.tif' OR a.name LIKE '%.tiff' OR a.name LIKE '%.bmp'
        OR trim(a.content_type) LIKE 'application/pdf%' OR trim(a.content_type) LIKE 'image/jpeg%' OR trim(a.content_type) LIKE 'image/pjpeg%'
        OR trim(a.content_type) LIKE 'image/png%' OR trim(a.content_type) LIKE 'image/tiff%' OR trim(a.content_type) LIKE 'image/bmp%')";

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

    /// Очередь фонового распознавания: ключи вложений писем не старше `since_ms`, свежие письма
    /// первыми. Какие вложения в неё попадают — см. `OCR_QUEUE`.
    pub fn ocr_queue(&self, account: AccountId, since_ms: i64) -> Result<Vec<i64>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT a.attachment_pk {OCR_QUEUE} ORDER BY COALESCE(i.received_at, i.sent_at, 0) DESC, a.attachment_pk DESC"))?;
            let rows = st.query_map(params![account.0, since_ms], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
        })
    }

    /// Сколько вложений ждёт в очереди и у скольких вложений учётной записи на страницах распознан
    /// текст (за всё время, кто бы его ни распознал — очередь или агент; чистый лист и фотография
    /// без надписей не в счёт).
    pub fn ocr_queue_stats(&self, account: AccountId, since_ms: i64) -> Result<(i64, i64)> {
        self.with_conn(|c| {
            let waiting: i64 = c.query_row(&format!("SELECT COUNT(*) {OCR_QUEUE}"), params![account.0, since_ms], |r| r.get(0))?;
            let recognized: i64 = c.query_row(
                "SELECT COUNT(DISTINCT p.attachment_pk) FROM attachment_pages p
                   JOIN attachments a ON a.attachment_pk = p.attachment_pk JOIN items i ON i.item_pk = a.item_pk
                  WHERE p.source = ?2 AND p.text <> '' AND i.account_id = ?1",
                params![account.0, PAGE_OCR],
                |r| r.get(0),
            )?;
            Ok((waiting, recognized))
        })
    }

    /// Очередь разобрала вложение: распознала, нашла, что распознавать нечего, или не смогла его
    /// открыть. Больше она к нему не возвращается; агент по запросу читает его как раньше.
    pub fn mark_ocr_done(&self, attachment_pk: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("UPDATE attachments SET ocr_at = ?2 WHERE attachment_pk = ?1 AND ocr_at IS NULL", params![attachment_pk, now_ms()])?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{AttachmentMeta, BodyType, FlagStatus, Folder, FolderId, Importance, ItemId, MessageBody, MessageFull, MessageMeta, Participant};

    fn file(id: &str, name: &str, content_type: Option<&str>, is_inline: bool) -> AttachmentMeta {
        AttachmentMeta { id: id.into(), name: Some(name.into()), content_type: content_type.map(String::from), size: Some(100), is_inline, content_id: None }
    }

    /// Пустая база с папкой «Входящие».
    fn empty_store(tag: &str) -> (Store, AccountId, i64) {
        let dir = std::env::temp_dir().join(format!("aiowa-pages-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("mail.db")).unwrap();
        let acc = store.account("h", "u").unwrap();
        let inbox = Folder { id: FolderId { id: "I".into(), change_key: None }, parent_id: None, display_name: "Входящие".into(), distinguished: Some("inbox".into()), folder_class: Some("IPF.Note".into()), total_count: 1, unread_count: 0, child_folder_count: 0 };
        store.upsert_folders(acc, &[inbox]).unwrap();
        let (folder_pk, _) = store.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        (store, acc, folder_pk)
    }

    /// База с одним письмом во «Входящих» и одним вложением-сканом без текста.
    fn store_with_scan(tag: &str) -> (Store, AccountId, i64, i64) {
        let (store, acc, folder_pk) = empty_store(tag);
        store.store_full_messages(acc, folder_pk, &[message("A", "Уведомление", "2026-11-20T08:00:00Z", vec![file("att", "скан.pdf", Some("application/pdf"), false)])]).unwrap();
        let item_pk = store.search(acc, "уведомление", 5).unwrap()[0].item_pk;
        let attachment_pk = store.attachments_of_item(item_pk).unwrap()[0].attachment_pk;
        (store, acc, item_pk, attachment_pk)
    }

    fn message(id: &str, subject: &str, at: &str, attachments: Vec<AttachmentMeta>) -> MessageFull {
        let when: jiff::Timestamp = at.parse().unwrap();
        MessageFull {
            meta: MessageMeta {
                item_id: ItemId { id: id.into(), change_key: Some("ck".into()) },
                parent_folder_id: None,
                conversation_id: None,
                internet_message_id: None,
                item_class: Some("IPM.Note".into()),
                subject: Some(subject.into()),
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
            attachments,
        }
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

    #[test]
    fn ocr_queue_takes_recent_downloaded_scans_once() {
        let (store, acc, folder_pk) = empty_store("queue");
        let fresh = vec![
            file("pdf", "Акт.PDF", Some("application/pdf"), false),
            file("photo", "фото.jpeg", None, false),
            file("typed", "без расширения", Some("image/png; name=x"), false),
            file("logo", "logo.png", Some("image/png"), true),
            file("doc", "договор.docx", None, false),
            file("later", "не скачан.pdf", Some("application/pdf"), false),
        ];
        store.store_full_messages(acc, folder_pk, &[message("N", "Свежее письмо", "2026-11-20T08:00:00Z", fresh), message("O", "Старое письмо", "2025-01-10T08:00:00Z", vec![file("old", "архив.pdf", None, false)])]).unwrap();
        let pk_of = |subject: &str, server_id: &str| {
            let item_pk = store.search(acc, subject, 5).unwrap()[0].item_pk;
            store.attachments_of_item(item_pk).unwrap().into_iter().find(|a| a.server_id == server_id).unwrap()
        };
        // Скачано всё, кроме одного файла.
        for (subject, id) in [("свежее", "pdf"), ("свежее", "photo"), ("свежее", "typed"), ("свежее", "logo"), ("свежее", "doc"), ("старое", "old")] {
            store.save_attachment(&pk_of(subject, id).todo(), b"file", None, crate::AttachmentText::Unsupported).unwrap();
        }
        let since: i64 = "2026-06-01T00:00:00Z".parse::<jiff::Timestamp>().unwrap().as_millisecond();
        let (pdf, photo, typed) = (pk_of("свежее", "pdf").attachment_pk, pk_of("свежее", "photo").attachment_pk, pk_of("свежее", "typed").attachment_pk);
        // В очереди — скачанные PDF и картинки свежего письма: не встроенная картинка, не документ
        // Word, не файл, которого нет на диске, и не вложение старого письма.
        let mut queue = store.ocr_queue(acc, since).unwrap();
        queue.sort_unstable();
        assert_eq!(queue, [pdf, photo, typed]);
        assert_eq!(store.ocr_queue_stats(acc, since).unwrap(), (3, 0));
        // Срок шире — попадает и старое письмо, после свежего.
        let wide = store.ocr_queue(acc, 0).unwrap();
        assert_eq!((wide.len(), wide.last().copied()), (4, Some(pk_of("старое", "old").attachment_pk)));

        // Разобранное в очередь не возвращается, и повторное скачивание файла отметку не снимает.
        store.mark_ocr_done(pdf).unwrap();
        store.save_attachment_pages(photo, 1, &[AttachmentPage { page: 1, source: PAGE_OCR, text: "Паспорт изделия".into() }], Some("Паспорт изделия")).unwrap();
        store.mark_ocr_done(photo).unwrap();
        store.save_attachment(&pk_of("свежее", "pdf").todo(), b"file", None, crate::AttachmentText::Unsupported).unwrap();
        assert_eq!(store.ocr_queue(acc, since).unwrap(), [typed]);
        assert_eq!(store.ocr_queue_stats(acc, since).unwrap(), (1, 1));
        // Страница распознана, а текста на ней нет (фотография без надписей) — не в счёт.
        store.save_attachment_pages(typed, 1, &[AttachmentPage { page: 1, source: PAGE_OCR, text: String::new() }], None).unwrap();
        assert_eq!(store.ocr_queue_stats(acc, since).unwrap(), (1, 1));
        // Удалённое письмо и чужая учётная запись в очередь не попадают.
        let other = store.account("h", "другой").unwrap();
        assert!(store.ocr_queue(other, 0).unwrap().is_empty());
    }
}
