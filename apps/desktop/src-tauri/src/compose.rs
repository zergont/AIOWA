//! Команды композера: заготовка письма (получатели и тема по виду: новое, ответ, ответ всем,
//! пересылка), отправка и сохранение черновика, подсказки адресов, подпись, выбор файлов.
//!
//! Отправка идёт через транспорт процесса (`SyncState::transport`, без новых входов, если сессия
//! уже есть) в три шага: `CreateItem` (черновик в «Черновиках») → `CreateAttachment` по файлу →
//! `SendItem`. Каждый шаг под своим таймаутом; отказ после создания черновика не ошибка команды,
//! а `SendReport { draft_saved: true }` с текстом для пользователя: черновик остался на сервере,
//! и повторная попытка из окна создаст новый. Мёртвая сессия сбрасывает транспорт через
//! `SyncState::invalidate_on`. Один замок на отправку и сохранение черновика: второй вызов,
//! пока идёт первый (двойной Ctrl+Enter), отклоняется сразу, а не ждёт и не шлёт письмо дважды.
//!
//! Тело письма перед отправкой проходит ту же очистку ammonia, что и показ писем: во вставленном
//! из буфера HTML не должно уйти скриптов и обработчиков. Цитату оригинала (и вложения оригинала
//! у пересылки) добавляет сервер, клиент передаёт только новую часть.

use crate::ondemand::{describe_transport, timed_out};
use crate::sync::SyncState;
use crate::{build_message, err, AppState, CmdResult, ComposeRecipientDto, InlineMap, MessageDto, ParticipantDto, RecipientDto};
use aiowa_core::{AccountId, ComposeKind, Importance, OutgoingAttachment, OutgoingMessage, Participant};
use aiowa_store::Store;
use aiowa_transport::MailTransport;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tracing::{info, warn};

/// Таймаут создания черновика (`CreateItem`) и обновления подписи.
const CREATE_TIMEOUT: Duration = Duration::from_secs(60);
/// Таймаут загрузки всех вложений (`CreateAttachment` по одному файлу, до 35 МиБ в base64).
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Таймаут отправки (`SendItem`).
const SEND_TIMEOUT: Duration = Duration::from_secs(60);
/// Лимит одного файла вложения.
pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// Лимит всех вложений письма вместе.
pub(crate) const MAX_ATTACHMENTS_TOTAL_BYTES: u64 = 35 * 1024 * 1024;
/// Подсказок адресов по умолчанию и максимум.
const SUGGEST_DEFAULT: usize = 10;
const SUGGEST_MAX: usize = 50;

/// Ключи подписи в `sync_state`.
const SIGNATURE_HTML_KEY: &str = "desktop.signature_html";
const SIGNATURE_AUTO_KEY: &str = "desktop.signature_auto";

/// Текст для окна, когда черновик создан, а дальше не вышло.
const DRAFT_KEPT: &str = "Письмо не отправлено, черновик сохранён на сервере в «Черновиках»";

/// Состояние композера: один замок на отправку и сохранение черновика.
#[derive(Default)]
pub(crate) struct ComposeState {
    sending: tokio::sync::Mutex<()>,
}

/// Вид письма, как его называет окно.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ComposeKindDto {
    New,
    Reply,
    ReplyAll,
    Forward,
}

impl ComposeKindDto {
    fn needs_reference(self) -> bool {
        !matches!(self, Self::New)
    }
}

/// Подпись для окна и настроек.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct SignatureDto {
    html: String,
    auto_add: bool,
}

/// Заготовка композера: получатели и тема подобраны по виду письма, оригинал — для показа
/// под редактором (только чтение), собственные адреса — чтобы окно не предлагало их в «ответить всем».
#[derive(Serialize)]
pub(crate) struct ComposeContext {
    kind: ComposeKindDto,
    reference_item_pk: Option<i64>,
    to: Vec<ComposeRecipientDto>,
    cc: Vec<ComposeRecipientDto>,
    subject: String,
    original: Option<MessageDto>,
    own_addresses: Vec<String>,
    signature: SignatureDto,
}

/// Письмо из окна: получатели, тема, HTML редактора, пути файлов-вложений.
#[derive(Debug, Deserialize)]
pub(crate) struct ComposeRequest {
    kind: ComposeKindDto,
    reference_item_pk: Option<i64>,
    #[serde(default)]
    to: Vec<ComposeRecipientDto>,
    #[serde(default)]
    cc: Vec<ComposeRecipientDto>,
    #[serde(default)]
    bcc: Vec<ComposeRecipientDto>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body_html: String,
    #[serde(default)]
    attachments: Vec<String>,
}

/// Итог отправки или сохранения черновика.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct SendReport {
    sent: bool,
    draft_saved: bool,
    /// Исход `SendItem` неизвестен (ответа нет или связь оборвалась после отправки запроса):
    /// письмо могло уйти, повтор из окна дал бы дубль адресатам.
    maybe_sent: bool,
    /// Текст для пользователя, если что-то не вышло после создания черновика.
    message: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct RecipientSuggestionDto {
    name: Option<String>,
    email: String,
    weight: i64,
}

