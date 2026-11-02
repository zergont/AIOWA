//! Вложенные письма `.eml` в окне (docs/obkatka.md п. 30). Письмо, пересланное «как вложение»,
//! сервер отдаёт целиком в MIME; файл лежит в кэше вложений, как любой другой. Окно показывает его
//! как письмо, как OWA: шапка (от, кому, копия, дата, тема), текст в той же изолированной рамке и
//! с той же очисткой, что у писем (картинки текста — `data:` из самого файла), и его вложения —
//! «Открыть» системным приложением или «Сохранить как…». Вложения, скачанные до п. 30, письмом не
//! помечены (имя-тема без расширения, тип `application/octet-stream`): их узнаёт содержимое
//! (`aiowa_extract::looks_like_message`).

use crate::ondemand::{ensure_cached, is_unsafe_to_open, OnDemand};
use crate::sync::SyncState;
use crate::{err, mail_dark, normalize_cid, sanitize_html, AppState, CmdResult};
use aiowa_extract::{MessagePerson, ParsedMessage};
use base64::Engine as _;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};
use tracing::warn;

/// Тип вложенного письма.
pub(crate) const MESSAGE_RFC822: &str = "message/rfc822";
/// Больше этого письмо в окне не разбирается (целиком в памяти).
const MESSAGE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Картинка текста больше этого в `data:` не превращается.
const INLINE_IMAGE_MAX: usize = 5 * 1024 * 1024;

/// Вложение — письмо: по типу записи, по имени `.eml` или, если тип общий, по содержимому файла.
pub(crate) fn is_message_file(name: Option<&str>, content_type: Option<&str>, path: &Path) -> bool {
    let declared = content_type.map(|c| c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase()).unwrap_or_default();
    if declared == MESSAGE_RFC822 || name.is_some_and(|n| n.trim().to_ascii_lowercase().ends_with(".eml")) {
        return true;
    }
    if !(declared.is_empty() || declared == "application/octet-stream") {
        return false;
    }
    let mut head = Vec::with_capacity(16 * 1024);
    std::fs::File::open(path).and_then(|f| f.take(16 * 1024).read_to_end(&mut head)).is_ok() && aiowa_extract::looks_like_message(&head)
}

#[derive(Debug, Serialize)]
pub(crate) struct EmbeddedPersonDto {
    name: Option<String>,
    email: Option<String>,
}

impl From<&MessagePerson> for EmbeddedPersonDto {
    fn from(p: &MessagePerson) -> Self {
        Self { name: p.name.clone(), email: p.email.clone() }
    }
}

/// Вложение вложенного письма: `index` — номер для `attachment_message_part`.
#[derive(Debug, Serialize)]
pub(crate) struct EmbeddedPartDto {
    index: usize,
    name: Option<String>,
    content_type: Option<String>,
    size: usize,
}

/// Вложенное письмо для окна.
#[derive(Debug, Serialize)]
pub(crate) struct EmbeddedMessageDto {
    subject: Option<String>,
    from: Vec<EmbeddedPersonDto>,
    to: Vec<EmbeddedPersonDto>,
    cc: Vec<EmbeddedPersonDto>,
    date_ms: Option<i64>,
    /// Очищенный HTML (картинки текста — `data:`), с тёмными тонами, как у писем.
    body_html: Option<String>,
    dark_colors: Option<Vec<String>>,
    dark_tag: Option<String>,
    attachments: Vec<EmbeddedPartDto>,
}

/// Файл вложения из кэша (скачать при необходимости), разобранный как письмо.
async fn parsed(state: &AppState, sync: &SyncState, od: &OnDemand, attachment_pk: i64) -> CmdResult<(PathBuf, ParsedMessage)> {
    state.store.attachment_of_account(state.account, attachment_pk).map_err(err)?.ok_or_else(|| "Вложение не найдено".to_string())?;
    let path = ensure_cached(state, sync, od, attachment_pk).await?;
    let file = path.clone();
    let message = tauri::async_runtime::spawn_blocking(move || -> CmdResult<ParsedMessage> {
        let len = std::fs::metadata(&file).map(|m| m.len()).map_err(|_| "Файл вложения недоступен".to_string())?;
        if len > MESSAGE_MAX_BYTES {
            return Err("Письмо слишком большое для просмотра: сохраните его".into());
        }
        let bytes = std::fs::read(&file).map_err(|_| "Файл вложения недоступен".to_string())?;
        aiowa_extract::parse_message(&bytes).ok_or_else(|| "Вложение не читается как письмо".to_string())
    })
    .await
    .map_err(err)??;
    Ok((path, message))
}

