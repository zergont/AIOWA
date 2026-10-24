//! Предпросмотр вложений в окне: собственный протокол Tauri `aiowa-att` и команда
//! `attachment_preview_url`.
//!
//! Окно получает от команды адрес `http://aiowa-att.localhost/<attachment_pk>` (на Windows
//! WebView2 показывает схему так) и подставляет его в `<img>`, `<iframe>` (PDF) или читает
//! `fetch` (текст). Протокол отдаёт только файлы кэша вложений: ключ — целое число в пути,
//! вложение должно принадлежать текущей учётной записи, а файл уже лежать в кэше (скачивание
//! выполняет команда, протокол в сеть не ходит), и тип должен быть из тех, что окно показывает
//! (`is_previewable`: XML, XHTML и прочие активные документы не отдаются). Всё остальное — 404.
//! Ответы без кэширования и с `nosniff`; HTML отдаётся как простой текст, а у всех ответов, кроме
//! PDF (его встроенному просмотрщику политика мешает), есть политика без скриптов и с `sandbox`,
//! так что даже прямой переход по адресу ничего не выполнит. Тексты ошибок команды — для пользователя.

use crate::ondemand::{ensure_cached, OnDemand};
use crate::sync::SyncState;
use crate::{err, AppState, CmdResult};
use aiowa_core::AccountId;
use aiowa_store::Store;
use serde::Serialize;
use std::sync::Arc;
use tauri::http::{header, Method, Response, StatusCode};
use tauri::State;
use tracing::{debug, warn};

/// Имя схемы в `register_asynchronous_uri_scheme_protocol`.
pub(crate) const SCHEME: &str = "aiowa-att";
/// Файл больше этого размера целиком в ответ не кладётся (ответ собирается в памяти).
const PREVIEW_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Политика ответов протокола (кроме PDF): без скриптов, внешних ресурсов и с `sandbox` — документ,
/// открытый по адресу напрямую, ничего не выполнит (SVG в `<img>` скрипты и так не исполняет).
const PREVIEW_POLICY: &str = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; sandbox";

/// Причина, по которой предпросмотра нет (`PreviewDto::reason`).
const REASON_TYPE: &str = "unsupported_type";
const REASON_SIZE: &str = "too_large";

/// Ответ команды: адрес файла в кэше и можно ли показывать его в окне.
#[derive(Debug, Serialize)]
pub(crate) struct PreviewDto {
    /// Адрес протокола `aiowa-att`; пусто, если предпросмотр недоступен.
    url: String,
    /// Тип, по которому окно выбирает способ показа (из записи вложения или по расширению).
    content_type: Option<String>,
    previewable: bool,
    /// Почему предпросмотра нет: `unsupported_type` — тип окно не показывает, `too_large` —
    /// файл больше лимита предпросмотра; `None`, если показывать можно.
    reason: Option<&'static str>,
    /// Можно ли открыть файл системным приложением («Открыть»); исполняемые типы — нет, только
    /// «Сохранить как…».
    openable: bool,
    name: Option<String>,
    size: Option<i64>,
}

/// Адрес вложения в протоколе: на Windows и Android схема живёт на поддомене `localhost`,
/// на остальных платформах — на хосте `localhost`.
pub(crate) fn preview_url(attachment_pk: i64) -> String {
    if cfg!(any(windows, target_os = "android")) { format!("http://{SCHEME}.localhost/{attachment_pk}") } else { format!("{SCHEME}://localhost/{attachment_pk}") }
}