/// Файл из диалога выбора вложений.
#[derive(Serialize)]
pub(crate) struct PickedFile {
    path: String,
    name: String,
    size: u64,
}

// --- чистые функции (без базы и сети) --------------------------------------------------------

/// Похоже ли на почтовый адрес: одно `@`, непустая локальная часть, домен с точкой, без пробелов
/// и разделителей списка. Строже не нужно: сервер сам отвергнет неизвестного адресата.
pub(crate) fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    let Some((local, domain)) = s.split_once('@') else { return false };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !s.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | ',' | ';' | '(' | ')' | '\\' | '/'))
}

/// Тема ответа или пересылки: префикс `RE:`/`FW:` добавляется, только если темы с таким же по
/// смыслу префиксом (в любом регистре, в русском написании) ещё нет. Ответ на пересылку даёт
/// `RE: FW: …`, как в OWA. Для нового письма тема возвращается как есть.
pub(crate) fn subject_with_prefix(kind: ComposeKindDto, original: Option<&str>) -> String {
    const REPLY: &[&str] = &["re:", "ответ:", "отв:"];
    const FORWARD: &[&str] = &["fw:", "fwd:", "пересл:", "пер:"];
    let subject = original.unwrap_or_default().trim();
    let (prefix, known) = match kind {
        ComposeKindDto::New => return subject.to_string(),
        ComposeKindDto::Reply | ComposeKindDto::ReplyAll => ("RE:", REPLY),
        ComposeKindDto::Forward => ("FW:", FORWARD),
    };
    let lower = subject.to_lowercase();
    if known.iter().any(|p| lower.starts_with(p)) {
        return subject.to_string();
    }
    if subject.is_empty() { prefix.to_string() } else { format!("{prefix} {subject}") }
}

/// Получатель для композера из имени и адреса письма: только с адресом SMTP (с `@`); устаревшие
/// адреса EX без `@` окно всё равно не пропустит, а сервер по ним письмо из клиента не примет.
fn compose_recipient(name: Option<&str>, email: Option<&str>) -> Option<ComposeRecipientDto> {
    let email = email?.trim();
    if !email.contains('@') {
        return None;
    }
    let name = name.map(str::trim).filter(|n| !n.is_empty() && *n != email).map(String::from);
    Some(ComposeRecipientDto { name, email: email.to_string() })
}

/// Получатели ответа: «ответить» — отправитель оригинала; «ответить всем» — отправитель и все
/// «Кому» в поле Кому, все «Копия» в поле Копия, без собственных адресов (`own`, в нижнем регистре)
/// и без повторов (без учёта регистра). Если после исключения своих адресов в «Кому» никого не
/// осталось (ответ на собственное письмо), туда возвращается отправитель. Для нового письма и
/// пересылки — пусто.
///
/// `reply_to` — адреса `Reply-To` письма, уже отфильтрованные (`MessageDto::reply_to`: только
/// если заголовок есть и отличается от отправителя): тогда вместо отправителя в «Кому» идут они,
/// как делают OWA и Outlook; сам отправитель в «ответить всем» не добавляется.
pub(crate) fn reply_recipients(kind: ComposeKindDto, from_name: Option<&str>, from_email: Option<&str>, reply_to: &[ParticipantDto], recipients: &[RecipientDto], own: &[String]) -> (Vec<ComposeRecipientDto>, Vec<ComposeRecipientDto>) {
    let senders: Vec<ComposeRecipientDto> = if reply_to.is_empty() {
        compose_recipient(from_name, from_email).into_iter().collect()
    } else {
        reply_to.iter().filter_map(|p| compose_recipient(p.name.as_deref(), p.email.as_deref())).collect()
    };
    match kind {
        ComposeKindDto::New | ComposeKindDto::Forward => (vec![], vec![]),
        ComposeKindDto::Reply => (senders, vec![]),
        ComposeKindDto::ReplyAll => {
            let mut seen: HashSet<String> = own.iter().map(|a| a.trim().to_lowercase()).collect();
            let mut pick = |r: Option<ComposeRecipientDto>| r.filter(|r| seen.insert(r.email.to_lowercase()));
            let mut to: Vec<ComposeRecipientDto> = senders.iter().cloned().filter_map(|s| pick(Some(s))).collect();
            to.extend(recipients.iter().filter(|r| r.kind == 1).filter_map(|r| pick(compose_recipient(r.name.as_deref(), r.email.as_deref()))));
            let cc: Vec<ComposeRecipientDto> = recipients.iter().filter(|r| r.kind == 2).filter_map(|r| pick(compose_recipient(r.name.as_deref(), r.email.as_deref()))).collect();
            if to.is_empty() {
                to.extend(senders);
            }
            (to, cc)
        }
    }
}

/// Проверка размеров вложений до чтения файлов: по одному и вместе.
pub(crate) fn check_sizes(sizes: &[(String, u64)]) -> CmdResult<()> {
    let mut total: u64 = 0;
    for (name, len) in sizes {
        if *len > MAX_ATTACHMENT_BYTES {
            return Err(format!("Файл «{name}» больше лимита {} МиБ", MAX_ATTACHMENT_BYTES / (1024 * 1024)));
        }
        total = total.saturating_add(*len);
        if total > MAX_ATTACHMENTS_TOTAL_BYTES {
            return Err(format!("Вложения вместе больше лимита {} МиБ", MAX_ATTACHMENTS_TOTAL_BYTES / (1024 * 1024)));
        }
    }
    Ok(())
}

