//! Запросы только на чтение для интерфейса: страницы бесед, письма беседы, тело, получатели.
//! Пагинация keyset: `(last_received_at, conversation_pk)` строго убывают, OFFSET не используется.

use crate::{decompress, ms_ts, ConversationRow, Result, Store, HAS_FLAGGED_SQL};
use aiowa_core::{AccountId, BodyType, Participant, RecipientKind};
use jiff::Timestamp;
use rusqlite::{params, Connection, OptionalExtension};

/// Заголовок письма для области чтения.
#[derive(Debug, Clone)]
pub struct MessageHead {
    pub item_pk: i64,
    pub folder_pk: i64,
    pub folder_name: Option<String>,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub display_to: Option<String>,
    pub received_at: Option<Timestamp>,
    pub sent_at: Option<Timestamp>,
    pub is_read: bool,
    pub is_draft: bool,
    pub has_attachments: bool,
    pub importance: Option<String>,
    pub preview: Option<String>,
    /// 0 тела нет, 1 есть, 2 загрузка не удалась.
    pub body_state: i64,
    /// Флаг «к исполнению» поставлен (`FlagStatus = Flagged`).
    pub is_flagged: bool,
    /// Класс письма: `IPM.Note`, приглашение `IPM.Schedule.Meeting.Request`, отмена
    /// `…Canceled`, ответ участника `…Resp.Pos`/`Neg`/`Tent`.
    pub item_class: Option<String>,
    /// Адреса `Reply-To` письма (`recipients.kind = 4`), как прислал сервер; пусто без заголовка
    /// или пока тело не загружено. Что из них ставить в «Кому» ответа — `aiowa_core::effective_reply_to`.
    pub reply_to: Vec<Participant>,
}

/// Тело письма для показа.
#[derive(Debug, Clone)]
pub struct BodyView {
    pub body_type: BodyType,
    /// Текст без цитируемой истории (UniqueBody), как пришёл с сервера, без очистки.
    pub unique: Option<String>,
    /// Полное тело, если загружалось.
    pub full: Option<String>,
    pub text: Option<String>,
    pub is_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct RecipientRow {
    /// 1 to, 2 cc, 3 bcc, 4 Reply-To (`RecipientKind`).
    pub kind: i64,
    pub name: Option<String>,
    pub email: Option<String>,
}

/// Курсор страницы: последняя строка предыдущей страницы.
#[derive(Debug, Clone, Copy)]
pub struct ConversationCursor {
    pub last_received_at: i64,
    pub conversation_pk: i64,
}

/// Ключ состояния письма в беседе: по нему окно решает, перечитывать ли беседу целиком
/// после синхронизации. Дёшево: один запрос без тел, вложений и файлов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageKey {
    pub item_pk: i64,
    pub body_state: i64,
    pub is_read: bool,
    /// Вложений с файлом в кэше (по базе, без проверки диска).
    pub cached: i64,
    pub is_flagged: bool,
}

/// Выборка заголовка письма в порядке, который разбирает `head_row`.
const HEAD_SELECT: &str = "SELECT i.item_pk, i.folder_pk, f.display_name, i.subject, i.from_name, i.from_email, i.display_to, i.received_at, i.sent_at,
        i.is_read, i.is_draft, i.has_attachments, i.importance, i.preview, i.body_state, i.flag_status = 'Flagged', i.item_class
 FROM items i LEFT JOIN folders f ON f.folder_pk = i.folder_pk";

fn head_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageHead> {
    Ok(MessageHead {
        item_pk: r.get(0)?,
        folder_pk: r.get(1)?,
        folder_name: r.get(2)?,
        subject: r.get(3)?,
        from_name: r.get(4)?,
        from_email: r.get(5)?,
        display_to: r.get(6)?,
        received_at: ms_ts(r.get(7)?),
        sent_at: ms_ts(r.get(8)?),
        is_read: r.get::<_, i64>(9)? != 0,
        is_draft: r.get::<_, i64>(10)? != 0,
        has_attachments: r.get::<_, i64>(11)? != 0,
        importance: r.get(12)?,
        preview: r.get(13)?,
        body_state: r.get(14)?,
        is_flagged: r.get::<_, Option<i64>>(15)?.unwrap_or(0) != 0,
        item_class: r.get(16)?,
        reply_to: Vec::new(),
    })
}