/// Ключ вложения из пути запроса: ровно `/` и десятичное целое, без знака и лишнего.
pub(crate) fn parse_pk(path: &str) -> Option<i64> {
    let digits = path.strip_prefix('/')?;
    if digits.is_empty() || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// MIME-тип вложения для показа: из записи (без параметров, в нижнем регистре), а если его нет
/// или он общий (`application/octet-stream`) — по расширению имени.
pub(crate) fn resolved_type(name: Option<&str>, content_type: Option<&str>) -> String {
    let declared = content_type.map(|c| c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase()).filter(|c| !c.is_empty() && c != "application/octet-stream");
    declared.unwrap_or_else(|| crate::compose::content_type_for(name.unwrap_or_default()).to_string())
}

/// Типы, которые окно умеет показать: картинки, которые рисует WebView, PDF и простой текст
/// (CSV и HTML показываются как текст, без выполнения).
pub(crate) fn is_previewable(content_type: &str) -> bool {
    matches!(
        content_type,
        "image/png"
            | "image/jpeg"
            | "image/jpg"
            | "image/pjpeg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/svg+xml"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "image/avif"
            | "application/pdf"
            | "text/plain"
            | "text/csv"
            | "text/html"
    )
}

/// Content-Type ответа: HTML и CSV отдаются как простой текст (документ из письма не должен
/// отрисоваться и выполниться, если по адресу перейдут напрямую).
fn served_type(content_type: &str) -> &str {
    match content_type {
        "text/html" | "text/plain" | "text/csv" => "text/plain; charset=utf-8",
        "image/jpg" | "image/pjpeg" => "image/jpeg",
        other => other,
    }
}

fn builder(status: StatusCode) -> tauri::http::response::Builder {
    // `*` нужен для fetch из окна: его origin (tauri.localhost или адрес dev-сервера) отличается от
    // origin протокола. Отдаются только файлы кэша, а сторонних страниц в окне приложения нет.
    Response::builder().status(status).header(header::CACHE_CONTROL, "no-store").header(header::X_CONTENT_TYPE_OPTIONS, "nosniff").header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
}

/// Ответ протокола в режиме первого запуска: базы ещё нет.
pub(crate) fn unavailable() -> Response<Vec<u8>> {
    bare(StatusCode::SERVICE_UNAVAILABLE)
}

/// Ответ без файла: пустое тело и общие заголовки безопасности.
fn bare(status: StatusCode) -> Response<Vec<u8>> {
    builder(status).header(header::CONTENT_TYPE, "text/plain; charset=utf-8").body(Vec::new()).expect("статус и заголовки заданы константами")
}

/// Обработка запроса протокола. Блокирующая (диск и база): вызывать не из потока окна.
pub(crate) fn serve(store: &Store, account: AccountId, method: &Method, path: &str) -> Response<Vec<u8>> {
    if method == Method::OPTIONS {
        return builder(StatusCode::NO_CONTENT).header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD").body(Vec::new()).expect("статус и заголовки заданы константами");
    }
    if method != Method::GET && method != Method::HEAD {
        return bare(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Some(pk) = parse_pk(path) else { return bare(StatusCode::NOT_FOUND) };
    let row = match store.attachment_of_account(account, pk) {
        Ok(Some(row)) => row,
        Ok(None) => return bare(StatusCode::NOT_FOUND),
        Err(e) => {
            warn!(attachment_pk = pk, error = %e, "протокол предпросмотра: база не ответила");
            return bare(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // Только типы, которые окно показывает: та же проверка, что у команды. Прочее (XML, XHTML и
    // другие документы, которые браузер исполнил бы) протокол не отдаёт, даже если файл в кэше.
    let ct = resolved_type(row.name.as_deref(), row.content_type.as_deref());
    if !is_previewable(&ct) {
        return bare(StatusCode::NOT_FOUND);
    }
    // Только то, что уже в кэше: скачивает команда, протокол в сеть не ходит.
    let Some(file) = row.local_path.as_ref().filter(|p| p.is_file()) else { return bare(StatusCode::NOT_FOUND) };
    match std::fs::metadata(file) {
        Ok(m) if m.len() > PREVIEW_MAX_BYTES => return bare(StatusCode::PAYLOAD_TOO_LARGE),
        Ok(_) => {}
        Err(_) => return bare(StatusCode::NOT_FOUND),
    }
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => {
            warn!(attachment_pk = pk, error = %e, "протокол предпросмотра: файл кэша не прочитан");
            return bare(StatusCode::NOT_FOUND);
        }
    };
    if let Err(e) = store.touch_attachment(pk) {
        debug!(attachment_pk = pk, error = %e, "не удалось отметить обращение к вложению");
    }
    let safe_name = aiowa_store::safe_file_name(row.name.as_deref().unwrap_or("вложение"));
    let disposition = format!("inline; filename*=UTF-8''{}", percent_encoding::utf8_percent_encode(&safe_name, percent_encoding::NON_ALPHANUMERIC));
    let mut resp = builder(StatusCode::OK).header(header::CONTENT_TYPE, served_type(&ct)).header(header::CONTENT_DISPOSITION, disposition).header(header::CONTENT_LENGTH, bytes.len());
    if ct != "application/pdf" {
        resp = resp.header(header::CONTENT_SECURITY_POLICY, PREVIEW_POLICY);
    }
    resp.body(bytes).unwrap_or_else(|_| bare(StatusCode::INTERNAL_SERVER_ERROR))
}

/// Скачать вложение при необходимости и вернуть адрес для предпросмотра. Типы, которые окно не
/// показывает, и слишком большие файлы не скачиваются: `previewable: false`, окно предложит «Открыть».
#[tauri::command]
pub(crate) async fn attachment_preview_url(attachment_pk: i64, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>) -> CmdResult<PreviewDto> {
    let row = state.store.attachment_of_account(state.account, attachment_pk).map_err(err)?.ok_or_else(|| "Вложение не найдено".to_string())?;
    let ct = resolved_type(row.name.as_deref(), row.content_type.as_deref());
    let openable = !crate::ondemand::is_unsafe_to_open(row.name.as_deref());
    let mut dto = PreviewDto { url: String::new(), content_type: Some(ct.clone()), previewable: false, reason: None, openable, name: row.name.clone(), size: row.size };
    if !is_previewable(&ct) {
        dto.reason = Some(REASON_TYPE);
        return Ok(dto);
    }
    if row.size.is_some_and(|s| s > PREVIEW_MAX_BYTES as i64) {
        dto.reason = Some(REASON_SIZE);
        return Ok(dto);
    }
    let path = ensure_cached(&state, &sync, &od, attachment_pk).await?;
    // Размер скачанного файла точнее записи с сервера.
    let len = std::fs::metadata(&path).map(|m| m.len()).map_err(|_| "Файл вложения недоступен".to_string())?;
    dto.size = Some(len as i64);
    if len > PREVIEW_MAX_BYTES {
        dto.reason = Some(REASON_SIZE);
        return Ok(dto);
    }
    dto.url = preview_url(attachment_pk);
    dto.previewable = true;
    Ok(dto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{AttachmentMeta, BodyType, Folder, FolderId, Importance, ItemId, MessageBody, MessageFull, MessageMeta};
    use aiowa_store::AttachmentText;

    #[test]
    fn path_must_be_a_plain_integer() {
        assert_eq!(parse_pk("/42"), Some(42));
        assert_eq!(parse_pk("/0"), Some(0));
        assert_eq!(parse_pk("/123456789012345678"), Some(123_456_789_012_345_678));
        for bad in ["", "/", "42", "/-1", "/+1", "/1/2", "/1.pdf", "/1 ", "/ 1", "/../1", "/%31", "/١٢", "/1234567890123456789", "//1", "/0x10"] {
            assert_eq!(parse_pk(bad), None, "{bad:?} не должен разбираться");
        }
    }

    #[test]
    fn types_come_from_record_or_extension() {
        assert_eq!(resolved_type(Some("a.pdf"), Some("Application/PDF; name=a.pdf")), "application/pdf");
        assert_eq!(resolved_type(Some("снимок.JPG"), None), "image/jpeg");
        assert_eq!(resolved_type(Some("заметки.txt"), Some("application/octet-stream")), "text/plain");
        assert_eq!(resolved_type(Some("a.bin"), Some("")), "application/octet-stream");
        assert_eq!(resolved_type(None, None), "application/octet-stream");
        for ok in ["image/png", "image/jpeg", "image/svg+xml", "application/pdf", "text/plain", "text/csv", "text/html"] {
            assert!(is_previewable(ok), "{ok}");
        }
        for no in ["image/tiff", "image/heic", "application/zip", "application/octet-stream", "text/xml", "video/mp4", "application/vnd.ms-excel", ""] {
            assert!(!is_previewable(no), "{no}");
        }
        assert_eq!(served_type("text/html"), "text/plain; charset=utf-8", "HTML не отрисовывается");
        assert_eq!(served_type("text/csv"), "text/plain; charset=utf-8");
        assert_eq!(served_type("image/png"), "image/png");
        assert_eq!(served_type("image/pjpeg"), "image/jpeg");
    }

    #[test]
    fn url_points_at_the_scheme() {
        let u = preview_url(17);
        assert!(u.ends_with("/17") && u.contains(SCHEME), "{u}");
        if cfg!(windows) {
            assert_eq!(u, "http://aiowa-att.localhost/17");
        }
    }

    fn message(id: &str, attachments: Vec<AttachmentMeta>) -> MessageFull {
        MessageFull {
            meta: MessageMeta {
                item_id: ItemId { id: id.into(), change_key: Some("c".into()) },
                parent_folder_id: None,
                conversation_id: None,
                internet_message_id: None,
                item_class: Some("IPM.Note".into()),
                subject: Some("Тема".into()),
                from: None,
                sender: None,
                display_to: None,
                received_at: Some("2026-10-16T10:00:00Z".parse().unwrap()),
                sent_at: None,
                last_modified_at: None,
                is_read: false,
                is_draft: false,
                has_attachments: !attachments.is_empty(),
                importance: Importance::Normal,
                flag: Default::default(),
                size: Some(100),
                preview: None,
            },
            to: vec![],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            body: Some(MessageBody { body_type: BodyType::Html, unique_body: Some("<p>x</p>".into()), full_body: None, is_truncated: false }),
            attachments,
        }
    }

    fn meta(id: &str, name: &str, content_type: Option<&str>) -> AttachmentMeta {
        AttachmentMeta { id: id.into(), name: Some(name.into()), content_type: content_type.map(String::from), size: Some(5), is_inline: false, content_id: None }
    }

    #[test]
    fn protocol_serves_only_cached_files_of_the_account() {
        let dir = std::env::temp_dir().join(format!("aiowa-preview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("mail.db")).unwrap();
        let acc = store.account("h", "u").unwrap();
        let folder = Folder {
            id: FolderId { id: "F".into(), change_key: None },
            parent_id: None,
            display_name: "Входящие".into(),
            distinguished: Some("inbox".into()),
            folder_class: Some("IPF.Note".into()),
            total_count: 1,
            unread_count: 1,
            child_folder_count: 0,
        };
        store.upsert_folders(acc, &[folder]).unwrap();
        let (fpk, _) = store.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let msg = message(
            "M",
            vec![
                meta("a-html", "страница.html", Some("text/html")),
                meta("a-png", "рисунок.png", Some("image/png")),
                meta("a-none", "ещё не скачано.pdf", Some("application/pdf")),
                meta("a-pdf", "отчёт.pdf", Some("application/pdf")),
                meta("a-xml", "report.xml", None),
                meta("a-xhtml", "page.xhtml", Some("application/xhtml+xml")),
            ],
        );
        store.store_full_messages(acc, fpk, &[msg]).unwrap();
        let item_pk: i64 = store.debug_query("SELECT item_pk FROM items WHERE server_id = 'M'").unwrap()[1].parse().unwrap();
        let rows = store.attachments_of_item(item_pk).unwrap();
        let by = |sid: &str| rows.iter().find(|r| r.server_id == sid).unwrap();
        store.save_attachment(&by("a-html").todo(), b"<script>alert(1)</script>", Some("text/html"), AttachmentText::Unsupported).unwrap();
        store.save_attachment(&by("a-png").todo(), b"\x89PNG\r\n", Some("image/png"), AttachmentText::Unsupported).unwrap();
        store.save_attachment(&by("a-pdf").todo(), b"%PDF-1.4\n", Some("application/pdf"), AttachmentText::Unsupported).unwrap();
        let xhtml = br#"<html xmlns="http://www.w3.org/1999/xhtml"><script>alert(1)</script></html>"#;
        store.save_attachment(&by("a-xml").todo(), xhtml, None, AttachmentText::Unsupported).unwrap();
        store.save_attachment(&by("a-xhtml").todo(), xhtml, Some("application/xhtml+xml"), AttachmentText::Unsupported).unwrap();

        let get = |path: &str| serve(&store, acc, &Method::GET, path);

        // HTML отдаётся как текст; заголовки безопасности на месте.
        let r = get(&format!("/{}", by("a-html").attachment_pk));
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.body().as_slice(), b"<script>alert(1)</script>");
        let h = r.headers();
        assert_eq!(h[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        let disposition = h[header::CONTENT_DISPOSITION].to_str().unwrap();
        assert!(disposition.starts_with("inline; filename*=UTF-8''") && disposition.is_ascii(), "{disposition}");
        assert_eq!(h[header::CONTENT_SECURITY_POLICY], PREVIEW_POLICY);

        let r = get(&format!("/{}", by("a-png").attachment_pk));
        assert_eq!((r.status(), r.headers()[header::CONTENT_TYPE].to_str().unwrap()), (StatusCode::OK, "image/png"));
        assert!(PREVIEW_POLICY.contains("sandbox") && r.headers()[header::CONTENT_SECURITY_POLICY] == PREVIEW_POLICY);

        // PDF — без политики: встроенному просмотрщику она мешает.
        let r = get(&format!("/{}", by("a-pdf").attachment_pk));
        assert_eq!((r.status(), r.headers()[header::CONTENT_TYPE].to_str().unwrap()), (StatusCode::OK, "application/pdf"));
        assert!(r.headers().get(header::CONTENT_SECURITY_POLICY).is_none());

        // Активные документы в кэше (XML по расширению, XHTML по типу) протокол не отдаёт.
        assert_eq!(get(&format!("/{}", by("a-xml").attachment_pk)).status(), StatusCode::NOT_FOUND, "text/xml не предпросматривается");
        assert_eq!(get(&format!("/{}", by("a-xhtml").attachment_pk)).status(), StatusCode::NOT_FOUND, "application/xhtml+xml не предпросматривается");

        // Не в кэше, чужой аккаунт, чужие ключи, не число и не тот метод — не файл.
        assert_eq!(get(&format!("/{}", by("a-none").attachment_pk)).status(), StatusCode::NOT_FOUND, "нет файла в кэше");
        let other = store.account("h", "другой").unwrap();
        assert_eq!(serve(&store, other, &Method::GET, &format!("/{}", by("a-png").attachment_pk)).status(), StatusCode::NOT_FOUND, "вложение чужой учётной записи");
        assert_eq!(get("/999999").status(), StatusCode::NOT_FOUND);
        assert_eq!(get("/abc").status(), StatusCode::NOT_FOUND);
        assert_eq!(get(&format!("/{}/../{}", by("a-png").attachment_pk, by("a-png").attachment_pk)).status(), StatusCode::NOT_FOUND);
        assert_eq!(serve(&store, acc, &Method::POST, &format!("/{}", by("a-png").attachment_pk)).status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(serve(&store, acc, &Method::OPTIONS, "/1").status(), StatusCode::NO_CONTENT);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