/// MIME-тип по расширению для частых случаев; остальное — `application/octet-stream`
/// (сервер и так определяет тип по имени при показе).
fn content_type_for(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("txt" | "log" | "md") => "text/plain",
        Some("csv") => "text/csv",
        Some("htm" | "html") => "text/html",
        Some("xml") => "text/xml",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("rtf") => "application/rtf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("tif" | "tiff") => "image/tiff",
        Some("zip") => "application/zip",
        Some("7z") => "application/x-7z-compressed",
        Some("rar") => "application/vnd.rar",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("eml") => "message/rfc822",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// Читает файлы вложений с проверкой: путь есть и это файл, лимиты на файл и на всё вместе.
/// Диск: вызывать из блокирующего потока.
pub(crate) fn load_attachments(paths: &[String]) -> CmdResult<Vec<OutgoingAttachment>> {
    let mut sizes = Vec::with_capacity(paths.len());
    for p in paths {
        let path = Path::new(p.trim());
        let name = path.file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()).ok_or_else(|| "У вложения нет имени файла".to_string())?.to_string();
        let meta = std::fs::metadata(path).map_err(|_| format!("Файл «{name}» не найден"))?;
        if !meta.is_file() {
            return Err(format!("«{name}» — не файл"));
        }
        sizes.push((name, meta.len()));
    }
    check_sizes(&sizes)?;
    let mut out = Vec::with_capacity(paths.len());
    for (p, (name, _)) in paths.iter().zip(sizes) {
        let bytes = std::fs::read(p.trim()).map_err(|e| {
            warn!(error = %e, "вложение не прочитано");
            format!("Не удалось прочитать файл «{name}»")
        })?;
        out.push(OutgoingAttachment { content_type: Some(content_type_for(&name).to_string()), name, bytes });
    }
    // Размер мог вырасти между `metadata` и `read`.
    check_sizes(&out.iter().map(|a| (a.name.clone(), a.bytes.len() as u64)).collect::<Vec<_>>())?;
    Ok(out)
}

/// Получатели запроса в участников письма с проверкой формата адреса.
fn participants(rs: &[ComposeRecipientDto]) -> CmdResult<Vec<Participant>> {
    rs.iter()
        .map(|r| {
            let email = r.email.trim();
            if !looks_like_email(email) {
                return Err(format!("Адрес «{email}» не похож на почтовый"));
            }
            let name = r.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(String::from);
            Ok(Participant { name, email: Some(email.to_string()), routing_type: Some("SMTP".into()) })
        })
        .collect()
}

/// Исходящее письмо из запроса окна: адреса проверены, тело очищено. `require_recipient` —
/// для отправки нужен хотя бы один получатель; черновик можно сохранить и без него.
pub(crate) fn outgoing(req: &ComposeRequest, require_recipient: bool) -> CmdResult<OutgoingMessage> {
    let to = participants(&req.to)?;
    let cc = participants(&req.cc)?;
    let bcc = participants(&req.bcc)?;
    if require_recipient && to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err("Укажите хотя бы одного получателя".into());
    }
    let body_html = crate::sanitize_html(&req.body_html, &Arc::new(InlineMap::new()));
    Ok(OutgoingMessage { to, cc, bcc, subject: req.subject.trim().to_string(), body_html, importance: Importance::Normal })
}

// --- база ----------------------------------------------------------------------------------

/// Собственные адреса: самые частые отправители в «Отправленных» плюс логин, если он адрес;
/// в нижнем регистре, без повторов.
fn own_addresses_of(store: &Store, account: AccountId, user: &str) -> CmdResult<Vec<String>> {
    let mut out = store.own_addresses(account).map_err(err)?;
    let login = user.trim().to_lowercase();
    if login.contains('@') && !out.contains(&login) {
        out.push(login);
    }
    Ok(out)
}

fn signature_of(store: &Store, account: AccountId) -> CmdResult<SignatureDto> {
    let html = store.get_state(account, SIGNATURE_HTML_KEY).map_err(err)?.unwrap_or_default();
    let auto_add = store.get_state(account, SIGNATURE_AUTO_KEY).map_err(err)?.as_deref() == Some("1");
    Ok(SignatureDto { html, auto_add })
}

fn store_signature(store: &Store, account: AccountId, sig: &SignatureDto) -> CmdResult<()> {
    store.set_state(account, SIGNATURE_HTML_KEY, Some(&sig.html)).map_err(err)?;
    store.set_state(account, SIGNATURE_AUTO_KEY, Some(if sig.auto_add { "1" } else { "0" })).map_err(err)
}