/// Дописывает заголовкам адреса `Reply-To` из `recipients` (kind 4) отдельным запросом на письмо:
/// заголовков в беседе десятки, строк Reply-To почти никогда нет.
fn fill_reply_to(c: &Connection, heads: &mut [MessageHead]) -> rusqlite::Result<()> {
    let mut st = c.prepare_cached("SELECT name, email FROM recipients WHERE item_pk = ?1 AND kind = ?2 ORDER BY position")?;
    for h in heads {
        h.reply_to = st
            .query_map(params![h.item_pk, RecipientKind::ReplyTo.as_i64()], |r| Ok(Participant { name: r.get(0)?, email: r.get(1)?, routing_type: None }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(())
}

impl Store {
    /// Страница бесед папки, свежие первыми; `after` — курсор предыдущей страницы.
    pub fn conversations_page(&self, account: AccountId, folder_pk: i64, after: Option<ConversationCursor>, limit: usize) -> Result<Vec<ConversationRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!(
                "SELECT c.conversation_pk, c.topic, cf.item_count, cf.unread_count, c.item_count, cf.last_received_at, c.has_attachments, {HAS_FLAGGED_SQL}
                 FROM conversation_folders cf JOIN conversations c ON c.conversation_pk = cf.conversation_pk
                 WHERE cf.folder_pk = ?1 AND c.account_id = ?2
                   AND (?3 IS NULL OR cf.last_received_at < ?3 OR (cf.last_received_at = ?3 AND cf.conversation_pk < ?4))
                 ORDER BY cf.last_received_at DESC, cf.conversation_pk DESC LIMIT ?5"
            ))?;
            let rows: Vec<ConversationRow> = st
                .query_map(params![folder_pk, account.0, after.map(|a| a.last_received_at), after.map(|a| a.conversation_pk).unwrap_or(i64::MAX), limit as i64], |r| {
                    Ok(ConversationRow {
                        conversation_pk: r.get(0)?,
                        topic: r.get(1)?,
                        participants: Vec::new(),
                        item_count: r.get(2)?,
                        unread_count: r.get(3)?,
                        total_count: r.get(4)?,
                        last_received_at: ms_ts(r.get::<_, Option<i64>>(5)?),
                        has_attachments: r.get::<_, i64>(6)? != 0,
                        has_flagged: r.get::<_, i64>(7)? != 0,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut people = c.prepare_cached(
                "SELECT COALESCE(from_name, from_email) FROM items WHERE conversation_pk = ?1 AND deleted = 0
                 ORDER BY COALESCE(received_at, sent_at) DESC LIMIT 30",
            )?;
            let mut out = Vec::with_capacity(rows.len());
            for mut row in rows {
                let names = people.query_map(params![row.conversation_pk], |r| r.get::<_, Option<String>>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
                let mut seen: Vec<String> = Vec::new();
                for n in names.into_iter().flatten() {
                    if !n.is_empty() && !seen.contains(&n) {
                        seen.push(n);
                    }
                }
                row.participants = seen;
                out.push(row);
            }
            Ok(out)
        })
    }

    /// Письма беседы во всех папках, новые первыми (как в OWA: свежий ответ сверху).
    pub fn conversation_messages(&self, account: AccountId, conversation_pk: i64) -> Result<Vec<MessageHead>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!(
                "{HEAD_SELECT} WHERE i.conversation_pk = ?1 AND i.account_id = ?2 AND i.deleted = 0
                 ORDER BY COALESCE(i.received_at, i.sent_at) DESC, i.item_pk DESC"
            ))?;
            let rows = st.query_map(params![conversation_pk, account.0], head_row)?;
            let mut heads = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            fill_reply_to(c, &mut heads)?;
            Ok(heads)
        })
    }

    /// Ключи писем беседы в том же порядке, что `conversation_messages`.
    pub fn conversation_keys(&self, account: AccountId, conversation_pk: i64) -> Result<Vec<MessageKey>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT i.item_pk, i.body_state, i.is_read,
                        (SELECT COUNT(*) FROM attachments a WHERE a.item_pk = i.item_pk AND a.local_path IS NOT NULL),
                        i.flag_status = 'Flagged'
                 FROM items i
                 WHERE i.conversation_pk = ?1 AND i.account_id = ?2 AND i.deleted = 0
                 ORDER BY COALESCE(i.received_at, i.sent_at) DESC, i.item_pk DESC",
            )?;
            let rows = st.query_map(params![conversation_pk, account.0], |r| {
                Ok(MessageKey { item_pk: r.get(0)?, body_state: r.get(1)?, is_read: r.get::<_, i64>(2)? != 0, cached: r.get(3)?, is_flagged: r.get::<_, Option<i64>>(4)?.unwrap_or(0) != 0 })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Заголовок одного не удалённого письма (для области чтения после загрузки тела).
    pub fn message_head(&self, item_pk: i64) -> Result<Option<MessageHead>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("{HEAD_SELECT} WHERE i.item_pk = ?1 AND i.deleted = 0"))?;
            let mut head = st.query_row(params![item_pk], head_row).optional()?;
            if let Some(h) = head.as_mut() {
                fill_reply_to(c, std::slice::from_mut(h))?;
            }
            Ok(head)
        })
    }

    /// Тело письма, если загружено.
    pub fn message_body(&self, item_pk: i64) -> Result<Option<BodyView>> {
        self.with_conn(|c| {
            let row = c
                .query_row("SELECT body_type, unique_body, full_body, unique_text, is_truncated FROM bodies WHERE item_pk = ?1", params![item_pk], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?, r.get::<_, Option<Vec<u8>>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?))
                })
                .optional()?;
            let Some((body_type, unique, full, text, truncated)) = row else { return Ok(None) };
            Ok(Some(BodyView {
                body_type: BodyType::parse(&body_type),
                unique: unique.as_deref().map(decompress).transpose()?,
                full: full.as_deref().map(decompress).transpose()?,
                text,
                is_truncated: truncated != 0,
            }))
        })
    }

    pub fn recipients(&self, item_pk: i64) -> Result<Vec<RecipientRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached("SELECT kind, name, email FROM recipients WHERE item_pk = ?1 ORDER BY kind, position")?;
            let rows = st.query_map(params![item_pk], |r| Ok(RecipientRow { kind: r.get(0)?, name: r.get(1)?, email: r.get(2)? }))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Папка, в которой лежит письмо (для беседы, собранной из нескольких папок).
    pub fn folder_name(&self, folder_pk: i64) -> Result<Option<String>> {
        self.with_conn(|c| Ok(c.query_row("SELECT display_name FROM folders WHERE folder_pk = ?1", params![folder_pk], |r| r.get(0)).optional()?))
    }
}
