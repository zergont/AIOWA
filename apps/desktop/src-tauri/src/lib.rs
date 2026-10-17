//! AIOWA desktop: команды Tauri поверх локальной базы. Только чтение: синхронизацию
//! делает `aiowa sync`, интерфейс показывает то, что уже лежит в SQLite.
//!
//! HTML писем очищается ammonia на стороне Rust и показывается во фрейме с sandbox и CSP;
//! удалённые картинки и внешние ресурсы не загружаются.

use aiowa_core::{AccountId, BodyType};
use aiowa_store::ui::ConversationCursor;
use aiowa_store::Store;
use jiff::Timestamp;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::State;

struct AppState {
    store: Arc<Store>,
    account: AccountId,
    host: String,
    user: String,
}

type CmdResult<T> = std::result::Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn ms(t: Option<Timestamp>) -> Option<i64> {
    t.map(|t| t.as_millisecond())
}

#[derive(Serialize)]
struct AccountDto {
    host: String,
    user: String,
    db_path: String,
}

#[derive(Serialize)]
struct FolderDto {
    folder_pk: i64,
    parent_pk: Option<i64>,
    name: String,
    distinguished: Option<String>,
    total: i64,
    unread: i64,
    is_mail: bool,
}

#[derive(Serialize)]
struct ConversationDto {
    conversation_pk: i64,
    topic: Option<String>,
    participants: Vec<String>,
    item_count: i64,
    unread_count: i64,
    total_count: i64,
    last_received_ms: Option<i64>,
    has_attachments: bool,
}

#[derive(Serialize)]
struct RecipientDto {
    kind: i64,
    name: Option<String>,
    email: Option<String>,
}

#[derive(Serialize)]
struct AttachmentDto {
    attachment_pk: i64,
    name: Option<String>,
    content_type: Option<String>,
    size: Option<i64>,
    is_inline: bool,
    cached: bool,
}

#[derive(Serialize)]
struct MessageDto {
    item_pk: i64,
    folder_pk: i64,
    folder_name: Option<String>,
    subject: Option<String>,
    from_name: Option<String>,
    from_email: Option<String>,
    display_to: Option<String>,
    received_ms: Option<i64>,
    sent_ms: Option<i64>,
    is_read: bool,
    is_draft: bool,
    has_attachments: bool,
    importance: Option<String>,
    preview: Option<String>,
    body_state: i64,
    /// Очищенный HTML тела без цитируемой истории; `None`, если тело не загружено.
    body_html: Option<String>,
    body_is_text: bool,
    is_truncated: bool,
    recipients: Vec<RecipientDto>,
    attachments: Vec<AttachmentDto>,
}

#[derive(Serialize)]
struct SearchHitDto {
    item_pk: i64,
    conversation_pk: Option<i64>,
    subject: Option<String>,
    from_name: Option<String>,
    from_email: Option<String>,
    received_ms: Option<i64>,
    folder_name: Option<String>,
    snippet: String,
    attachment_name: Option<String>,
}

/// Очистка HTML письма. Инлайновые стили разрешены: без них деловая почта нечитаема,
/// а внешние ресурсы из CSS всё равно блокирует CSP фрейма.
fn sanitize_html(html: &str) -> String {
    let mut b = ammonia::Builder::default();
    b.add_generic_attributes(["style", "width", "height", "align", "valign", "bgcolor", "border", "cellpadding", "cellspacing", "color", "face", "size"])
        .add_tags(["font", "center", "u", "s", "strike", "tt", "big", "small", "caption", "colgroup", "col", "thead", "tbody", "tfoot", "span", "div", "img", "table", "tr", "td", "th", "hr"])
        .link_rel(Some("noopener noreferrer"))
        .url_schemes(["http", "https", "mailto", "cid", "data"].into_iter().collect());
    b.clean(html).to_string()
}

fn text_to_html(text: &str) -> String {
    let escaped = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<pre style=\"white-space:pre-wrap;font:inherit;margin:0\">{escaped}</pre>")
}

#[tauri::command]
fn account_info(state: State<'_, AppState>) -> AccountDto {
    AccountDto { host: state.host.clone(), user: state.user.clone(), db_path: state.store.path().display().to_string() }
}

#[tauri::command]
fn folders(state: State<'_, AppState>) -> CmdResult<Vec<FolderDto>> {
    let list = state.store.list_folders(state.account).map_err(err)?;
    let by_server: HashMap<&str, i64> = list.iter().map(|(pk, f)| (f.id.id.as_str(), *pk)).collect();
    Ok(list
        .iter()
        .map(|(pk, f)| FolderDto {
            folder_pk: *pk,
            parent_pk: f.parent_id.as_ref().and_then(|p| by_server.get(p.id.as_str()).copied()),
            name: f.display_name.clone(),
            distinguished: f.distinguished.clone(),
            total: f.total_count,
            unread: f.unread_count,
            is_mail: f.is_mail(),
        })
        .collect())
}