/// Вид письма для транспорта: у ответа и пересылки — идентификатор оригинала из базы.
fn compose_kind(store: &Store, kind: ComposeKindDto, reference_item_pk: Option<i64>) -> CmdResult<ComposeKind> {
    if !kind.needs_reference() {
        return Ok(ComposeKind::New);
    }
    let pk = reference_item_pk.ok_or_else(|| "Не указано исходное письмо".to_string())?;
    let reference = store.item_ref(pk).map_err(err)?.ok_or_else(|| "Исходное письмо удалено".to_string())?.id;
    Ok(match kind {
        ComposeKindDto::Reply => ComposeKind::Reply { reference },
        ComposeKindDto::ReplyAll => ComposeKind::ReplyAll { reference },
        ComposeKindDto::Forward => ComposeKind::Forward { reference },
        ComposeKindDto::New => ComposeKind::New,
    })
}

/// Всё, что нужно транспорту: вид, письмо, файлы (прочитаны в блокирующем потоке).
async fn prepare(state: &AppState, req: &ComposeRequest, require_recipient: bool) -> CmdResult<(ComposeKind, OutgoingMessage, Vec<OutgoingAttachment>)> {
    let kind = compose_kind(&state.store, req.kind, req.reference_item_pk)?;
    let msg = outgoing(req, require_recipient)?;
    let paths = req.attachments.clone();
    let files = if paths.is_empty() { Vec::new() } else { tauri::async_runtime::spawn_blocking(move || load_attachments(&paths)).await.map_err(err)?? };
    Ok((kind, msg, files))
}

// --- сеть ----------------------------------------------------------------------------------

/// Один сетевой шаг под таймаутом: ошибка транспорта сбрасывает мёртвую сессию и превращается
/// в текст для пользователя.
async fn step<T>(sync: &SyncState, what: &'static str, limit: Duration, fut: impl Future<Output = aiowa_transport::Result<T>>) -> CmdResult<T> {
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            warn!(step = what, error = %e, "шаг композера отклонён");
            sync.invalidate_on(&e).await;
            Err(describe_transport(&e))
        }
        Err(_) => {
            warn!(step = what, "шаг композера не уложился в таймаут");
            Err(timed_out(limit))
        }
    }
}

/// Черновик остался на сервере, дальше не вышло.
fn kept(why: String) -> SendReport {
    SendReport { sent: false, draft_saved: true, maybe_sent: false, message: Some(format!("{DRAFT_KEPT}. {why}")) }
}

/// `SendItem` ушёл, а исход неизвестен: сказать «не отправлено» было бы неправдой, а повтор —
/// возможным дублем. Окно по `maybe_sent` не предлагает «Повторить».
fn maybe_sent(why: String) -> SendReport {
    SendReport { sent: false, draft_saved: true, maybe_sent: true, message: Some(format!("{why}, исход отправки неизвестен. Проверьте «Отправленные»: если письма там нет, черновик остался в «Черновиках»")) }
}

/// Ошибка транспорта на `SendItem`, при которой письмо могло уйти: обрыв связи (в том числе при
/// чтении ответа после того, как сервер выполнил запрос) и неразобранный ответ или шлюз (ответ
/// был, но что сервер сделал — неизвестно). Явный отказ сервера (`Server`, вход, лимиты) — нет.
fn outcome_unknown(e: &aiowa_transport::TransportError) -> bool {
    use aiowa_transport::TransportError;
    matches!(e, TransportError::Network(_) | TransportError::Protocol { .. })
}

// --- команды Tauri --------------------------------------------------------------------------

/// Заготовка композера. Для ответа и пересылки `item_pk` — исходное письмо: его DTO (с телом из
/// базы, если оно загружено), получатели и тема с префиксом. Сети нет.
#[tauri::command]
pub(crate) async fn compose_context(kind: ComposeKindDto, item_pk: Option<i64>, state: State<'_, AppState>) -> CmdResult<ComposeContext> {
    let store = state.store.clone();
    let account = state.account;
    let own_addresses = own_addresses_of(&store, account, &state.user)?;
    let signature = signature_of(&store, account)?;
    if !kind.needs_reference() {
        return Ok(ComposeContext { kind, reference_item_pk: None, to: vec![], cc: vec![], subject: String::new(), original: None, own_addresses, signature });
    }
    let pk = item_pk.ok_or_else(|| "Не указано исходное письмо".to_string())?;
    // DTO читает файлы кэша (инлайновые картинки) — в блокирующем потоке.
    let original = tauri::async_runtime::spawn_blocking(move || {
        let head = store.message_head(pk).map_err(err)?.ok_or_else(|| "Исходное письмо удалено".to_string())?;
        build_message(&store, head)
    })
    .await
    .map_err(err)??;
    let (to, cc) = reply_recipients(kind, original.from_name.as_deref(), original.from_email.as_deref(), &original.reply_to, &original.recipients, &own_addresses);
    let subject = subject_with_prefix(kind, original.subject.as_deref());
    Ok(ComposeContext { kind, reference_item_pk: Some(pk), to, cc, subject, original: Some(original), own_addresses, signature })
}