/// Картинки текста письма — адресами `data:` по нормализованному `cid`.
fn inline_images(m: &ParsedMessage) -> HashMap<String, String> {
    m.attachments
        .iter()
        .filter(|a| a.inline && a.bytes.len() <= INLINE_IMAGE_MAX)
        .filter_map(|a| {
            let ct = a.content_type.as_deref().filter(|c| c.starts_with("image/"))?;
            let cid = normalize_cid(a.content_id.as_deref()?);
            (!cid.is_empty()).then(|| (cid, format!("data:{ct};base64,{}", base64::engine::general_purpose::STANDARD.encode(&a.bytes))))
        })
        .collect()
}

/// Разобрать вложенное письмо для показа в окне.
#[tauri::command]
pub(crate) async fn attachment_message(attachment_pk: i64, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>) -> CmdResult<EmbeddedMessageDto> {
    let (_, m) = parsed(&state, &sync, &od, attachment_pk).await?;
    let inline = Arc::new(inline_images(&m));
    let (body_html, dark_colors, dark_tag) = match m.html.as_deref().map(|h| mail_dark::darken(&sanitize_html(h, &inline))) {
        Some(d) => (Some(d.html), Some(d.colors), Some(d.tag)),
        None => (None, None, None),
    };
    let people = |ps: &[MessagePerson]| ps.iter().map(EmbeddedPersonDto::from).collect::<Vec<_>>();
    Ok(EmbeddedMessageDto {
        subject: m.subject.clone(),
        from: people(&m.from),
        to: people(&m.to),
        cc: people(&m.cc),
        date_ms: m.date_ms,
        body_html,
        dark_colors,
        dark_tag,
        attachments: m.attachments.iter().enumerate().filter(|(_, a)| !a.inline).map(|(index, a)| EmbeddedPartDto { index, name: a.name.clone(), content_type: a.content_type.clone(), size: a.bytes.len() }).collect(),
    })
}

/// Вложение вложенного письма: `save = false` — открыть системным приложением (файл кладётся рядом
/// с кэшем вложений, с меткой зоны «Интернет»; исполняемые типы не открываются), `save = true` —
/// «Сохранить как…» (`None` — отмена). Возвращает путь сохранённой копии.
#[tauri::command]
pub(crate) async fn attachment_message_part(attachment_pk: i64, index: usize, save: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>) -> CmdResult<Option<String>> {
    let (path, m) = parsed(&state, &sync, &od, attachment_pk).await?;
    let part = m.attachments.into_iter().nth(index).ok_or_else(|| "Вложение письма не найдено".to_string())?;
    let name = aiowa_store::safe_file_name(part.name.as_deref().unwrap_or("вложение"));
    let target = if save {
        let mut dialog = rfd::AsyncFileDialog::new().set_title("Сохранить вложение").set_file_name(&name);
        if let Some(dir) = dirs::download_dir() {
            dialog = dialog.set_directory(dir);
        }
        if let Some(window) = app.get_webview_window("main") {
            dialog = dialog.set_parent(&window);
        }
        let Some(chosen) = dialog.save_file().await else { return Ok(None) };
        chosen.path().to_path_buf()
    } else {
        if is_unsafe_to_open(Some(&name)) {
            return Err("Файлы такого типа открывать небезопасно — используйте «Сохранить как…»".into());
        }
        let dir = path.parent().ok_or_else(|| "Папка кэша вложений недоступна".to_string())?;
        dir.join(format!("{attachment_pk}-{index}-{name}"))
    };
    let written = target.clone();
    tauri::async_runtime::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::write(&written, &part.bytes)?;
        if let Err(e) = aiowa_store::mark_from_internet(&written) {
            warn!(error = %e, "не удалось поставить метку Zone.Identifier на вложение письма");
        }
        Ok(())
    })
    .await
    .map_err(err)?
    .map_err(|e| {
        warn!(attachment_pk, index, error = %e, "вложение письма не записано");
        "Не удалось записать файл: проверьте путь и права доступа".to_string()
    })?;
    if save {
        return Ok(Some(target.display().to_string()));
    }
    tauri::async_runtime::spawn_blocking(move || open::that_detached(&target)).await.map_err(err)?.map_err(|e| {
        warn!(attachment_pk, index, error = %e, "системное приложение не открыло вложение письма");
        "Не удалось открыть файл системным приложением".to_string()
    })?;
    Ok(None)
}