#[tauri::command]
fn conversations(state: State<'_, AppState>, folder_pk: i64, after_ms: Option<i64>, after_pk: Option<i64>, limit: Option<usize>) -> CmdResult<Vec<ConversationDto>> {
    let cursor = match (after_ms, after_pk) {
        (Some(last_received_at), Some(conversation_pk)) => Some(ConversationCursor { last_received_at, conversation_pk }),
        _ => None,
    };
    let rows = state.store.conversations_page(state.account, folder_pk, cursor, limit.unwrap_or(100).clamp(1, 500)).map_err(err)?;
    Ok(rows
        .into_iter()
        .map(|r| ConversationDto {
            conversation_pk: r.conversation_pk,
            topic: r.topic,
            participants: r.participants,
            item_count: r.item_count,
            unread_count: r.unread_count,
            total_count: r.total_count,
            last_received_ms: ms(r.last_received_at),
            has_attachments: r.has_attachments,
        })
        .collect())
}

#[tauri::command]
fn conversation(state: State<'_, AppState>, conversation_pk: i64) -> CmdResult<Vec<MessageDto>> {
    let heads = state.store.conversation_messages(state.account, conversation_pk).map_err(err)?;
    let mut out = Vec::with_capacity(heads.len());
    for h in heads {
        let body = state.store.message_body(h.item_pk).map_err(err)?;
        let (body_html, body_is_text, is_truncated) = match body {
            Some(b) => {
                let raw = b.unique.or(b.full);
                match (raw, b.body_type) {
                    (Some(html), BodyType::Html) => (Some(sanitize_html(&html)), false, b.is_truncated),
                    (Some(text), BodyType::Text) => (Some(text_to_html(&text)), true, b.is_truncated),
                    (None, _) => (b.text.as_deref().map(text_to_html), true, b.is_truncated),
                }
            }
            None => (None, false, false),
        };
        let recipients = state.store.recipients(h.item_pk).map_err(err)?.into_iter().map(|r| RecipientDto { kind: r.kind, name: r.name, email: r.email }).collect();
        let attachments = state
            .store
            .attachments_of_item(h.item_pk)
            .map_err(err)?
            .into_iter()
            .map(|a| AttachmentDto { attachment_pk: a.attachment_pk, name: a.name, content_type: a.content_type, size: a.size, is_inline: a.is_inline, cached: a.local_path.is_some() })
            .collect();
        out.push(MessageDto {
            item_pk: h.item_pk,
            folder_pk: h.folder_pk,
            folder_name: h.folder_name,
            subject: h.subject,
            from_name: h.from_name,
            from_email: h.from_email,
            display_to: h.display_to,
            received_ms: ms(h.received_at),
            sent_ms: ms(h.sent_at),
            is_read: h.is_read,
            is_draft: h.is_draft,
            has_attachments: h.has_attachments,
            importance: h.importance,
            preview: h.preview,
            body_state: h.body_state,
            body_html,
            body_is_text,
            is_truncated,
            recipients,
            attachments,
        });
    }
    Ok(out)
}

#[tauri::command]
fn search(state: State<'_, AppState>, query: String, limit: Option<usize>) -> CmdResult<Vec<SearchHitDto>> {
    let hits = state.store.search(state.account, &query, limit.unwrap_or(50).clamp(1, 200)).map_err(err)?;
    let mut out = Vec::with_capacity(hits.len());
    for h in hits {
        out.push(SearchHitDto {
            item_pk: h.item_pk,
            conversation_pk: h.conversation_pk,
            subject: h.subject,
            from_name: h.from_name,
            from_email: h.from_email,
            received_ms: ms(h.received_at),
            folder_name: h.folder_name,
            snippet: h.snippet,
            attachment_name: h.attachment_name,
        });
    }
    Ok(out)
}

fn default_db_path(host: &str, user: &str) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let safe_user: String = user.chars().map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect();
    base.join("AIOWA").join(host).join(safe_user).join("mail.db")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))).try_init();
    let host = std::env::var("AIOWA_HOST").unwrap_or_default();
    let user = std::env::var("AIOWA_USER").unwrap_or_default();
    let db_path = std::env::var_os("AIOWA_DB").map(PathBuf::from).unwrap_or_else(|| default_db_path(&host, &user));
    let store = Store::open(&db_path).expect("не удалось открыть локальную базу");
    let account = store.account(&host, &user).expect("учётная запись");
    let state = AppState { store: Arc::new(store), account, host, user };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![account_info, folders, conversations, conversation, search])
        .run(tauri::generate_context!())
        .expect("ошибка запуска приложения");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizer_keeps_layout_and_drops_scripts() {
        let html = r##"<div style="color:red"><script>alert(1)</script><a href="javascript:x" onclick="y">ссылка</a><img src="https://t.example/p.gif"><table><tr><td bgcolor="#eee">ячейка</td></tr></table></div>"##;
        let out = sanitize_html(html);
        assert!(!out.contains("script"));
        assert!(!out.contains("onclick"));
        assert!(!out.contains("javascript:"));
        assert!(out.contains("style=\"color:red\""));
        assert!(out.contains("bgcolor"));
        assert!(out.contains("<img"));
    }
}