/// Отправка: проверки → черновик → вложения → `SendItem`. Отказ до создания черновика — ошибка
/// команды (окно покажет её с «Повторить»); после — `draft_saved` с текстом.
#[tauri::command]
pub(crate) async fn compose_send(req: ComposeRequest, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cs: State<'_, ComposeState>) -> CmdResult<SendReport> {
    let _busy = cs.sending.try_lock().map_err(|_| "Отправка уже идёт".to_string())?;
    let (kind, msg, files) = prepare(&state, &req, true).await?;
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let draft = step(&sync, "CreateItem", CREATE_TIMEOUT, transport.create_draft(&kind, &msg)).await?;
    let mut current = draft.clone();
    if !files.is_empty() {
        match step(&sync, "CreateAttachment", ATTACH_TIMEOUT, transport.add_attachments(&draft, &files)).await {
            Ok(id) => current = id,
            Err(why) => return Ok(kept(why)),
        }
    }
    match tokio::time::timeout(SEND_TIMEOUT, transport.send_draft(&current)).await {
        Ok(Ok(())) => {
            info!(kind = ?req.kind, to = msg.to.len(), cc = msg.cc.len(), bcc = msg.bcc.len(), attachments = files.len(), "письмо отправлено");
            Ok(SendReport { sent: true, draft_saved: false, maybe_sent: false, message: None })
        }
        // Обрыв связи или неразобранный ответ: запрос мог дойти, письмо могло уйти.
        Ok(Err(e)) if outcome_unknown(&e) => {
            warn!(error = %e, "SendItem без ответа, исход неизвестен");
            sync.invalidate_on(&e).await;
            Ok(maybe_sent(describe_transport(&e)))
        }
        Ok(Err(e)) => {
            warn!(error = %e, "SendItem отклонён");
            sync.invalidate_on(&e).await;
            Ok(kept(describe_transport(&e)))
        }
        // Ответа на SendItem нет: письмо могло и уйти — сказать «не отправлено» было бы неправдой.
        Err(_) => {
            warn!("SendItem не уложился в таймаут, исход неизвестен");
            Ok(maybe_sent(format!("Сервер не ответил за {} с", SEND_TIMEOUT.as_secs())))
        }
    }
}

/// Сохранить черновик на сервере: черновик → вложения, без отправки. Получатели не обязательны.
#[tauri::command]
pub(crate) async fn compose_save_draft(req: ComposeRequest, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cs: State<'_, ComposeState>) -> CmdResult<SendReport> {
    let _busy = cs.sending.try_lock().map_err(|_| "Отправка уже идёт".to_string())?;
    let (kind, msg, files) = prepare(&state, &req, false).await?;
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let draft = step(&sync, "CreateItem", CREATE_TIMEOUT, transport.create_draft(&kind, &msg)).await?;
    if !files.is_empty()
        && let Err(why) = step(&sync, "CreateAttachment", ATTACH_TIMEOUT, transport.add_attachments(&draft, &files)).await
    {
        return Ok(SendReport { sent: false, draft_saved: true, maybe_sent: false, message: Some(format!("Черновик сохранён в «Черновиках» без вложений. {why}")) });
    }
    info!(kind = ?req.kind, attachments = files.len(), "черновик сохранён на сервере");
    Ok(SendReport { sent: false, draft_saved: true, maybe_sent: false, message: None })
}

/// Подсказки адресатов по началу адреса или слова имени. Первый вызов строит кэш по всей базе
/// (доли секунды), поэтому в блокирующем потоке.
#[tauri::command]
pub(crate) async fn recipients_suggest(prefix: String, limit: Option<usize>, state: State<'_, AppState>) -> CmdResult<Vec<RecipientSuggestionDto>> {
    let store = state.store.clone();
    let account = state.account;
    let limit = limit.unwrap_or(SUGGEST_DEFAULT).clamp(1, SUGGEST_MAX);
    tauri::async_runtime::spawn_blocking(move || {
        let rows = store.suggest_recipients(account, &prefix, limit).map_err(err)?;
        Ok(rows.into_iter().map(|r| RecipientSuggestionDto { name: r.name, email: r.email, weight: r.weight }).collect())
    })
    .await
    .map_err(err)?
}

#[tauri::command]
pub(crate) fn own_addresses(state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    own_addresses_of(&state.store, state.account, &state.user)
}

#[tauri::command]
pub(crate) fn signature_get(state: State<'_, AppState>) -> CmdResult<SignatureDto> {
    signature_of(&state.store, state.account)
}

/// Сохранить подпись локально. HTML очищается так же, как тело письма.
#[tauri::command]
pub(crate) fn signature_set(html: String, auto_add: bool, state: State<'_, AppState>) -> CmdResult<()> {
    let sig = SignatureDto { html: crate::sanitize_html(&html, &Arc::new(InlineMap::new())), auto_add };
    store_signature(&state.store, state.account, &sig)
}

/// Подпись из настроек OWA (`GetUserConfiguration OWA.UserOptions`): очищается, сохраняется
/// локально и возвращается. Если в OWA подписи нет — ошибка строкой.
#[tauri::command]
pub(crate) async fn signature_import_owa(state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>) -> CmdResult<SignatureDto> {
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let found = step(&sync, "GetUserConfiguration", CREATE_TIMEOUT, transport.user_signature()).await?;
    let Some(owa) = found else { return Err("В настройках OWA подпись не задана".into()) };
    let sig = SignatureDto { html: crate::sanitize_html(&owa.html, &Arc::new(InlineMap::new())), auto_add: owa.auto_add };
    store_signature(&state.store, state.account, &sig)?;
    info!(chars = sig.html.chars().count(), auto_add = sig.auto_add, "подпись взята из OWA");
    Ok(sig)
}

/// Системный диалог выбора файлов для вложений; отмена — пустой список. Размер читается с диска
/// в блокирующем потоке; лимиты проверяются при отправке.
#[tauri::command]
pub(crate) async fn attachments_pick(app: AppHandle) -> CmdResult<Vec<PickedFile>> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Вложить файлы");
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }
    let Some(chosen) = dialog.pick_files().await else { return Ok(vec![]) };
    let paths: Vec<std::path::PathBuf> = chosen.iter().map(|f| f.path().to_path_buf()).collect();
    tauri::async_runtime::spawn_blocking(move || {
        Ok(paths
            .into_iter()
            .filter_map(|p| {
                let name = p.file_name().and_then(|n| n.to_str())?.to_string();
                let size = std::fs::metadata(&p).ok().filter(|m| m.is_file())?.len();
                Some(PickedFile { path: p.display().to_string(), name, size })
            })
            .collect())
    })
    .await
    .map_err(err)?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(kind: i64, name: Option<&str>, email: Option<&str>) -> RecipientDto {
        RecipientDto { kind, name: name.map(String::from), email: email.map(String::from) }
    }

    fn emails(rs: &[ComposeRecipientDto]) -> Vec<&str> {
        rs.iter().map(|r| r.email.as_str()).collect()
    }

    #[test]
    fn subject_prefix_is_not_duplicated() {
        use ComposeKindDto::*;
        assert_eq!(subject_with_prefix(Reply, Some("Отчёт")), "RE: Отчёт");
        assert_eq!(subject_with_prefix(ReplyAll, Some("  Отчёт ")), "RE: Отчёт");
        assert_eq!(subject_with_prefix(Reply, Some("RE: Отчёт")), "RE: Отчёт");
        assert_eq!(subject_with_prefix(Reply, Some("Re: Отчёт")), "Re: Отчёт");
        assert_eq!(subject_with_prefix(Reply, Some("re: отчёт")), "re: отчёт");
        assert_eq!(subject_with_prefix(Reply, Some("Ответ: Отчёт")), "Ответ: Отчёт");
        assert_eq!(subject_with_prefix(Forward, Some("Отчёт")), "FW: Отчёт");
        assert_eq!(subject_with_prefix(Forward, Some("FW: Отчёт")), "FW: Отчёт");
        assert_eq!(subject_with_prefix(Forward, Some("Fwd: Отчёт")), "Fwd: Отчёт");
        assert_eq!(subject_with_prefix(Forward, Some("FWD: Отчёт")), "FWD: Отчёт");
        assert_eq!(subject_with_prefix(Forward, Some("Пересл: Отчёт")), "Пересл: Отчёт");
        // Ответ на пересылку и пересылка ответа получают свой префикс, как в OWA.
        assert_eq!(subject_with_prefix(Reply, Some("FW: Отчёт")), "RE: FW: Отчёт");
        assert_eq!(subject_with_prefix(Forward, Some("RE: Отчёт")), "FW: RE: Отчёт");
        assert_eq!(subject_with_prefix(Reply, None), "RE:");
        assert_eq!(subject_with_prefix(Forward, Some("   ")), "FW:");
        assert_eq!(subject_with_prefix(New, Some("RE: x")), "RE: x");
        // «Remont» не префикс: нужен двоеточие.
        assert_eq!(subject_with_prefix(Reply, Some("Remont")), "RE: Remont");
    }

    #[test]
    fn reply_all_excludes_own_addresses_and_duplicates() {
        let own = vec!["me@x.ru".to_string(), "alias@x.ru".to_string()];
        let recipients = [
            r(1, Some("Я"), Some("Me@X.ru")),
            r(1, Some("Борис"), Some("b@y.ru")),
            r(1, None, Some("B@Y.RU")),
            r(1, Some("Устаревший"), Some("/o=Org/ou=Exchange/cn=Recipients/cn=old")),
            r(1, Some("Без адреса"), None),
            r(2, Some("Вера"), Some("v@z.ru")),
            r(2, None, Some("alias@x.ru")),
            r(2, Some("Анна"), Some("a@y.ru")),
            r(3, Some("Скрытый"), Some("hidden@y.ru")),
        ];
        let (to, cc) = reply_recipients(ComposeKindDto::ReplyAll, Some("Анна"), Some("a@y.ru"), &[], &recipients, &own);
        assert_eq!(emails(&to), vec!["a@y.ru", "b@y.ru"], "отправитель, затем «Кому» без своих, повторов и EX");
        assert_eq!(emails(&cc), vec!["v@z.ru"], "«Копия» без своих и без тех, кто уже в «Кому»; скрытые не возвращаются");
        assert_eq!(to[0].name.as_deref(), Some("Анна"));
        assert_eq!(to[1].name.as_deref(), Some("Борис"));

        let (to, cc) = reply_recipients(ComposeKindDto::Reply, Some("Анна"), Some("a@y.ru"), &[], &recipients, &own);
        assert_eq!(emails(&to), vec!["a@y.ru"]);
        assert!(cc.is_empty());

        // Ответ всем на собственное письмо: отправитель исключён, остальные остаются.
        let (to, _) = reply_recipients(ComposeKindDto::ReplyAll, None, Some("ME@x.ru"), &[], &recipients, &own);
        assert_eq!(emails(&to), vec!["b@y.ru"]);
        // Если кроме своих адресов никого нет — отправитель возвращается в «Кому».
        let only_me = [r(1, None, Some("alias@x.ru"))];
        let (to, _) = reply_recipients(ComposeKindDto::ReplyAll, None, Some("me@x.ru"), &[], &only_me, &own);
        assert_eq!(emails(&to), vec!["me@x.ru"]);

        // Имя, совпадающее с адресом, не дублируется в чипе.
        let (to, _) = reply_recipients(ComposeKindDto::Reply, Some("a@y.ru"), Some("a@y.ru"), &[], &[], &own);
        assert_eq!(to[0].name, None);

        assert_eq!(reply_recipients(ComposeKindDto::Forward, Some("Анна"), Some("a@y.ru"), &[], &recipients, &own), (vec![], vec![]));
        assert_eq!(reply_recipients(ComposeKindDto::New, Some("Анна"), Some("a@y.ru"), &[], &recipients, &own), (vec![], vec![]));
        assert_eq!(reply_recipients(ComposeKindDto::Reply, Some("Без адреса"), None, &[], &recipients, &own), (vec![], vec![]));
    }

    #[test]
    fn reply_to_replaces_sender() {
        let own = vec!["me@x.ru".to_string()];
        let p = |name: Option<&str>, email: &str| ParticipantDto { name: name.map(String::from), email: Some(email.into()) };
        let reply_to = [p(Some("Владелец списка"), "owner@list.ru"), p(None, "help@list.ru")];
        let recipients = [r(1, Some("Я"), Some("me@x.ru")), r(1, Some("Борис"), Some("b@y.ru")), r(2, Some("Вера"), Some("v@z.ru"))];

        // «Ответить»: все адреса Reply-To вместо отправителя.
        let (to, cc) = reply_recipients(ComposeKindDto::Reply, Some("Рассылка"), Some("list@list.ru"), &reply_to, &recipients, &own);
        assert_eq!(emails(&to), vec!["owner@list.ru", "help@list.ru"]);
        assert_eq!(to[0].name.as_deref(), Some("Владелец списка"));
        assert!(cc.is_empty());

        // «Ответить всем»: Reply-To, затем «Кому» без своих; отправитель рассылки не добавляется.
        let (to, cc) = reply_recipients(ComposeKindDto::ReplyAll, Some("Рассылка"), Some("list@list.ru"), &reply_to, &recipients, &own);
        assert_eq!(emails(&to), vec!["owner@list.ru", "help@list.ru", "b@y.ru"]);
        assert_eq!(emails(&cc), vec!["v@z.ru"]);

        // Reply-To без SMTP-адреса (EX или пусто) отбрасывается; если никого не осталось — пусто, как и у отправителя без адреса.
        let bad = [p(Some("Устаревший"), "/o=Org/ou=Exchange/cn=Recipients/cn=old"), ParticipantDto { name: Some("Без адреса".into()), email: None }];
        let (to, _) = reply_recipients(ComposeKindDto::Reply, Some("Анна"), Some("a@y.ru"), &bad, &recipients, &own);
        assert!(to.is_empty(), "Reply-To есть, но без годных адресов: отправитель не подставляется (заголовок просил не ему)");

        // Повтор Reply-To в «Кому» оригинала не дублируется.
        let dup = [r(1, Some("Владелец"), Some("OWNER@list.ru"))];
        let (to, _) = reply_recipients(ComposeKindDto::ReplyAll, None, Some("list@list.ru"), &reply_to[..1], &dup, &own);
        assert_eq!(emails(&to), vec!["owner@list.ru"]);

        // Пересылка и новое письмо Reply-To не трогают.
        assert_eq!(reply_recipients(ComposeKindDto::Forward, None, Some("list@list.ru"), &reply_to, &recipients, &own), (vec![], vec![]));
    }

    #[test]
    fn email_format_check() {
        assert!(looks_like_email("user@example.org"));
        assert!(looks_like_email("  Ivan.Petrov@corp.example.ru "));
        assert!(looks_like_email("иван@почта.рф"));
        assert!(!looks_like_email(""));
        assert!(!looks_like_email("user"));
        assert!(!looks_like_email("user@"));
        assert!(!looks_like_email("@example.org"));
        assert!(!looks_like_email("user@localhost"));
        assert!(!looks_like_email("user@.example.org"));
        assert!(!looks_like_email("user@example.org."));
        assert!(!looks_like_email("user@@example.org"));
        assert!(!looks_like_email("user name@example.org"));
        assert!(!looks_like_email("Иван <ivan@example.org>"));
        assert!(!looks_like_email("a@b.c,d@e.f"));
        assert!(!looks_like_email("/o=Org/ou=Exchange/cn=Recipients/cn=old"));
    }

    #[test]
    fn outgoing_validates_recipients_and_cleans_body() {
        let req = ComposeRequest {
            kind: ComposeKindDto::New,
            reference_item_pk: None,
            to: vec![ComposeRecipientDto { name: Some(" Анна ".into()), email: " a@y.ru ".into() }],
            cc: vec![],
            bcc: vec![],
            subject: "  Тема  ".into(),
            body_html: r#"<div onclick="x()">Привет<script>alert(1)</script><a href="javascript:1">ссылка</a></div>"#.into(),
            attachments: vec![],
        };
        let msg = outgoing(&req, true).unwrap();
        assert_eq!(msg.subject, "Тема");
        assert_eq!(msg.to[0].email.as_deref(), Some("a@y.ru"));
        assert_eq!(msg.to[0].name.as_deref(), Some("Анна"));
        assert_eq!(msg.to[0].routing_type.as_deref(), Some("SMTP"));
        assert!(!msg.body_html.contains("script") && !msg.body_html.contains("onclick") && !msg.body_html.contains("javascript:"), "{}", msg.body_html);
        assert!(msg.body_html.contains("Привет"));

        let empty = ComposeRequest { to: vec![], ..req };
        assert_eq!(outgoing(&empty, true).unwrap_err(), "Укажите хотя бы одного получателя");
        assert!(outgoing(&empty, false).is_ok(), "черновик можно сохранить без получателей");

        let bad = ComposeRequest { bcc: vec![ComposeRecipientDto { name: None, email: "кто-то".into() }], ..empty };
        assert!(outgoing(&bad, false).unwrap_err().contains("не похож на почтовый"));
    }

    #[test]
    fn attachment_limits() {
        let mib = 1024 * 1024;
        assert!(check_sizes(&[]).is_ok());
        assert!(check_sizes(&[("a.pdf".into(), 25 * mib)]).is_ok(), "ровно 25 МиБ проходит");
        assert_eq!(check_sizes(&[("a.pdf".into(), 25 * mib + 1)]).unwrap_err(), "Файл «a.pdf» больше лимита 25 МиБ");
        assert!(check_sizes(&[("a.pdf".into(), 20 * mib), ("b.pdf".into(), 15 * mib)]).is_ok(), "ровно 35 МиБ вместе проходит");
        assert_eq!(check_sizes(&[("a.pdf".into(), 20 * mib), ("b.pdf".into(), 15 * mib + 1)]).unwrap_err(), "Вложения вместе больше лимита 35 МиБ");
        assert!(check_sizes(&[("a".into(), u64::MAX), ("b".into(), 1)]).is_err(), "переполнение не обходит лимит");
    }

    #[test]
    fn load_attachments_reads_files_and_reports_missing() {
        let dir = std::env::temp_dir().join(format!("aiowa-compose-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("отчёт.TXT");
        std::fs::write(&file, b"hello").unwrap();
        let got = load_attachments(&[file.display().to_string()]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "отчёт.TXT");
        assert_eq!(got[0].content_type.as_deref(), Some("text/plain"));
        assert_eq!(got[0].bytes, b"hello");
        let missing = dir.join("нет.pdf");
        assert_eq!(load_attachments(&[missing.display().to_string()]).unwrap_err(), "Файл «нет.pdf» не найден");
        assert!(load_attachments(&[dir.display().to_string()]).unwrap_err().contains("не файл"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type_for("a.PDF"), "application/pdf");
        assert_eq!(content_type_for("a.docx"), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(content_type_for("archive.tar.gz"), "application/octet-stream");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
    }

    #[test]
    fn send_item_outcome_unknown_only_for_network_and_protocol() {
        use aiowa_transport::TransportError;
        assert!(outcome_unknown(&TransportError::Network("обрыв".into())));
        assert!(outcome_unknown(&TransportError::Protocol { op: "SendItem".into(), status: 502, raw: String::new() }));
        assert!(outcome_unknown(&TransportError::Protocol { op: "SendItem".into(), status: 200, raw: "не JSON".into() }));
        assert!(!outcome_unknown(&TransportError::Server { code: "ErrorInvalidRecipients".into(), message: String::new() }));
        assert!(!outcome_unknown(&TransportError::SessionExpired));
        assert!(!outcome_unknown(&TransportError::Throttled { retry_after: None }));
        let rep = maybe_sent("Нет связи с сервером".into());
        assert!(rep.maybe_sent && rep.draft_saved && !rep.sent);
        assert!(rep.message.as_deref().unwrap().contains("исход отправки неизвестен"), "{rep:?}");
        assert!(!rep.message.as_deref().unwrap().contains("повторите"), "повтор после неизвестного исхода не предлагается: {rep:?}");
        let rep = kept("Сервер отказал: X".into());
        assert!(!rep.maybe_sent && rep.draft_saved);
    }

    #[test]
    fn compose_kind_dto_uses_snake_case() {
        assert_eq!(serde_json::from_str::<ComposeKindDto>("\"reply_all\"").unwrap(), ComposeKindDto::ReplyAll);
        assert_eq!(serde_json::to_string(&ComposeKindDto::Forward).unwrap(), "\"forward\"");
        assert!(ComposeKindDto::Reply.needs_reference() && !ComposeKindDto::New.needs_reference());
    }
}
