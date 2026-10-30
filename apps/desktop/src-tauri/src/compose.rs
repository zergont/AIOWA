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
//! Черновики. Композер автосохраняет письмо (`compose_autosave`: первый раз `CreateItem`, дальше
//! `UpdateItem`, вложения не трогает), сохраняет его вручную вместе с файлами
//! (`compose_save_draft`) и открывает черновик из «Черновиков» (`compose_open_draft`: текст из
//! базы, а если полного тела нет — `GetItem` с сервера; обрезанный сервером не открывается).
//! Отправка существующего черновика (`ComposeRequest::draft_server_id`) идёт по тому же письму:
//! `UpdateItem` → новые файлы (`CreateAttachment`) → убранные (`DeleteAttachment`) → `SendItem`;
//! новое копию не плодит. Перед вложениями черновик сверяется с сервером: уже загруженные файлы и
//! уже удалённые вложения повтор не трогает. Отказ после появления черновика возвращает его
//! идентификатор (`SendReport::draft`), чтобы окно продолжало работать с ним, а не создавало второй.
//! `compose_discard_draft` переносит черновик в «Удалённые». `UpdateItem` заменяет тело целиком, а
//! цитату оригинала сервер добавляет только при создании ответа, поэтому при обновлении черновика
//! ответа или пересылки цитата собирается заново (`quote_html`) из полного тела оригинала,
//! прочитанного с сервера: в базе только `UniqueBody`, без истории переписки.
//!
//! Тело письма перед отправкой проходит ту же очистку ammonia, что и показ писем: во вставленном
//! из буфера HTML не должно уйти скриптов и обработчиков. Ссылки `cid:` на инлайновые вложения
//! при этом остаются как есть (`sanitize_keep_cid`): подмена на `data:` сломала бы картинки у
//! адресата. Цитату оригинала (и вложения оригинала у пересылки) при создании добавляет сервер,
//! клиент передаёт только новую часть.

use crate::ondemand::{describe_transport, is_stale_object, timed_out};
use crate::sync::SyncState;
use crate::{build_message, err, prepare_attachments, AppState, CmdResult, ComposeRecipientDto, InlineMap, MessageDto, ParticipantDto, RecipientDto};
use aiowa_core::{AccountId, AttachmentMeta, BodyType, ComposeKind, DistinguishedFolder, DraftContent, FolderRef, Importance, ItemId, MessageFull, MessageMeta, OutgoingAttachment, OutgoingMessage, Participant};
use aiowa_store::ui::RecipientRow;
use aiowa_store::{AttachmentRow, LocalDraft as StoredDraft, Store};
use aiowa_transport::{DeleteMode, ItemFetchShape, ItemResult, MailTransport, TransportError, WriteResult};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};
use tracing::{debug, info, warn};

/// Таймаут создания черновика (`CreateItem`) и обновления подписи.
const CREATE_TIMEOUT: Duration = Duration::from_secs(60);
/// Таймаут загрузки всех вложений (`CreateAttachment` по одному файлу, до 35 МиБ в base64).
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Таймаут отправки (`SendItem`).
const SEND_TIMEOUT: Duration = Duration::from_secs(60);
/// Таймаут перечитывания вложений черновика после их изменения: окно ждёт ответа, долго тянуть нельзя.
const REREAD_TIMEOUT: Duration = Duration::from_secs(15);
/// Сколько цитат оригиналов держать в памяти (композер один, хватит с запасом).
const QUOTE_CACHE: usize = 4;
/// Сколько попыток `CreateItem` с неизвестным исходом помнить на композер.
const LOST_KEPT: usize = 4;
/// Сколько черновиков-кандидатов читать (`GetItem`), сверяя адреса с оборванной попыткой.
const ADOPT_CHECKS: usize = 3;
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
/// Цитату ответа или пересылки не собрать: оригинала нет ни в базе, ни на сервере. Окно предлагает
/// продолжить без цитаты (`ComposeRequest::without_quote`).
const ORIGINAL_GONE: &str = "Исходное письмо удалено или перенесено: цитату не собрать, черновик на сервере не обновлён. Текст остался в этом окне — письмо можно сохранить или отправить без цитаты";
/// `UpdateItem` черновика, которого на сервере нет.
const DRAFT_GONE: &str = "Черновика на сервере нет: он удалён или письмо уже ушло раньше. Проверьте «Отправленные», прежде чем отправлять заново";

/// Отказ команды композера для окна: текст и что с ним делать. Окно показывает «Повторить»
/// только при `retry`, а при `network` само повторяет автосохранение и говорит, что текст пока
/// только в окне (docs/obkatka.md п. 22).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ComposeFail {
    pub message: String,
    /// Повтор того же действия может помочь: сеть, занятый сервер, истёкшая сессия, устаревший ключ.
    pub retry: bool,
    /// Причина — связь, а не отказ сервера или проверка в окне.
    pub network: bool,
    /// Оригинала ответа или пересылки больше нет: окно предлагает продолжить без цитаты.
    pub original_gone: bool,
}

impl ComposeFail {
    /// Отказ, который повтором не лечится: проверка запроса, база, удалённый оригинал.
    fn fixed(message: impl Into<String>) -> Self {
        Self { message: message.into(), retry: false, network: false, original_gone: false }
    }

    fn retryable(message: impl Into<String>) -> Self {
        Self { message: message.into(), retry: true, network: false, original_gone: false }
    }

    fn from_transport(e: &TransportError) -> Self {
        let network = e.is_network();
        let retry = network || e.is_retryable() || e.needs_reauth() || is_stale_object(e) || matches!(e, TransportError::Protocol { .. });
        Self { message: describe_transport(e), retry, network, original_gone: false }
    }

    /// Оригинала ответа или пересылки нет: цитату не собрать, повтор не поможет — только «без цитаты».
    fn original_gone() -> Self {
        Self { original_gone: true, ..Self::fixed(ORIGINAL_GONE) }
    }

    fn timed_out(limit: Duration) -> Self {
        Self { message: timed_out(limit), retry: true, network: true, original_gone: false }
    }

    /// Вход на сервер не удался: без пароля или с отклонённым паролем повтор не поможет.
    fn connect(e: &crate::sync::ConnectError) -> Self {
        match e {
            crate::sync::ConnectError::Connect(t) => Self::from_transport(t),
            other => Self::fixed(other.to_string()),
        }
    }

    /// Текст с предисловием: «Письмо не отправлено, текст остался в этом окне. Нет связи…».
    fn prefixed(mut self, prefix: &str) -> Self {
        self.message = format!("{prefix}. {}", self.message);
        self
    }
}

impl From<String> for ComposeFail {
    fn from(message: String) -> Self {
        Self::fixed(message)
    }
}

/// Состояние композера: один замок на отправку и сохранение черновика, последние известные
/// ключи изменений черновиков, с которыми работало окно (окно передаёт только серверный `Id`),
/// цитаты оригиналов для обновляемых черновиков ответа и пересылки (ключ — `item_pk` оригинала:
/// полное тело читается с сервера один раз, а не на каждое автосохранение) и попытки `CreateItem`
/// с неизвестным исходом по локальному черновику композера (`ComposeRequest::local_draft_id`):
/// черновик такой попытки может появиться на сервере и после поиска, следующая попытка ищет его первой.
#[derive(Default)]
pub(crate) struct ComposeState {
    sending: tokio::sync::Mutex<()>,
    change_keys: std::sync::Mutex<HashMap<String, Option<String>>>,
    quotes: std::sync::Mutex<Vec<(i64, Arc<String>)>>,
    lost: std::sync::Mutex<HashMap<String, Vec<LostCreate>>>,
}

impl ComposeState {
    /// Черновики, с которыми работали композеры этого запуска (созданные, открытые, обновлённые):
    /// черновик оборванной попытки другого композера среди них быть не может.
    fn known_ids(&self) -> HashSet<String> {
        self.change_keys.lock().unwrap_or_else(|p| p.into_inner()).keys().cloned().collect()
    }

    fn lost(&self, local: Option<&str>) -> Vec<LostCreate> {
        let Some(local) = local else { return Vec::new() };
        self.lost.lock().unwrap_or_else(|p| p.into_inner()).get(local).cloned().unwrap_or_default()
    }

    fn remember_lost(&self, local: Option<&str>, attempt: LostCreate) {
        let Some(local) = local else { return };
        let mut lost = self.lost.lock().unwrap_or_else(|p| p.into_inner());
        let list = lost.entry(local.to_string()).or_default();
        list.push(attempt);
        if list.len() > LOST_KEPT {
            list.remove(0);
        }
    }

    /// У композера есть черновик на сервере: оборванные попытки больше не ищутся.
    fn clear_lost(&self, local: Option<&str>) {
        if let Some(local) = local {
            self.lost.lock().unwrap_or_else(|p| p.into_inner()).remove(local);
        }
    }

    /// Идентификатор черновика с последним известным `ChangeKey` (без него — только `Id`:
    /// `UpdateItem` идёт с `AlwaysOverwrite`, устаревший ключ ему не помеха).
    fn item_id(&self, server_id: &str) -> ItemId {
        let change_key = self.change_keys.lock().unwrap_or_else(|p| p.into_inner()).get(server_id).cloned().flatten();
        ItemId { id: server_id.to_string(), change_key }
    }

    fn remember(&self, id: &ItemId) {
        self.change_keys.lock().unwrap_or_else(|p| p.into_inner()).insert(id.id.clone(), id.change_key.clone());
    }

    fn forget(&self, server_id: &str) {
        self.change_keys.lock().unwrap_or_else(|p| p.into_inner()).remove(server_id);
    }

    fn cached_quote(&self, original_pk: i64) -> Option<Arc<String>> {
        self.quotes.lock().unwrap_or_else(|p| p.into_inner()).iter().find(|(pk, _)| *pk == original_pk).map(|(_, q)| q.clone())
    }

    fn remember_quote(&self, original_pk: i64, quote: Arc<String>) {
        let mut quotes = self.quotes.lock().unwrap_or_else(|p| p.into_inner());
        quotes.retain(|(pk, _)| *pk != original_pk);
        quotes.push((original_pk, quote));
        if quotes.len() > QUOTE_CACHE {
            quotes.remove(0);
        }
    }
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

/// Вложение черновика, уже лежащее на сервере.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct DraftAttachmentDto {
    /// Запись в локальной базе; `None`, если черновик известен только с сервера.
    attachment_pk: Option<i64>,
    /// Серверный `AttachmentId`: им вложение убирается (`ComposeRequest::removed_attachment_ids`).
    server_id: String,
    name: Option<String>,
    size: Option<i64>,
    content_type: Option<String>,
}

impl DraftAttachmentDto {
    fn of(a: &AttachmentMeta, attachment_pk: Option<i64>) -> Self {
        Self { attachment_pk, server_id: a.id.clone(), name: a.name.clone(), size: a.size, content_type: a.content_type.clone() }
    }
}

/// Открытый черновик: откуда он взялся и какие файлы уже приложены на сервере.
#[derive(Debug, Serialize)]
pub(crate) struct DraftInfoDto {
    item_pk: Option<i64>,
    server_id: String,
    change_key: Option<String>,
    attachments: Vec<DraftAttachmentDto>,
    /// Инлайновые картинки тела для показа в редакторе: нормализованный `cid` (как у
    /// `normalize_cid`: без `cid:` и `<>`, в нижнем регистре) → адрес протокола `aiowa-att`.
    /// Только файлы, уже лежащие в кэше. Тело хранит `cid:`; окно подставляет адрес лишь для
    /// показа и возвращает `cid:` перед сохранением.
    inline_images: HashMap<String, String>,
}

/// Ссылка на черновик на сервере после автосохранения; `item_pk` — локальная запись, если
/// синхронизация её уже принесла.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct DraftRef {
    server_id: String,
    change_key: Option<String>,
    item_pk: Option<i64>,
}

/// Заготовка композера: получатели и тема подобраны по виду письма, оригинал — для показа
/// под редактором (только чтение), собственные адреса — чтобы окно не предлагало их в «ответить всем».
/// У открытого черновика (`compose_open_draft`) есть ещё скрытые получатели, тело и `draft`.
#[derive(Serialize)]
pub(crate) struct ComposeContext {
    kind: ComposeKindDto,
    reference_item_pk: Option<i64>,
    to: Vec<ComposeRecipientDto>,
    cc: Vec<ComposeRecipientDto>,
    bcc: Vec<ComposeRecipientDto>,
    subject: String,
    original: Option<MessageDto>,
    own_addresses: Vec<String>,
    signature: SignatureDto,
    /// Тело открытого черновика (очищенный HTML); у новой заготовки `None` — тело строится из подписи.
    body_html: Option<String>,
    draft: Option<DraftInfoDto>,
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
    /// Черновик уже на сервере (автосохранение или открытый из «Черновиков»): отправка идёт по нему.
    #[serde(default)]
    draft_server_id: Option<String>,
    /// Серверные вложения черновика, убранные в композере (`AttachmentId`).
    #[serde(default)]
    removed_attachment_ids: Vec<String>,
    /// Локальный черновик этого композера (`compose_local_save`): после отправки он не нужен.
    #[serde(default)]
    local_draft_id: Option<String>,
    /// Автор решил продолжить без цитаты: оригинала больше нет (`ORIGINAL_GONE`), тело черновика
    /// заменяется одним текстом из окна.
    #[serde(default)]
    without_quote: bool,
}

/// Локальный черновик композера для окна (`local_drafts` в базе): всё, что нужно, чтобы открыть
/// письмо заново после падения или закрытия окна.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LocalDraftDto {
    draft_id: String,
    kind: ComposeKindDto,
    reference_item_pk: Option<i64>,
    draft_server_id: Option<String>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    to: Vec<ComposeRecipientDto>,
    #[serde(default)]
    cc: Vec<ComposeRecipientDto>,
    #[serde(default)]
    bcc: Vec<ComposeRecipientDto>,
    #[serde(default)]
    body_html: String,
    #[serde(default)]
    attachments: Vec<PickedFile>,
    #[serde(default)]
    removed_attachment_ids: Vec<String>,
    #[serde(default)]
    updated_ms: i64,
}

impl LocalDraftDto {
    fn to_row(&self) -> CmdResult<StoredDraft> {
        let json = |v: &dyn erased_ser::Ser| v.to_json();
        Ok(StoredDraft {
            draft_id: self.draft_id.trim().to_string(),
            kind: match self.kind {
                ComposeKindDto::New => "new",
                ComposeKindDto::Reply => "reply",
                ComposeKindDto::ReplyAll => "reply_all",
                ComposeKindDto::Forward => "forward",
            }
            .to_string(),
            reference_item_pk: self.reference_item_pk,
            draft_server_id: self.draft_server_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
            subject: self.subject.clone(),
            to_json: json(&self.to)?,
            cc_json: json(&self.cc)?,
            bcc_json: json(&self.bcc)?,
            body_html: self.body_html.clone(),
            attachments_json: json(&self.attachments)?,
            removed_json: json(&self.removed_attachment_ids)?,
            created_at: 0,
            updated_at: 0,
        })
    }

    fn from_row(d: StoredDraft) -> Self {
        fn list<T: serde::de::DeserializeOwned>(s: &str) -> Vec<T> {
            serde_json::from_str(s).unwrap_or_default()
        }
        Self {
            draft_id: d.draft_id,
            kind: match d.kind.as_str() {
                "reply" => ComposeKindDto::Reply,
                "reply_all" => ComposeKindDto::ReplyAll,
                "forward" => ComposeKindDto::Forward,
                _ => ComposeKindDto::New,
            },
            reference_item_pk: d.reference_item_pk,
            draft_server_id: d.draft_server_id,
            subject: d.subject,
            to: list(&d.to_json),
            cc: list(&d.cc_json),
            bcc: list(&d.bcc_json),
            body_html: d.body_html,
            attachments: list(&d.attachments_json),
            removed_attachment_ids: list(&d.removed_json),
            updated_ms: d.updated_at,
        }
    }

    /// Локальный черновик из запроса отправки, уже записанного в черновик на сервере (`server_id`):
    /// пишется перед `SendItem`. Если окно закроется или приложение упадёт во время отправки,
    /// «Открыть» из полосы «Несохранённое письмо» пойдёт через этот черновик (`UpdateItem`, а у уже
    /// отправленного — отказ), а не создаст новый и не отправит письмо второй раз.
    fn sending(draft_id: &str, req: &ComposeRequest, server_id: &str, attachments: Vec<PickedFile>) -> Self {
        Self {
            draft_id: draft_id.to_string(),
            kind: req.kind,
            reference_item_pk: req.reference_item_pk,
            draft_server_id: Some(server_id.to_string()),
            subject: req.subject.trim().to_string(),
            to: req.to.clone(),
            cc: req.cc.clone(),
            bcc: req.bcc.clone(),
            body_html: req.body_html.clone(),
            attachments,
            removed_attachment_ids: req.removed_attachment_ids.clone(),
            updated_ms: 0,
        }
    }
}

/// Сериализация в JSON с текстом ошибки окна (serde не знает про `CmdResult`).
mod erased_ser {
    pub(super) trait Ser {
        fn to_json(&self) -> super::CmdResult<String>;
    }
    impl<T: serde::Serialize> Ser for T {
        fn to_json(&self) -> super::CmdResult<String> {
            serde_json::to_string(self).map_err(|e| format!("локальный черновик не сериализован: {e}"))
        }
    }
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
    /// Черновик на сервере, если он остался (не отправлено) или сохранён: окно продолжает работать
    /// с ним (автосохранение и «Повторить» идут через `UpdateItem`), а не создаёт второй.
    draft: Option<DraftRef>,
    /// Файловые вложения черновика на сервере после того, как этот вызов их менял (`None` —
    /// не менял или перечитать не вышло): окно сверяет с ними свой список.
    attachments: Option<Vec<DraftAttachmentDto>>,
}

impl SendReport {
    fn with_draft(mut self, draft: DraftRef, attachments: Option<Vec<DraftAttachmentDto>>) -> Self {
        self.draft = Some(draft);
        self.attachments = attachments;
        self
    }
}

#[derive(Serialize)]
pub(crate) struct RecipientSuggestionDto {
    name: Option<String>,
    email: String,
    weight: i64,
}

/// Файл из диалога выбора вложений.
#[derive(Serialize, Debug, Clone, Deserialize, PartialEq, Eq)]
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
pub(crate) fn content_type_for(name: &str) -> &'static str {
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
        Some("ico") => "image/x-icon",
        Some("avif") => "image/avif",
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

/// Исходящее письмо из запроса окна: адреса проверены, тело очищено (ссылки `cid:` на инлайновые
/// вложения черновика остаются). `require_recipient` — для отправки нужен хотя бы один
/// получатель; черновик можно сохранить и без него.
pub(crate) fn outgoing(req: &ComposeRequest, require_recipient: bool) -> CmdResult<OutgoingMessage> {
    let to = participants(&req.to)?;
    let cc = participants(&req.cc)?;
    let bcc = participants(&req.bcc)?;
    if require_recipient && to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err("Укажите хотя бы одного получателя".into());
    }
    let body_html = crate::sanitize_keep_cid(&req.body_html);
    Ok(OutgoingMessage { to, cc, bcc, subject: req.subject.trim().to_string(), body_html, importance: Importance::Normal })
}

/// Идентификатор черновика из запроса; пустая строка — «черновика на сервере нет».
fn draft_id_of(req: &ComposeRequest) -> Option<String> {
    req.draft_server_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Локальный черновик композера из запроса; пустая строка — «нет».
fn local_id_of(req: &ComposeRequest) -> Option<&str> {
    req.local_draft_id.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// `AttachmentId` убранных вложений: без пустых и повторов, порядок сохранён.
fn removed_ids(req: &ComposeRequest) -> Vec<String> {
    let mut seen = HashSet::new();
    req.removed_attachment_ids.iter().map(|s| s.trim()).filter(|s| !s.is_empty() && seen.insert(s.to_string())).map(String::from).collect()
}

/// Файлы, которых ещё нет среди вложений черновика на сервере (имя без учёта регистра и размер
/// совпадают — значит, файл уже приложен): повтор отправки после оборванной загрузки не
/// прикладывает загруженное второй раз.
fn filter_new_files(files: Vec<OutgoingAttachment>, on_server: &[AttachmentMeta]) -> Vec<OutgoingAttachment> {
    files
        .into_iter()
        .filter(|f| {
            let name = f.name.to_lowercase();
            !on_server.iter().any(|a| !a.is_inline && a.size == Some(f.bytes.len() as i64) && a.name.as_deref().is_some_and(|n| n.to_lowercase() == name))
        })
        .collect()
}

/// Сверка с вложениями черновика на сервере (см. `reconcile`): файлы, которых там ещё нет (убираемые
/// вложения не в счёт), и убранные вложения, которые там ещё есть.
fn reconcile_with(on_server: &[AttachmentMeta], removed: Vec<String>, files: Vec<OutgoingAttachment>) -> (Vec<OutgoingAttachment>, Vec<String>) {
    let kept: Vec<AttachmentMeta> = on_server.iter().filter(|a| !removed.contains(&a.id)).cloned().collect();
    let removed = removed.into_iter().filter(|id| on_server.iter().any(|a| &a.id == id)).collect();
    (filter_new_files(files, &kept), removed)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// «Имя <адрес>», только адрес или только имя; пусто, если нет ни того, ни другого.
fn person_label(name: Option<&str>, email: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|n| !n.is_empty());
    let email = email.map(str::trim).filter(|e| !e.is_empty());
    match (name, email) {
        (Some(n), Some(e)) if n != e => format!("{n} <{e}>"),
        (_, Some(e)) => e.to_string(),
        (Some(n), None) => n.to_string(),
        (None, None) => String::new(),
    }
}

/// Цитата оригинала под текстом ответа или пересылки при обновлении черновика: шапка (от кого,
/// когда, кому, копия, тема) и тело оригинала. Тело `body_html` уже очищено, остальное — текст,
/// он экранируется. Пустые «Кому» и «Копия» в шапку не попадают.
pub(crate) fn quote_html(from: &str, sent: Option<&str>, to: &str, cc: &str, subject: &str, body_html: &str) -> String {
    let mut head = format!("<b>От:</b> {}", esc(from));
    if let Some(sent) = sent {
        head.push_str(&format!("<br><b>Отправлено:</b> {}", esc(sent)));
    }
    if !to.is_empty() {
        head.push_str(&format!("<br><b>Кому:</b> {}", esc(to)));
    }
    if !cc.is_empty() {
        head.push_str(&format!("<br><b>Копия:</b> {}", esc(cc)));
    }
    head.push_str(&format!("<br><b>Тема:</b> {}", esc(subject)));
    format!("<hr><div>{head}</div><br><div>{body_html}</div>")
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

/// Вид письма для транспорта: у ответа и пересылки — идентификатор оригинала из базы. Если
/// черновик уже на сервере (`existing`), вид для его обновления не нужен и удалённый оригинал
/// здесь не мешает — вид становится `New`; цитату без оригинала не собрать, это решает `append_quote`.
fn compose_kind(store: &Store, kind: ComposeKindDto, reference_item_pk: Option<i64>, existing: bool) -> CmdResult<ComposeKind> {
    if !kind.needs_reference() {
        return Ok(ComposeKind::New);
    }
    let pk = match reference_item_pk {
        Some(pk) => pk,
        None if existing => return Ok(ComposeKind::New),
        None => return Err("Не указано исходное письмо".to_string()),
    };
    let reference = match store.item_ref(pk).map_err(err)? {
        Some(r) => r.id,
        None if existing => {
            warn!(reference_pk = pk, "исходное письмо удалено, черновик уже на сервере: вид для его обновления не нужен");
            return Ok(ComposeKind::New);
        }
        None => return Err("Исходное письмо удалено".to_string()),
    };
    Ok(match kind {
        ComposeKindDto::Reply => ComposeKind::Reply { reference },
        ComposeKindDto::ReplyAll => ComposeKind::ReplyAll { reference },
        ComposeKindDto::Forward => ComposeKind::Forward { reference },
        ComposeKindDto::New => ComposeKind::New,
    })
}

/// Всё, что нужно транспорту: вид, письмо, файлы (прочитаны в блокирующем потоке).
async fn prepare(state: &AppState, req: &ComposeRequest, require_recipient: bool) -> CmdResult<(ComposeKind, OutgoingMessage, Vec<OutgoingAttachment>)> {
    let kind = compose_kind(&state.store, req.kind, req.reference_item_pk, draft_id_of(req).is_some())?;
    let msg = outgoing(req, require_recipient)?;
    let paths = req.attachments.clone();
    let files = if paths.is_empty() { Vec::new() } else { tauri::async_runtime::spawn_blocking(move || load_attachments(&paths)).await.map_err(err)?? };
    Ok((kind, msg, files))
}

/// Черновик из локальной базы: заголовок, получатели, файлы и тело, если оно есть полностью.
struct LocalDraft {
    id: ItemId,
    subject: String,
    to: Vec<ComposeRecipientDto>,
    cc: Vec<ComposeRecipientDto>,
    bcc: Vec<ComposeRecipientDto>,
    /// Полное тело вместе с цитатой, очищенное, с исходными `cid:`; `None` — локального тела нет
    /// или оно обрезано.
    body_html: Option<String>,
    /// Файловые вложения (без инлайновых картинок тела).
    files: Vec<AttachmentRow>,
    /// Инлайновые картинки из кэша для показа в редакторе (`DraftInfoDto::inline_images`).
    inline_images: HashMap<String, String>,
}

/// Получатели одного вида из строк базы (1 — Кому, 2 — Копия, 3 — Скрытая); без адреса SMTP не берутся.
fn recipients_of(rows: &[RecipientRow], kind: i64) -> Vec<ComposeRecipientDto> {
    rows.iter().filter(|r| r.kind == kind).filter_map(|r| compose_recipient(r.name.as_deref(), r.email.as_deref())).collect()
}

fn local_draft(store: &Store, item_pk: i64) -> CmdResult<LocalDraft> {
    const GONE: &str = "Черновик удалён";
    let item = store.item_ref(item_pk).map_err(err)?.ok_or_else(|| GONE.to_string())?;
    let head = store.message_head(item_pk).map_err(err)?.ok_or_else(|| GONE.to_string())?;
    if !head.is_draft {
        return Err("Это письмо не черновик".into());
    }
    // Контакт, встреча и задача тоже приходят с `IsDraft` (никогда не отправлялись): в композере их
    // сохранение переписало бы элемент полями письма.
    if !aiowa_core::ItemKind::of(head.item_class.as_deref(), true).is_mail() {
        return Err("Это не письмо: в композере открываются только черновики писем".into());
    }
    let rows = store.recipients(item_pk).map_err(err)?;
    // Правится полное тело: UniqueBody у ответа обрезан до нового текста, а в черновике нужна и цитата.
    let body = store.message_body(item_pk).map_err(err)?.filter(|b| !b.is_truncated);
    let (atts, _) = prepare_attachments(store, item_pk, None)?;
    // Ссылки cid: остаются в теле: при сохранении они должны уйти на сервер как были, а не data:.
    let body_html = body.and_then(|b| match (b.full, b.body_type) {
        (Some(html), BodyType::Html) => Some(crate::sanitize_keep_cid(&html)),
        (Some(text), BodyType::Text) => Some(crate::text_to_html(&text)),
        (None, _) => None,
    });
    let inline_images = inline_preview_urls(&atts);
    Ok(LocalDraft {
        id: item.id,
        subject: head.subject.unwrap_or_default(),
        to: recipients_of(&rows, 1),
        cc: recipients_of(&rows, 2),
        bcc: recipients_of(&rows, 3),
        body_html,
        files: atts.into_iter().filter(|a| !a.is_inline).collect(),
        inline_images,
    })
}

/// Адреса для показа инлайновых картинок черновика в редакторе: нормализованный `cid` → адрес
/// протокола `aiowa-att` (только файлы, уже лежащие в кэше).
fn inline_preview_urls(rows: &[AttachmentRow]) -> HashMap<String, String> {
    rows.iter()
        .filter(|a| a.is_inline && a.local_path.is_some())
        .filter_map(|a| {
            let key = crate::normalize_cid(a.content_id.as_deref()?);
            (!key.is_empty()).then(|| (key, crate::preview::preview_url(a.attachment_pk)))
        })
        .collect()
}

/// Цитата оригинала из письма, прочитанного с сервера с полным телом (`ItemFetchShape::WithFullBody`):
/// шапка (от кого, когда, кому, копия, тема) и тело вместе с историей переписки. Ссылки `cid:`
/// остаются как есть — они указывают на инлайновые вложения, которые сервер скопировал в черновик
/// при создании; `data:` не подставляется. Полного тела нет или оно обрезано — ошибка: черновик не
/// должен уйти без цитаты или с обрезанной.
pub(crate) fn quote_from_full(m: &MessageFull) -> CmdResult<String> {
    const NO_BODY: &str = "Сервер не отдал полный текст исходного письма — цитату не собрать, черновик не обновлён";
    let body = m.body.as_ref().ok_or(NO_BODY)?;
    if body.is_truncated {
        return Err("Исходное письмо слишком большое: цитата в черновике была бы обрезана, черновик не обновлён".into());
    }
    let raw = body.full_body.as_deref().ok_or(NO_BODY)?;
    let html = match body.body_type {
        BodyType::Html => crate::sanitize_keep_cid(raw),
        BodyType::Text => crate::text_to_html(raw),
    };
    let list = |ps: &[Participant]| ps.iter().map(|p| person_label(p.name.as_deref(), p.email.as_deref())).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; ");
    let sent = m.meta.sent_at.or(m.meta.received_at).map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%d.%m.%Y %H:%M").to_string());
    let from = m.meta.from.as_ref().map(|p| person_label(p.name.as_deref(), p.email.as_deref())).unwrap_or_default();
    Ok(quote_html(&from, sent.as_deref(), &list(&m.to), &list(&m.cc), m.meta.subject.as_deref().unwrap_or_default(), &html))
}

/// Убирает черновик из локальной базы сразу, не дожидаясь синхронизации «Черновиков»: окно
/// получает `items-changed`. Если черновика в базе нет (создан автосохранением и ещё не
/// синхронизирован) — ничего.
fn drop_local_draft(app: &AppHandle, store: &Store, account: AccountId, server_id: &str) {
    let Ok(Some((pk, false))) = store.item_pk_by_server_id(account, server_id) else { return };
    let folder_pk = store.item_ref(pk).ok().flatten().map(|r| r.folder_pk);
    match store.mark_deleted_local(&[pk]) {
        Ok(conversations) => crate::write::emit_items_changed(app, conversations, folder_pk.into_iter().collect()),
        Err(e) => warn!(error = %e, "не удалось убрать черновик из локальной базы"),
    }
}

// --- сеть ----------------------------------------------------------------------------------

/// Один сетевой шаг под таймаутом: ошибка транспорта сбрасывает мёртвую сессию и превращается
/// в отказ для окна (`ComposeFail`). В журнале — шаг, длительность и цепочка причин.
async fn step<T>(sync: &SyncState, what: &'static str, limit: Duration, fut: impl Future<Output = aiowa_transport::Result<T>>) -> Result<T, ComposeFail> {
    let started = Instant::now();
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(v)) => {
            debug!(step = what, ms = started.elapsed().as_millis() as u64, "шаг композера");
            Ok(v)
        }
        Ok(Err(e)) => {
            warn!(step = what, ms = started.elapsed().as_millis() as u64, error = %e.chain(), "шаг композера отклонён");
            sync.invalidate_on(&e).await;
            Err(step_fail(what, &e))
        }
        Err(_) => {
            warn!(step = what, limit_s = limit.as_secs(), "шаг композера не уложился в таймаут");
            Err(ComposeFail::timed_out(limit))
        }
    }
}

/// Отказ шага для окна. `UpdateItem` черновика, которого на сервере нет, — не голый код, а что
/// могло случиться: черновик удалён или письмо уже ушло (окно закрылось во время отправки, и
/// письмо открыто из полосы «Несохранённое письмо»); новое письмо из того же текста стало бы дублем.
fn step_fail(what: &str, e: &TransportError) -> ComposeFail {
    match e {
        TransportError::Server { code, .. } if what == "UpdateItem" && code == ITEM_NOT_FOUND => ComposeFail::fixed(DRAFT_GONE),
        _ => ComposeFail::from_transport(e),
    }
}

/// Черновик остался на сервере, дальше не вышло.
fn kept(why: String) -> SendReport {
    SendReport { draft_saved: true, message: Some(format!("{DRAFT_KEPT}. {why}")), ..SendReport::default() }
}

/// `SendItem` ушёл, а исход неизвестен: сказать «не отправлено» было бы неправдой, а повтор —
/// возможным дублем. Окно по `maybe_sent` не предлагает «Повторить» и не автосохраняет.
fn maybe_sent(why: String) -> SendReport {
    SendReport { sent: false, draft_saved: true, maybe_sent: true, message: Some(format!("{why}, исход отправки неизвестен. Проверьте «Отправленные»: если письма там нет, черновик остался в «Черновиках»")), ..SendReport::default() }
}

/// Ссылка на черновик для окна; `item_pk` — если синхронизация уже принесла его в базу.
fn draft_ref(state: &AppState, id: &ItemId) -> DraftRef {
    let item_pk = state.store.item_pk_by_server_id(state.account, &id.id).ok().flatten().and_then(|(pk, deleted)| (!deleted).then_some(pk));
    DraftRef { server_id: id.id.clone(), change_key: id.change_key.clone(), item_pk }
}

/// Файловые вложения черновика на сервере после того, как их меняли: окно сверяет с ними свой
/// список (загруженные файлы становятся серверными, удалённые пропадают). Не вышло — `None`:
/// окно оставляет свой список, а следующий вызов сверится с сервером сам (`reconcile`).
async fn server_files(sync: &SyncState, transport: &impl MailTransport, item: &ItemId) -> Option<Vec<DraftAttachmentDto>> {
    match step(sync, "GetItem", REREAD_TIMEOUT, transport.get_draft(item)).await {
        Ok(draft) => Some(draft.file_attachments().map(|a| DraftAttachmentDto::of(a, None)).collect()),
        Err(why) => {
            warn!(why = %why.message, "вложения черновика не перечитаны после изменения");
            None
        }
    }
}

/// Дописывает к телу обновляемого черновика цитату оригинала (ответ, ответ всем, пересылка):
/// `UpdateItem` заменяет тело целиком, а сервер цитирует только при создании. Цитата — из
/// `original_quote`. Не вышло (сервер не ответил, оригинала больше нет) — ошибка: без цитаты
/// черновик не обновляется, иначе тело с цитатой сервера молча заменилось бы одним новым текстом.
/// Текст при этом остаётся в окне и в локальном черновике.
async fn append_quote(state: &AppState, sync: &SyncState, transport: &impl MailTransport, cs: &ComposeState, req: &ComposeRequest, msg: &mut OutgoingMessage) -> Result<(), ComposeFail> {
    let (true, Some(pk), false) = (req.kind.needs_reference(), req.reference_item_pk, req.without_quote) else { return Ok(()) };
    let quote = original_quote(state, sync, transport, cs, pk).await?;
    msg.body_html.push_str(&quote);
    Ok(())
}

/// Цитата оригинала `pk` из памяти или с сервера: тело читается полностью (`ItemNormalizedBody`:
/// в базе только `UniqueBody`, без истории переписки) и запоминается в `ComposeState`.
async fn original_quote(state: &AppState, sync: &SyncState, transport: &impl MailTransport, cs: &ComposeState, pk: i64) -> Result<Arc<String>, ComposeFail> {
    if let Some(q) = cs.cached_quote(pk) {
        return Ok(q);
    }
    let reference = quote_reference(&state.store, pk)?;
    let got = step(sync, "GetItem", CREATE_TIMEOUT, transport.get_items(std::slice::from_ref(&reference), ItemFetchShape::WithFullBody)).await?;
    let q = Arc::new(quote_of(pk, got.into_iter().next())?);
    cs.remember_quote(pk, q.clone());
    Ok(q)
}

/// Идентификатор оригинала для цитаты; удалён в базе — отказ без повтора.
fn quote_reference(store: &Store, pk: i64) -> Result<ItemId, ComposeFail> {
    match store.item_ref(pk).map_err(err)? {
        Some(r) => Ok(r.id),
        None => {
            warn!(reference_pk = pk, "исходное письмо удалено: цитату не собрать, черновик не обновляется");
            Err(ComposeFail::original_gone())
        }
    }
}

/// Цитата из ответа `GetItem` оригинала. Оригинала нет на сервере (`ErrorItemNotFound`: удалён или
/// перенесён в другом клиенте) — отказ без повтора; другой отказ или пустой ответ — с повтором.
fn quote_of(pk: i64, got: Option<ItemResult>) -> Result<String, ComposeFail> {
    match got {
        Some(ItemResult::Ok(m)) => Ok(quote_from_full(&m)?),
        Some(ItemResult::Failed { code, .. }) if code == ITEM_NOT_FOUND => {
            warn!(reference_pk = pk, "исходного письма нет на сервере: цитату не собрать, черновик не обновляется");
            Err(ComposeFail::original_gone())
        }
        Some(ItemResult::Failed { code, .. }) => Err(ComposeFail::retryable(format!("Исходное письмо не прочитано с сервера ({code}) — цитату не собрать, черновик не обновлён"))),
        None => Err(ComposeFail::retryable("Сервер не вернул исходное письмо — цитату не собрать, черновик не обновлён")),
    }
}

/// Чем кончилась запись черновика с вложениями (`write_draft`), если не всем.
enum DraftFailure {
    /// Черновика на сервере нет или он не обновлён: ошибка команды.
    Before(ComposeFail),
    /// Черновик есть (`id`), а с вложениями не вышло; `touched` — вложения могли измениться.
    After { id: ItemId, why: String, touched: bool },
}

/// Записать письмо в черновик на сервере вместе с вложениями: нового — `CreateItem`, существующего
/// (`existing`) и найденного после оборванного `CreateItem` — цитата у ответа и `UpdateItem`
/// (`rewrite_draft`); затем сверка с сервером, новые файлы и удаление убранных. `prepared` — то,
/// что вернул `prepare`. Ключ черновика запоминается после каждого шага. Возвращает черновик и
/// признак, что вложения менялись.
async fn write_draft(state: &AppState, sync: &SyncState, transport: &impl MailTransport, cs: &ComposeState, req: &ComposeRequest, prepared: (ComposeKind, OutgoingMessage, Vec<OutgoingAttachment>), existing: Option<&str>) -> Result<(ItemId, bool), DraftFailure> {
    let (kind, mut msg, files) = prepared;
    let target = match existing {
        Some(sid) => DraftTarget::Existing(cs.item_id(sid)),
        None => create_draft_fresh(state, sync, transport, cs, kind, &msg, req).await.map_err(DraftFailure::Before)?,
    };
    let (mut current, files, removed) = match target {
        DraftTarget::Created(draft) => {
            cs.remember(&draft);
            (draft, files, Vec::new())
        }
        DraftTarget::Existing(item) => {
            let current = rewrite_draft(state, sync, transport, cs, req, &mut msg, &item).await.map_err(DraftFailure::Before)?;
            let (files, removed) = reconcile(sync, transport, &current, removed_ids(req), files).await;
            (current, files, removed)
        }
    };
    let touched = !files.is_empty() || !removed.is_empty();
    if !files.is_empty() {
        match step(sync, "CreateAttachment", ATTACH_TIMEOUT, transport.add_attachments(&current, &files)).await {
            Ok(id) => {
                cs.remember(&id);
                current = id;
            }
            Err(why) => return Err(DraftFailure::After { id: current, why: why.message, touched }),
        }
    }
    if !removed.is_empty() {
        match step(sync, "DeleteAttachment", CREATE_TIMEOUT, transport.delete_attachments(&removed)).await {
            Ok(id) => {
                cs.remember(&id);
                current = id;
            }
            Err(why) => return Err(DraftFailure::After { id: current, why: why.message, touched }),
        }
    }
    Ok((current, touched))
}

/// Черновик, в который пишется письмо.
enum DraftTarget {
    /// Только что создан `CreateItem`: текст в нём текущий, вложений ещё нет.
    Created(ItemId),
    /// Уже есть на сервере: существующий или найденный после `CreateItem` с неизвестным исходом (в
    /// нём текст той попытки, а не текущий). Текущий текст пишется `rewrite_draft`.
    Existing(ItemId),
}

/// Текущее письмо в черновик на сервере: цитата у ответа и пересылки (`append_quote`) и
/// `UpdateItem`. После удачной записи у композера есть свой черновик — оборванные попытки
/// `CreateItem` больше не ищутся.
async fn rewrite_draft(state: &AppState, sync: &SyncState, transport: &impl MailTransport, cs: &ComposeState, req: &ComposeRequest, msg: &mut OutgoingMessage, item: &ItemId) -> Result<ItemId, ComposeFail> {
    append_quote(state, sync, transport, cs, req, msg).await?;
    let current = step(sync, "UpdateItem", CREATE_TIMEOUT, transport.update_draft(item, msg)).await?;
    cs.remember(&current);
    cs.clear_lost(local_id_of(req));
    Ok(current)
}

/// Тот же вид письма с другим идентификатором оригинала.
fn with_reference(kind: ComposeKind, reference: ItemId) -> ComposeKind {
    match kind {
        ComposeKind::New => ComposeKind::New,
        ComposeKind::Reply { .. } => ComposeKind::Reply { reference },
        ComposeKind::ReplyAll { .. } => ComposeKind::ReplyAll { reference },
        ComposeKind::Forward { .. } => ComposeKind::Forward { reference },
    }
}

/// Текущий идентификатор оригинала с сервера (`GetItem` только с идентификатором). Ключ в базе
/// бывает усечённым (после нашей же пометки «прочитано») или устаревшим (письмо изменили в OWA,
/// на него уже ответили): с таким `ReferenceItemId` сервер отвечает `ErrorStaleObject`
/// (docs/obkatka.md п. 19). Полный ключ записывается в базу.
async fn fresh_reference(state: &AppState, sync: &SyncState, transport: &impl MailTransport, pk: i64) -> Result<ItemId, ComposeFail> {
    let stored = state.store.item_ref(pk).map_err(err)?.ok_or_else(|| "Исходное письмо удалено".to_string())?.id;
    let mut got = step(sync, "GetItem", REREAD_TIMEOUT, transport.refresh_item_ids(std::slice::from_ref(&stored))).await?;
    match got.pop() {
        Some(WriteResult::Ok { id }) => {
            info!(reference_pk = pk, stored_ck_len = stored.change_key_len(), fresh_ck_len = id.change_key_len(), "ключ исходного письма перечитан");
            if id.has_full_change_key()
                && let Err(e) = state.store.update_item_id(pk, &id)
            {
                warn!(reference_pk = pk, error = %e, "перечитанный ключ не записан в базу");
            }
            Ok(id)
        }
        Some(WriteResult::Failed { code, .. }) if code == ITEM_NOT_FOUND => Err(ComposeFail::fixed("Исходное письмо удалено на сервере")),
        Some(WriteResult::Failed { code, message, .. }) => {
            warn!(reference_pk = pk, code, message = %message, "сервер не отдал идентификатор исходного письма");
            Err(ComposeFail::retryable(format!("Исходное письмо не прочитано с сервера ({code})")))
        }
        None => Err(ComposeFail::retryable("Сервер не вернул идентификатор исходного письма")),
    }
}

/// Код отказа сервера «письма нет».
const ITEM_NOT_FOUND: &str = "ErrorItemNotFound";

/// `CreateItem` черновика. У ответа и пересылки ключ оригинала (`reference_item_pk`) сначала
/// перечитывается с сервера; если сервер всё же ответил `ErrorStaleObject` (письмо изменилось
/// между запросами), ещё одна попытка с заново перечитанным ключом, и только потом ошибка.
/// Исход `CreateItem` неизвестен (обрыв после отправки, неразобранный ответ или шлюз, таймаут):
/// попытка запоминается за локальным черновиком композера, и её черновик ищется в «Черновиках»
/// (`adopt_lost_draft`) — сразу и перед следующим `CreateItem` того же композера (сервер мог
/// закончить уже после поиска), чтобы повтор не создал второй. Найденный черновик — `Existing`:
/// в нём текст той попытки, текущий записывает вызывающий. Не нашёлся — ошибка с повтором.
async fn create_draft_fresh(state: &AppState, sync: &SyncState, transport: &impl MailTransport, cs: &ComposeState, kind: ComposeKind, msg: &OutgoingMessage, req: &ComposeRequest) -> Result<DraftTarget, ComposeFail> {
    let reference_pk = req.reference_item_pk;
    let local = local_id_of(req);
    let lost = cs.lost(local);
    if !lost.is_empty()
        && let Some(found) = adopt_lost_draft(state, transport, cs, local, &lost).await
    {
        info!(?reference_pk, attempts = lost.len(), "черновик прошлой попытки найден в «Черновиках», продолжаем с ним");
        return Ok(DraftTarget::Existing(found));
    }
    let mut kind = match reference_pk {
        Some(pk) if kind.reference().is_some() => with_reference(kind, fresh_reference(state, sync, transport, pk).await?),
        _ => kind,
    };
    let mut stale_retried = false;
    loop {
        let started = Timestamp::now();
        let fail = match tokio::time::timeout(CREATE_TIMEOUT, transport.create_draft(&kind, msg)).await {
            Ok(Ok(id)) => {
                cs.clear_lost(local);
                return Ok(DraftTarget::Created(id));
            }
            Ok(Err(e)) if is_stale_object(&e) && !stale_retried && reference_pk.is_some() => {
                let pk = reference_pk.expect("проверено выше");
                warn!(reference_pk = pk, ck_len = kind.reference().and_then(ItemId::change_key_len), "CreateItem: ErrorStaleObject после перечитывания ключа, ещё одна попытка");
                kind = with_reference(kind, fresh_reference(state, sync, transport, pk).await?);
                stale_retried = true;
                continue;
            }
            Ok(Err(e)) if outcome_unknown(&e) => {
                warn!(step = "CreateItem", ?reference_pk, error = %e.chain(), "CreateItem без ответа: исход неизвестен, ищу черновик в «Черновиках»");
                sync.invalidate_on(&e).await;
                ComposeFail::from_transport(&e)
            }
            Ok(Err(e)) => {
                warn!(step = "CreateItem", ?reference_pk, error = %e.chain(), "шаг композера отклонён");
                sync.invalidate_on(&e).await;
                return Err(ComposeFail::from_transport(&e));
            }
            Err(_) => {
                warn!(step = "CreateItem", ?reference_pk, limit_s = CREATE_TIMEOUT.as_secs(), "CreateItem не уложился в таймаут: исход неизвестен, ищу черновик в «Черновиках»");
                ComposeFail::timed_out(CREATE_TIMEOUT)
            }
        };
        let attempt = LostCreate::of(msg, started);
        cs.remember_lost(local, attempt.clone());
        let attempts: Vec<LostCreate> = lost.iter().cloned().chain(std::iter::once(attempt)).collect();
        return match adopt_lost_draft(state, transport, cs, local, &attempts).await {
            Some(found) => {
                info!(?reference_pk, ck_len = found.change_key_len(), "черновик найден в «Черновиках» после обрыва, продолжаем с ним");
                Ok(DraftTarget::Existing(found))
            }
            None => Err(fail),
        };
    }
}

/// Попытка `CreateItem` с неизвестным исходом: когда начата и что ушло на сервер (тема и адреса).
/// По ним черновик этой попытки отличается от других черновиков с той же темой.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LostCreate {
    started: Timestamp,
    subject: String,
    /// Адреса «Кому», «Копия», «Скрытая» (`address_sets`).
    recipients: [Vec<String>; 3],
}

impl LostCreate {
    fn of(msg: &OutgoingMessage, started: Timestamp) -> Self {
        Self { started, subject: msg.subject.trim().to_string(), recipients: address_sets(&msg.to, &msg.cc, &msg.bcc) }
    }

    /// Письмо из списка «Черновиков» может быть черновиком этой попытки: та же тема, изменено не
    /// раньше начала попытки (с запасом на расхождение часов).
    fn may_be(&self, m: &MessageMeta) -> bool {
        let since = self.started - jiff::SignedDuration::from_secs(60);
        m.subject.as_deref().unwrap_or("").trim() == self.subject && draft_stamp(m).is_some_and(|t| t >= since)
    }

    /// Черновик с сервера — этой попытки: та же тема и те же адреса во всех трёх полях.
    fn is(&self, d: &DraftContent) -> bool {
        d.subject.trim() == self.subject && address_sets(&d.to, &d.cc, &d.bcc) == self.recipients
    }
}

/// Адреса «Кому», «Копия», «Скрытая» для сравнения: без регистра и пробелов по краям, без повторов,
/// по порядку. Участник без адреса — пустая строка: с адресом его не спутать.
fn address_sets(to: &[Participant], cc: &[Participant], bcc: &[Participant]) -> [Vec<String>; 3] {
    let set = |ps: &[Participant]| {
        let mut v: Vec<String> = ps.iter().map(|p| p.email.as_deref().unwrap_or("").trim().to_lowercase()).collect();
        v.sort();
        v.dedup();
        v
    };
    [set(to), set(cc), set(bcc)]
}

fn draft_stamp(m: &MessageMeta) -> Option<Timestamp> {
    m.last_modified_at.or(m.received_at).or(m.sent_at)
}

/// Черновик, который сервер мог создать по `CreateItem` без ответа (`lost` — такие попытки
/// композера): среди последних 20 в «Черновиках» (`FindItem` § 5m) кандидаты по теме и времени
/// (`adoption_candidates`), у самых свежих `GetItem` сверяет адреса. Черновики других композеров
/// этого запуска и других локальных черновиков (`local` — свой) не берутся: чужое письмо не должно
/// уйти вместо этого или затереться им. Сбой поиска — `None`.
async fn adopt_lost_draft(state: &AppState, transport: &impl MailTransport, cs: &ComposeState, local: Option<&str>, lost: &[LostCreate]) -> Option<ItemId> {
    let folder = FolderRef::Distinguished(DistinguishedFolder::Drafts);
    let metas = match tokio::time::timeout(REREAD_TIMEOUT, transport.find_recent_items(&folder, 20)).await {
        Ok(Ok(metas)) => metas,
        Ok(Err(e)) => {
            warn!(error = %e.chain(), "черновик после обрыва не найден: FindItem отклонён");
            return None;
        }
        Err(_) => {
            warn!("черновик после обрыва не найден: FindItem не уложился в таймаут");
            return None;
        }
    };
    let mut taken = cs.known_ids();
    match state.store.local_draft_server_ids(state.account, local) {
        Ok(ids) => taken.extend(ids),
        Err(e) => {
            warn!(error = %e, "черновик после обрыва не ищется: локальные черновики не прочитаны");
            return None;
        }
    }
    for m in adoption_candidates(&metas, lost, &taken).into_iter().take(ADOPT_CHECKS) {
        match tokio::time::timeout(REREAD_TIMEOUT, transport.get_draft(&m.item_id)).await {
            Ok(Ok(d)) if lost.iter().any(|a| a.may_be(m) && a.is(&d)) => return Some(d.id),
            Ok(Ok(_)) => debug!("черновик с той же темой — не этой попытки: адреса другие"),
            Ok(Err(e)) => warn!(error = %e.chain(), "черновик-кандидат после обрыва не прочитан"),
            Err(_) => warn!("черновик-кандидат после обрыва не прочитан за таймаут"),
        }
    }
    None
}

/// Кандидаты на черновик оборванных попыток среди последних писем «Черновиков», свежие первыми:
/// похожие на одну из попыток (`LostCreate::may_be`) и не занятые (`taken`). Чистая часть
/// `adopt_lost_draft`; адреса сверяются уже по `GetItem`.
fn adoption_candidates<'a>(metas: &'a [MessageMeta], lost: &[LostCreate], taken: &HashSet<String>) -> Vec<&'a MessageMeta> {
    let mut out: Vec<&MessageMeta> = metas.iter().filter(|m| !taken.contains(&m.item_id.id) && lost.iter().any(|a| a.may_be(m))).collect();
    out.sort_by_key(|m| std::cmp::Reverse(draft_stamp(m)));
    out
}

/// Ошибка транспорта на `CreateItem` и `SendItem`, при которой запрос мог быть выполнен: обрыв
/// связи (в том числе при чтении ответа после того, как сервер выполнил запрос) и неразобранный
/// ответ или шлюз (ответ был, но что сервер сделал — неизвестно). Явный отказ сервера (`Server`,
/// вход, лимиты) и неустановленное соединение — нет.
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
        return Ok(ComposeContext { kind, reference_item_pk: None, to: vec![], cc: vec![], bcc: vec![], subject: String::new(), original: None, own_addresses, signature, body_html: None, draft: None });
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
    Ok(ComposeContext { kind, reference_item_pk: Some(pk), to, cc, bcc: vec![], subject, original: Some(original), own_addresses, signature, body_html: None, draft: None })
}

/// Сверка с черновиком на сервере перед загрузкой и удалением вложений (`reconcile_with`): повтор
/// после оборванной загрузки не прикладывает загруженное второй раз, а повтор после удачного
/// `DeleteAttachment` (или вложение, убранное в OWA) не упирается в отказ по уже несуществующему
/// вложению. Черновик прочитать не удалось — всё как есть.
async fn reconcile(sync: &SyncState, transport: &impl MailTransport, item: &ItemId, removed: Vec<String>, files: Vec<OutgoingAttachment>) -> (Vec<OutgoingAttachment>, Vec<String>) {
    if files.is_empty() && removed.is_empty() {
        return (files, removed);
    }
    match step(sync, "GetItem", CREATE_TIMEOUT, transport.get_draft(item)).await {
        Ok(draft) => reconcile_with(&draft.attachments, removed, files),
        Err(why) => {
            warn!(why = %why.message, "черновик не прочитан перед изменением вложений, они меняются без сверки");
            (files, removed)
        }
    }
}

/// Отправка: проверки → черновик → вложения → `SendItem`. Отказ до создания (или обновления)
/// черновика — ошибка команды (окно покажет её с «Повторить»); после — `draft_saved` с текстом и
/// ссылкой на черновик (`SendReport::draft`), чтобы окно продолжило с ним. Если черновик уже есть
/// на сервере (`draft_server_id`), он обновляется вместо создания, новые файлы прикладываются,
/// убранные удаляются, и уходит он же.
#[tauri::command]
pub(crate) async fn compose_send(req: ComposeRequest, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cs: State<'_, ComposeState>) -> Result<SendReport, ComposeFail> {
    const NOT_SENT: &str = "Письмо не отправлено, текст остался в этом окне";
    let _busy = cs.sending.try_lock().map_err(|_| ComposeFail::fixed("Отправка уже идёт"))?;
    let (kind, msg, files) = prepare(&state, &req, true).await?;
    let existing = draft_id_of(&req);
    let attached = files.len();
    let picked: Vec<PickedFile> = req.attachments.iter().zip(&files).map(|(path, f)| PickedFile { path: path.clone(), name: f.name.clone(), size: f.bytes.len() as u64 }).collect();
    let (to, cc, bcc) = (msg.to.len(), msg.cc.len(), msg.bcc.len());
    let started = Instant::now();
    let transport = sync.transport().await.map_err(|e| ComposeFail::connect(&e).prefixed(NOT_SENT))?;
    let (current, touched) = match write_draft(&state, &sync, &transport, &cs, &req, (kind, msg, files), existing.as_deref()).await {
        Ok(v) => v,
        Err(DraftFailure::Before(why)) => {
            warn!(kind = ?req.kind, ?existing, ms = started.elapsed().as_millis() as u64, why = %why.message, retry = why.retry, "отправка: черновик не создан или не обновлён");
            return Err(why.prefixed(NOT_SENT));
        }
        Err(DraftFailure::After { id, why, touched }) => {
            let files = if touched { server_files(&sync, &transport, &id).await } else { None };
            return Ok(kept(why).with_draft(draft_ref(&state, &id), files));
        }
    };
    // Окно может закрыться (или приложение упасть) до ответа на SendItem: локальный черновик
    // должен знать черновик на сервере, иначе повтор из полосы «Несохранённое письмо» отправит письмо второй раз.
    if let Some(local) = local_id_of(&req)
        && let Err(e) = LocalDraftDto::sending(local, &req, &current.id, picked).to_row().and_then(|row| state.store.local_draft_save(state.account, &row).map_err(err))
    {
        warn!(error = %e, "черновик на сервере не записан в локальный черновик перед отправкой");
    }
    match tokio::time::timeout(SEND_TIMEOUT, transport.send_draft(&current)).await {
        Ok(Ok(())) => {
            // Сервер перенёс черновик в «Отправленные» под новым идентификатором.
            cs.forget(&current.id);
            if let Some(sid) = existing.as_deref() {
                cs.forget(sid);
                drop_local_draft(&app, &state.store, state.account, sid);
            }
            forget_local_draft(&state.store, req.local_draft_id.as_deref());
            info!(kind = ?req.kind, to, cc, bcc, attachments = attached, existing_draft = existing.is_some(), ms = started.elapsed().as_millis() as u64, "письмо отправлено");
            Ok(SendReport { sent: true, ..SendReport::default() })
        }
        // Обрыв связи или неразобранный ответ: запрос мог дойти, письмо могло уйти.
        Ok(Err(e)) if outcome_unknown(&e) => {
            warn!(error = %e.chain(), ms = started.elapsed().as_millis() as u64, "SendItem без ответа, исход неизвестен");
            sync.invalidate_on(&e).await;
            Ok(maybe_sent(describe_transport(&e)).with_draft(draft_ref(&state, &current), None))
        }
        Ok(Err(e)) => {
            warn!(error = %e.chain(), ms = started.elapsed().as_millis() as u64, "SendItem отклонён");
            sync.invalidate_on(&e).await;
            let files = if touched { server_files(&sync, &transport, &current).await } else { None };
            Ok(kept(describe_transport(&e)).with_draft(draft_ref(&state, &current), files))
        }
        // Ответа на SendItem нет: письмо могло и уйти — сказать «не отправлено» было бы неправдой.
        Err(_) => {
            warn!("SendItem не уложился в таймаут, исход неизвестен");
            Ok(maybe_sent(format!("Сервер не ответил за {} с", SEND_TIMEOUT.as_secs())).with_draft(draft_ref(&state, &current), None))
        }
    }
}

/// Открыть черновик из «Черновиков»: тема, получатели (и скрытые), тело и вложения. Текст берётся
/// из локальной базы; если полного тела там нет (черновик создан в OWA и ещё не догружен или
/// обрезан) — читается с сервера (`GetItem`), и тогда тема, получатели и вложения тоже оттуда.
/// Если и сервер отдал тело обрезанным (больше `MaximumBodySize`), черновик не открывается:
/// первое же сохранение записало бы обрезанный текст обратно. Ссылки `cid:` в теле остаются,
/// картинки из кэша окно показывает по `DraftInfoDto::inline_images`.
#[tauri::command]
pub(crate) async fn compose_open_draft(item_pk: i64, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cs: State<'_, ComposeState>) -> CmdResult<ComposeContext> {
    let store = state.store.clone();
    let account = state.account;
    let own_addresses = own_addresses_of(&store, account, &state.user)?;
    let signature = signature_of(&store, account)?;
    let local = tauri::async_runtime::spawn_blocking(move || local_draft(&store, item_pk)).await.map_err(err)??;
    let inline_images = local.inline_images;
    let (id, subject, to, cc, bcc, body_html, attachments) = match local.body_html {
        Some(body_html) => {
            let attachments: Vec<DraftAttachmentDto> = local
                .files
                .iter()
                .map(|a| DraftAttachmentDto { attachment_pk: Some(a.attachment_pk), server_id: a.server_id.clone(), name: a.name.clone(), size: a.size, content_type: a.content_type.clone() })
                .collect();
            (local.id, local.subject, local.to, local.cc, local.bcc, body_html, attachments)
        }
        None => {
            let transport = sync.transport().await.map_err(|e| e.to_string())?;
            let draft = step(&sync, "GetItem", CREATE_TIMEOUT, transport.get_draft(&local.id)).await.map_err(|f| f.message)?;
            if draft.is_truncated {
                warn!(item_pk, body_chars = draft.body_html.chars().count(), "черновик с сервера обрезан, в композере не открывается");
                return Err("Черновик слишком большой для правки в AIOWA: сервер отдал его обрезанным, и сохранение записало бы обрезанный текст обратно. Откройте его в OWA".into());
            }
            let people = |ps: &[Participant]| ps.iter().filter_map(|p| compose_recipient(p.name.as_deref(), p.email.as_deref())).collect::<Vec<_>>();
            let body_html = crate::sanitize_keep_cid(&draft.body_html);
            let attachments = draft.file_attachments().map(|a| DraftAttachmentDto::of(a, local.files.iter().find(|r| r.server_id == a.id).map(|r| r.attachment_pk))).collect();
            let (to, cc, bcc) = (people(&draft.to), people(&draft.cc), people(&draft.bcc));
            (draft.id, draft.subject, to, cc, bcc, body_html, attachments)
        }
    };
    cs.remember(&id);
    info!(item_pk, to = to.len(), cc = cc.len(), bcc = bcc.len(), attachments = attachments.len(), "черновик открыт в композере");
    Ok(ComposeContext {
        kind: ComposeKindDto::New,
        reference_item_pk: None,
        to,
        cc,
        bcc,
        subject,
        original: None,
        own_addresses,
        signature,
        body_html: Some(body_html),
        draft: Some(DraftInfoDto { item_pk: Some(item_pk), server_id: id.id, change_key: id.change_key, attachments, inline_images }),
    })
}

/// Автосохранение: первый раз создаёт черновик на сервере, дальше обновляет тему, тело и
/// получателей (`UpdateItem`); вложения не трогает — их меняют открытие и отправка. Получатели не
/// обязательны. Возвращает серверный идентификатор для следующих вызовов и отправки.
#[tauri::command]
pub(crate) async fn compose_autosave(req: ComposeRequest, draft_server_id: Option<String>, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cs: State<'_, ComposeState>) -> Result<DraftRef, ComposeFail> {
    let _busy = cs.sending.try_lock().map_err(|_| ComposeFail::fixed("Отправка уже идёт"))?;
    let existing = draft_server_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from).or_else(|| draft_id_of(&req));
    let mut msg = outgoing(&req, false)?;
    let started = Instant::now();
    let transport = sync.transport().await.map_err(|e| ComposeFail::connect(&e))?;
    let target = match existing.as_deref() {
        None => {
            let kind = compose_kind(&state.store, req.kind, req.reference_item_pk, false)?;
            create_draft_fresh(&state, &sync, &transport, &cs, kind, &msg, &req).await?
        }
        Some(sid) => DraftTarget::Existing(cs.item_id(sid)),
    };
    let id = match target {
        DraftTarget::Created(id) => {
            // Цитата оригинала — сразу, пока он есть: следующее обновление черновика без неё не
            // обойдётся, а оригинал до него могут удалить или перенести.
            if let (true, Some(pk)) = (req.kind.needs_reference(), req.reference_item_pk)
                && let Err(why) = original_quote(&state, &sync, &transport, &cs, pk).await
            {
                warn!(why = %why.message, "цитата оригинала не прочитана заранее");
            }
            id
        }
        DraftTarget::Existing(item) => rewrite_draft(&state, &sync, &transport, &cs, &req, &mut msg, &item).await?,
    };
    cs.remember(&id);
    info!(kind = ?req.kind, updated = existing.is_some(), to = msg.to.len(), cc = msg.cc.len(), bcc = msg.bcc.len(), ms = started.elapsed().as_millis() as u64, "черновик автосохранён");
    Ok(draft_ref(&state, &id))
}

/// Локальный черновик композера: записывается окном раз в одну-две секунды после правок, чтобы
/// текст письма пережил падение или закрытие окна при любой связи (п. 22 обкатки).
#[tauri::command]
pub(crate) fn compose_local_save(draft: LocalDraftDto, state: State<'_, AppState>) -> CmdResult<()> {
    let row = draft.to_row()?;
    if row.draft_id.is_empty() {
        return Err("Не указан идентификатор локального черновика".into());
    }
    state.store.local_draft_save(state.account, &row).map_err(err)
}

/// Убрать локальный черновик: письмо отправлено, сохранено на сервере или отброшено.
#[tauri::command]
pub(crate) fn compose_local_drop(draft_id: String, state: State<'_, AppState>) -> CmdResult<()> {
    forget_local_draft(&state.store, Some(&draft_id));
    Ok(())
}

/// Локальные черновики, оставшиеся с прошлого запуска, свежие первыми: окно предлагает открыть их.
#[tauri::command]
pub(crate) fn compose_local_list(state: State<'_, AppState>) -> CmdResult<Vec<LocalDraftDto>> {
    Ok(state.store.local_drafts(state.account).map_err(err)?.into_iter().map(LocalDraftDto::from_row).collect())
}

fn forget_local_draft(store: &Store, draft_id: Option<&str>) {
    let Some(id) = draft_id.map(str::trim).filter(|s| !s.is_empty()) else { return };
    match store.local_draft_drop(id) {
        Ok(true) => debug!("локальный черновик убран"),
        Ok(false) => {}
        Err(e) => warn!(error = %e, "локальный черновик не убран"),
    }
}

/// Удалить черновик: перенести в «Удалённые» (`DeleteItem` с `MoveToDeletedItems`, как обычное
/// удаление и «Отменить» в OWA — из «Удалённых» его можно вернуть) и убрать из локальной базы,
/// если он там есть. Черновика на сервере уже нет (удалён в OWA) — не ошибка.
#[tauri::command]
pub(crate) async fn compose_discard_draft(draft_server_id: String, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cs: State<'_, ComposeState>) -> CmdResult<()> {
    let _busy = cs.sending.try_lock().map_err(|_| "Отправка уже идёт".to_string())?;
    let sid = draft_server_id.trim();
    if sid.is_empty() {
        return Err("Не указан черновик".into());
    }
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let item = cs.item_id(sid);
    let results = step(&sync, "DeleteItem", CREATE_TIMEOUT, transport.delete_items(std::slice::from_ref(&item), DeleteMode::ToDeletedItems)).await.map_err(|f| f.message)?;
    if let Some(WriteResult::Failed { code, .. }) = results.first()
        && code != "ErrorItemNotFound"
    {
        warn!(code = %code, "сервер не удалил черновик");
        return Err(format!("Сервер отказал: {code}"));
    }
    cs.forget(sid);
    drop_local_draft(&app, &state.store, state.account, sid);
    info!("черновик перенесён в «Удалённые»");
    Ok(())
}

/// Сохранить черновик на сервере вместе с файлами, без отправки (кнопка «Сохранить черновик»,
/// Ctrl+S, «Сохранить» при отмене): нового — `CreateItem`, существующего (`draft_server_id`) —
/// `UpdateItem` с цитатой у ответа; затем новые файлы и удаление убранных, как у отправки.
/// Получатели не обязательны. В ответе — черновик (`draft`) и, если вложения менялись, их список
/// на сервере; если текст сохранён, а с вложениями не вышло, — `message` с причиной.
#[tauri::command]
pub(crate) async fn compose_save_draft(req: ComposeRequest, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cs: State<'_, ComposeState>) -> Result<SendReport, ComposeFail> {
    const NOT_SAVED: &str = "Черновик не сохранён на сервере, текст остался в этом окне";
    let _busy = cs.sending.try_lock().map_err(|_| ComposeFail::fixed("Отправка уже идёт"))?;
    let (kind, msg, files) = prepare(&state, &req, false).await?;
    let existing = draft_id_of(&req);
    let attached = files.len();
    let started = Instant::now();
    let transport = sync.transport().await.map_err(|e| ComposeFail::connect(&e).prefixed(NOT_SAVED))?;
    let (id, touched, why) = match write_draft(&state, &sync, &transport, &cs, &req, (kind, msg, files), existing.as_deref()).await {
        Ok((id, touched)) => (id, touched, None),
        Err(DraftFailure::Before(why)) => {
            warn!(kind = ?req.kind, ?existing, ms = started.elapsed().as_millis() as u64, why = %why.message, retry = why.retry, "сохранение черновика не удалось");
            return Err(why.prefixed(NOT_SAVED));
        }
        Err(DraftFailure::After { id, why, touched }) => (id, touched, Some(why)),
    };
    let files = if touched { server_files(&sync, &transport, &id).await } else { None };
    info!(kind = ?req.kind, attachments = attached, updated = existing.is_some(), attachments_failed = why.is_some(), ms = started.elapsed().as_millis() as u64, "черновик сохранён на сервере");
    let message = why.map(|w| format!("Текст черновика сохранён, а вложения изменить не вышло. {w}"));
    Ok(SendReport { draft_saved: true, message, ..SendReport::default() }.with_draft(draft_ref(&state, &id), files))
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
    let found = step(&sync, "GetUserConfiguration", CREATE_TIMEOUT, transport.user_signature()).await.map_err(|f| f.message)?;
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
            draft_server_id: None,
            removed_attachment_ids: vec![],
            local_draft_id: None,
            without_quote: false,
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

        // Ссылки на инлайновые вложения черновика уходят на сервер как были, а не теряются.
        let pictures = ComposeRequest { body_html: r#"<p>Лого: <img src="cid:image001.png@01DA"> <img src="https://x.ru/a.png"></p>"#.into(), ..bad };
        let body = outgoing(&ComposeRequest { bcc: vec![], ..pictures }, false).unwrap().body_html;
        assert!(body.contains(r#"src="cid:image001.png@01DA""#) && body.contains(r#"src="https://x.ru/a.png""#), "{body}");
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
    fn compose_fail_flags_follow_transport_error() {
        let net = ComposeFail::from_transport(&TransportError::Network("обрыв".into()));
        assert!(net.retry && net.network);
        let conn = ComposeFail::from_transport(&TransportError::Connect("нет соединения".into()));
        assert!(conn.retry && conn.network);
        let stale = ComposeFail::from_transport(&TransportError::Server { code: "ErrorStaleObject".into(), message: String::new() });
        assert!(stale.retry && !stale.network, "ключ перечитывается, повтор поможет");
        let rejected = ComposeFail::from_transport(&TransportError::Server { code: "ErrorInvalidRecipients".into(), message: String::new() });
        assert!(!rejected.retry && !rejected.network);
        let auth = ComposeFail::from_transport(&TransportError::AuthFailed("x".into()));
        assert!(!auth.retry);
        let fixed: ComposeFail = "Неверный адрес".to_string().into();
        assert!(!fixed.retry && !fixed.network);
        assert_eq!(ComposeFail::timed_out(Duration::from_secs(30)).prefixed("Письмо не отправлено, текст остался в этом окне").message, "Письмо не отправлено, текст остался в этом окне. Сервер не ответил за 30 с, повторите попытку");
        let v = serde_json::to_value(&net).unwrap();
        assert_eq!((v["retry"].as_bool(), v["network"].as_bool()), (Some(true), Some(true)));
    }

    fn person(email: &str) -> Participant {
        Participant { name: Some("Имя".into()), email: Some(email.into()), routing_type: Some("SMTP".into()) }
    }

    fn letter(subject: &str, to: &[&str], cc: &[&str], bcc: &[&str]) -> OutgoingMessage {
        let ps = |xs: &[&str]| xs.iter().map(|e| person(e)).collect::<Vec<_>>();
        OutgoingMessage { to: ps(to), cc: ps(cc), bcc: ps(bcc), subject: subject.into(), body_html: "<p>текст</p>".into(), importance: Importance::Normal }
    }

    fn server_draft(id: &str, msg: &OutgoingMessage) -> DraftContent {
        DraftContent { id: ItemId { id: id.into(), change_key: Some("full".into()) }, subject: msg.subject.clone(), body_html: msg.body_html.clone(), to: msg.to.clone(), cc: msg.cc.clone(), bcc: msg.bcc.clone(), importance: Importance::Normal, attachments: vec![], is_truncated: false }
    }

    fn draft_meta(id: &str, subject: &str, stamp: Option<Timestamp>) -> MessageMeta {
        MessageMeta {
            item_id: ItemId { id: id.into(), change_key: Some("k".into()) },
            parent_folder_id: None,
            conversation_id: None,
            internet_message_id: None,
            item_class: None,
            subject: Some(subject.into()),
            from: None,
            sender: None,
            display_to: None,
            received_at: None,
            sent_at: None,
            last_modified_at: stamp,
            is_read: true,
            is_draft: true,
            has_attachments: false,
            importance: Importance::Normal,
            flag: aiowa_core::FlagStatus::NotFlagged,
            size: None,
            preview: None,
            last_verb: None,
            last_verb_at: None,
        }
    }

    #[test]
    fn adoption_candidates_are_fresh_same_subject_and_not_taken() {
        let started = Timestamp::now();
        let ago = |s: i64| Some(started - jiff::SignedDuration::from_secs(s));
        let lost = [LostCreate::of(&letter("Тема", &["a@y.ru"], &[], &[]), started)];
        let metas = vec![
            draft_meta("old", "Тема", ago(3600)),
            draft_meta("other", "Другая", ago(5)),
            draft_meta("fresh", "Тема ", ago(10)),
            draft_meta("fresher", "Тема", ago(2)),
            draft_meta("nostamp", "Тема", None),
            draft_meta("taken", "Тема", ago(1)),
        ];
        let taken: HashSet<String> = ["taken".to_string()].into();
        let ids = |v: Vec<&MessageMeta>| v.into_iter().map(|m| m.item_id.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(adoption_candidates(&metas, &lost, &taken)), vec!["fresher", "fresh"], "свежие первыми, пробелы в теме не в счёт; старый, без отметки времени и занятый другим композером — нет");
        assert!(adoption_candidates(&metas, &[], &taken).is_empty(), "без оборванных попыток не ищется ничего");
        // Попытка, начатая раньше (прошлое автосохранение), видит и черновики, изменённые с тех пор.
        let earlier = [LostCreate::of(&letter("Тема", &[], &[], &[]), started - jiff::SignedDuration::from_secs(4000))];
        assert_eq!(ids(adoption_candidates(&metas, &earlier, &HashSet::new())), vec!["taken", "fresher", "fresh", "old"]);
    }

    #[test]
    fn lost_attempt_matches_only_a_draft_with_the_same_subject_and_addresses() {
        let msg = letter(" RE: Смета ", &["A@y.ru", "b@y.ru"], &["v@z.ru"], &["h@y.ru"]);
        let attempt = LostCreate::of(&msg, Timestamp::now());
        assert_eq!(attempt.subject, "RE: Смета");
        // Тот же черновик с сервера: регистр, порядок и повторы адресов не в счёт.
        let mut same = server_draft("D1", &msg);
        same.to = vec![person("b@y.ru"), person(" a@Y.ru "), person("b@y.ru")];
        assert!(attempt.is(&same));
        // Черновик другого ответа с той же темой: другие «Кому» — не наш (его текст не должен уйти вместо этого письма).
        assert!(!attempt.is(&server_draft("D2", &letter("RE: Смета", &["a@y.ru"], &["v@z.ru"], &["h@y.ru"]))));
        assert!(!attempt.is(&server_draft("D3", &letter("RE: Смета", &["a@y.ru", "b@y.ru"], &["v@z.ru", "x@z.ru"], &["h@y.ru"]))), "лишний в копии");
        assert!(!attempt.is(&server_draft("D4", &letter("RE: Смета", &["a@y.ru", "b@y.ru"], &["v@z.ru", "h@y.ru"], &[]))), "скрытый перенесён в копию");
        assert!(!attempt.is(&server_draft("D5", &letter("RE: Другая", &["a@y.ru", "b@y.ru"], &["v@z.ru"], &["h@y.ru"]))));
        let mut no_email = server_draft("D6", &msg);
        no_email.bcc = vec![Participant { name: Some("h@y.ru".into()), email: None, routing_type: None }];
        assert!(!attempt.is(&no_email), "участник без адреса не равен адресу");
    }

    #[test]
    fn lost_attempts_are_kept_per_local_draft() {
        let cs = ComposeState::default();
        let attempt = |subject: &str| LostCreate::of(&letter(subject, &[], &[], &[]), Timestamp::now());
        assert!(cs.lost(Some("L1")).is_empty());
        cs.remember_lost(None, attempt("без композера"));
        cs.remember_lost(Some("L1"), attempt("первая"));
        cs.remember_lost(Some("L2"), attempt("другой композер"));
        assert_eq!(cs.lost(None), vec![], "без локального черновика попытка не помнится");
        assert_eq!(cs.lost(Some("L1")).iter().map(|a| a.subject.as_str()).collect::<Vec<_>>(), vec!["первая"]);
        for i in 0..LOST_KEPT {
            cs.remember_lost(Some("L1"), attempt(&format!("повтор {i}")));
        }
        let kept = cs.lost(Some("L1"));
        assert_eq!(kept.len(), LOST_KEPT);
        assert_eq!(kept[0].subject, "повтор 0", "самая старая вытесняется");
        cs.clear_lost(Some("L1"));
        assert!(cs.lost(Some("L1")).is_empty());
        assert_eq!(cs.lost(Some("L2")).len(), 1, "попытки другого композера не трогаются");
        // Черновики, с которыми работали композеры, известны: чужой композер их не подберёт.
        cs.remember(&ItemId { id: "D1".into(), change_key: None });
        assert!(cs.known_ids().contains("D1"));
    }

    #[test]
    fn update_of_a_missing_draft_says_it_may_be_sent_already() {
        let gone = TransportError::Server { code: ITEM_NOT_FOUND.into(), message: String::new() };
        let fail = step_fail("UpdateItem", &gone);
        assert_eq!(fail, ComposeFail::fixed(DRAFT_GONE), "черновика нет: повтор не поможет, а новое письмо могло бы стать дублем");
        assert!(fail.message.contains("«Отправленные»"));
        assert_eq!(step_fail("GetItem", &gone), ComposeFail::from_transport(&gone), "другие шаги — как раньше");
        let net = TransportError::Network("обрыв".into());
        assert_eq!(step_fail("UpdateItem", &net), ComposeFail::from_transport(&net));
    }

    #[test]
    fn original_gone_offers_to_continue_without_quote() {
        let fail = ComposeFail::original_gone();
        assert!(fail.original_gone && !fail.retry && !fail.network);
        assert_eq!(serde_json::to_value(&fail).unwrap()["original_gone"], true, "окно видит признак и предлагает «без цитаты»");
        assert!(!ComposeFail::fixed("x").original_gone && !ComposeFail::retryable("x").prefixed("y").original_gone);
        assert!(request(r#"{"kind":"reply","reference_item_pk":5,"without_quote":true}"#).without_quote);
        assert!(!request(r#"{"kind":"reply","reference_item_pk":5}"#).without_quote, "по умолчанию цитата обязательна");
    }

    #[test]
    fn quote_is_not_dropped_silently_when_the_original_is_gone() {
        let gone = quote_of(1, Some(ItemResult::Failed { id: ItemId { id: "O".into(), change_key: None }, code: ITEM_NOT_FOUND.into(), message: String::new() })).unwrap_err();
        assert_eq!(gone, ComposeFail::original_gone(), "оригинала нет на сервере: черновик не обновляется, повтор не поможет");
        assert!(gone.message.contains("цитату не собрать") && gone.message.contains("не обновлён"), "{gone:?}");
        let busy = quote_of(1, Some(ItemResult::Failed { id: ItemId { id: "O".into(), change_key: None }, code: "ErrorServerBusy".into(), message: String::new() })).unwrap_err();
        assert!(busy.retry && busy.message.contains("ErrorServerBusy"));
        assert!(quote_of(1, None).unwrap_err().retry);
    }

    #[test]
    fn local_draft_before_send_keeps_the_server_draft() {
        let req = request(r#"{"kind":"reply","reference_item_pk":5,"to":[{"name":"Анна","email":"a@y.ru"}],"subject":" RE: Смета ","body_html":"<p>ок</p>","attachments":["C:/a.pdf"],"draft_server_id":null,"removed_attachment_ids":["att1"],"local_draft_id":" L1 "}"#);
        assert_eq!(local_id_of(&req), Some("L1"));
        let picked = vec![PickedFile { path: "C:/a.pdf".into(), name: "a.pdf".into(), size: 3 }];
        let row = LocalDraftDto::sending("L1", &req, "SRV", picked).to_row().unwrap();
        assert_eq!((row.draft_id.as_str(), row.kind.as_str(), row.reference_item_pk, row.draft_server_id.as_deref(), row.subject.as_str()), ("L1", "reply", Some(5), Some("SRV"), "RE: Смета"));
        assert!(row.to_json.contains("a@y.ru") && row.body_html == "<p>ок</p>" && row.attachments_json.contains("a.pdf") && row.removed_json.contains("att1"));
        assert_eq!(local_id_of(&request(r#"{"kind":"new","reference_item_pk":null,"local_draft_id":"  "}"#)), None);
    }

    #[test]
    fn local_draft_dto_round_trips_through_row() {
        let dto: LocalDraftDto = serde_json::from_str(r#"{"draft_id":" d1 ","kind":"reply_all","reference_item_pk":5,"draft_server_id":"  ","subject":"s","to":[{"name":"Имя","email":"a@x.ru"}],"cc":[],"bcc":[],"body_html":"<p>t</p>","attachments":[{"path":"C:/a.pdf","name":"a.pdf","size":3}],"removed_attachment_ids":["att1"]}"#).unwrap();
        let row = dto.to_row().unwrap();
        assert_eq!((row.draft_id.as_str(), row.kind.as_str(), row.reference_item_pk, row.draft_server_id.as_deref()), ("d1", "reply_all", Some(5), None));
        assert!(row.to_json.contains("a@x.ru") && row.attachments_json.contains("a.pdf") && row.removed_json.contains("att1"));
        let back = LocalDraftDto::from_row(row);
        assert_eq!(back.kind, ComposeKindDto::ReplyAll);
        assert_eq!((back.to.len(), back.attachments.len(), back.removed_attachment_ids.len(), back.subject.as_str()), (1, 1, 1, "s"));
        assert_eq!(back.attachments[0].size, 3);
    }

    #[test]
    fn with_reference_keeps_kind_and_swaps_original() {
        let old = ItemId { id: "A".into(), change_key: Some("CQAAAA==".into()) };
        let fresh = ItemId { id: "A".into(), change_key: Some("CQAAABYAAAD7bL0OVIq7Rr0Dd0Rdq5a5AAAbuyX5".into()) };
        assert_eq!(with_reference(ComposeKind::Reply { reference: old.clone() }, fresh.clone()), ComposeKind::Reply { reference: fresh.clone() });
        assert_eq!(with_reference(ComposeKind::ReplyAll { reference: old.clone() }, fresh.clone()), ComposeKind::ReplyAll { reference: fresh.clone() });
        assert_eq!(with_reference(ComposeKind::Forward { reference: old }, fresh.clone()), ComposeKind::Forward { reference: fresh.clone() });
        assert_eq!(with_reference(ComposeKind::New, fresh), ComposeKind::New);
    }

    #[test]
    fn send_item_outcome_unknown_only_for_network_and_protocol() {
        use aiowa_transport::TransportError;
        assert!(outcome_unknown(&TransportError::Network("обрыв".into())));
        assert!(!outcome_unknown(&TransportError::Connect("нет соединения".into())), "запрос не ушёл — письмо точно не отправлено");
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
        assert!(!rep.maybe_sent && rep.draft_saved && rep.draft.is_none());
        // Черновик остался на сервере: окно получает его id и продолжает с ним, а не создаёт второй.
        let draft = DraftRef { server_id: "D".into(), change_key: Some("k".into()), item_pk: None };
        let files = vec![DraftAttachmentDto { attachment_pk: None, server_id: "a1".into(), name: Some("отчёт.pdf".into()), size: Some(10), content_type: None }];
        let rep = kept("Сервер отказал: X".into()).with_draft(draft.clone(), Some(files));
        let v = serde_json::to_value(&rep).unwrap();
        assert_eq!(v["draft"]["server_id"], "D");
        assert_eq!(v["draft"]["change_key"], "k");
        assert_eq!(v["attachments"][0]["server_id"], "a1");
        assert_eq!(serde_json::to_value(SendReport { sent: true, ..SendReport::default() }).unwrap()["draft"], serde_json::Value::Null);
    }

    #[test]
    fn compose_kind_dto_uses_snake_case() {
        assert_eq!(serde_json::from_str::<ComposeKindDto>("\"reply_all\"").unwrap(), ComposeKindDto::ReplyAll);
        assert_eq!(serde_json::to_string(&ComposeKindDto::Forward).unwrap(), "\"forward\"");
        assert!(ComposeKindDto::Reply.needs_reference() && !ComposeKindDto::New.needs_reference());
    }

    fn request(json: &str) -> ComposeRequest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn draft_fields_of_request_are_optional_and_cleaned() {
        // Окно старой сборки или новое письмо без черновика: полей нет.
        let req = request(r#"{"kind":"new","reference_item_pk":null,"to":[],"cc":[],"bcc":[],"subject":"s","body_html":"","attachments":[]}"#);
        assert_eq!(draft_id_of(&req), None);
        assert!(removed_ids(&req).is_empty());
        // Окно шлёт null и пустой список.
        let req = request(r#"{"kind":"new","reference_item_pk":null,"draft_server_id":null,"removed_attachment_ids":[]}"#);
        assert_eq!((draft_id_of(&req), removed_ids(&req)), (None, vec![]));
        assert_eq!(draft_id_of(&request(r#"{"kind":"new","reference_item_pk":null,"draft_server_id":"  "}"#)), None, "пустой id — черновика нет");
        let req = request(r#"{"kind":"reply","reference_item_pk":5,"draft_server_id":"  AAA= ","removed_attachment_ids":["a1"," ","a1","a2"]}"#);
        assert_eq!(draft_id_of(&req).as_deref(), Some("AAA="));
        assert_eq!(removed_ids(&req), vec!["a1".to_string(), "a2".to_string()], "пустые и повторы убраны, порядок сохранён");
    }

    #[test]
    fn change_keys_are_remembered_per_draft() {
        let cs = ComposeState::default();
        assert_eq!(cs.item_id("D1"), ItemId { id: "D1".into(), change_key: None });
        cs.remember(&ItemId { id: "D1".into(), change_key: Some("k1".into()) });
        cs.remember(&ItemId { id: "D1".into(), change_key: Some("k2".into()) });
        assert_eq!(cs.item_id("D1").change_key.as_deref(), Some("k2"));
        assert_eq!(cs.item_id("D2").change_key, None);
        cs.forget("D1");
        assert_eq!(cs.item_id("D1").change_key, None);
    }

    #[test]
    fn files_already_on_the_draft_are_not_uploaded_again() {
        let file = |name: &str, len: usize| OutgoingAttachment { name: name.into(), content_type: None, bytes: vec![0; len] };
        let meta = |id: &str, name: &str, size: i64, inline: bool| AttachmentMeta { id: id.into(), name: Some(name.into()), content_type: None, size: Some(size), is_inline: inline, content_id: None };
        let on_server = [meta("a1", "Отчёт.PDF", 10, false), meta("a2", "картинка.png", 5, true)];
        let names = |v: Vec<OutgoingAttachment>| v.into_iter().map(|f| f.name).collect::<Vec<_>>();
        // Тот же файл (имя без учёта регистра, размер) пропускается; другой размер, другое имя и инлайновая картинка — нет.
        assert_eq!(names(filter_new_files(vec![file("отчёт.pdf", 10), file("отчёт.pdf", 11), file("другой.pdf", 10), file("картинка.png", 5)], &on_server)), vec!["отчёт.pdf", "другой.pdf", "картинка.png"]);
        assert_eq!(filter_new_files(vec![file("отчёт.pdf", 10)], &on_server).len(), 0);
        assert_eq!(filter_new_files(vec![file("x.txt", 1)], &[]).len(), 1);
    }

    #[test]
    fn reconcile_skips_uploaded_files_and_already_removed_attachments() {
        let file = |name: &str, len: usize| OutgoingAttachment { name: name.into(), content_type: None, bytes: vec![0; len] };
        let meta = |id: &str, name: &str, size: i64| AttachmentMeta { id: id.into(), name: Some(name.into()), content_type: None, size: Some(size), is_inline: false, content_id: None };
        let names = |v: Vec<OutgoingAttachment>| v.into_iter().map(|f| f.name).collect::<Vec<_>>();
        let on_server = [meta("a1", "отчёт.pdf", 10), meta("a2", "старый.pdf", 7)];
        // Уже загруженный файл не грузится второй раз; уже удалённое вложение не удаляется снова.
        let (files, removed) = reconcile_with(&on_server, vec!["a2".into(), "gone".into()], vec![file("отчёт.pdf", 10), file("новый.pdf", 3)]);
        assert_eq!(names(files), vec!["новый.pdf"]);
        assert_eq!(removed, vec!["a2".to_string()]);
        // Файл с тем же именем и размером, что у убираемого вложения, грузится: то вложение уйдёт.
        let (files, removed) = reconcile_with(&on_server, vec!["a2".into()], vec![file("старый.pdf", 7)]);
        assert_eq!((files.len(), removed), (1, vec!["a2".to_string()]));
        // Повтор после удачного DeleteAttachment (или вложение убрано в OWA): удалять нечего.
        let (files, removed) = reconcile_with(&on_server[..1], vec!["a2".into()], vec![]);
        assert!(files.is_empty() && removed.is_empty());
    }

    #[test]
    fn quote_escapes_header_and_keeps_cleaned_body() {
        let q = quote_html("Иван <ivan@x.ru>", Some("24.10.2026 10:05"), "Анна <a@y.ru>; b@y.ru", "", "Смета <срочно> & \"точно\"", "<p>Текст <b>оригинала</b></p>");
        assert!(q.starts_with("<hr><div><b>От:</b> Иван &lt;ivan@x.ru&gt;<br><b>Отправлено:</b> 24.10.2026 10:05<br><b>Кому:</b> Анна &lt;a@y.ru&gt;; b@y.ru<br><b>Тема:</b> Смета &lt;срочно&gt; &amp; &quot;точно&quot;</div>"), "{q}");
        assert!(!q.contains("Копия"), "пустая «Копия» в шапку не попадает: {q}");
        assert!(q.ends_with("<div><p>Текст <b>оригинала</b></p></div>"), "{q}");
        let bare = quote_html("", None, "", "", "", "");
        assert_eq!(bare, "<hr><div><b>От:</b> <br><b>Тема:</b> </div><br><div></div>");
        assert_eq!(person_label(Some(" Иван "), Some("ivan@x.ru")), "Иван <ivan@x.ru>");
        assert_eq!(person_label(Some("ivan@x.ru"), Some("ivan@x.ru")), "ivan@x.ru");
        assert_eq!(person_label(None, Some(" ")), "");
        assert_eq!(person_label(Some("Иван"), None), "Иван");
    }

    mod store_backed {
        use super::*;
        use aiowa_core::{ConversationId, Folder, FolderId, MessageBody, MessageFull, MessageMeta};

        fn full(id: &str, is_draft: bool, unique: &str, full_body: Option<&str>, attachments: Vec<AttachmentMeta>) -> MessageFull {
            let p = |name: &str, email: &str| Participant { name: Some(name.into()), email: Some(email.into()), routing_type: Some("SMTP".into()) };
            MessageFull {
                meta: MessageMeta {
                    item_id: ItemId { id: id.into(), change_key: Some("ck1".into()) },
                    parent_folder_id: None,
                    conversation_id: Some(ConversationId("conv".into())),
                    internet_message_id: None,
                    item_class: Some("IPM.Note".into()),
                    subject: Some("Смета на ремонт".into()),
                    from: Some(p("Иван Петров", "ivan@x.ru")),
                    sender: None,
                    display_to: None,
                    received_at: Some("2026-10-16T10:00:00Z".parse().unwrap()),
                    sent_at: Some("2026-10-16T09:59:00Z".parse().unwrap()),
                    last_modified_at: None,
                    is_read: true,
                    is_draft,
                    has_attachments: !attachments.is_empty(),
                    importance: Importance::Normal,
                    flag: Default::default(),
                    size: Some(100),
                    preview: None,
                    last_verb: None,
                    last_verb_at: None,
                },
                to: vec![p("Анна", "a@y.ru"), Participant { name: Some("Старый адрес".into()), email: Some("/o=Org/cn=old".into()), routing_type: Some("EX".into()) }],
                cc: vec![p("Вера", "v@z.ru")],
                bcc: vec![p("Скрытый", "hidden@y.ru")],
                reply_to: vec![],
                body: Some(MessageBody { body_type: BodyType::Html, unique_body: Some(unique.into()), full_body: full_body.map(String::from), is_truncated: false }),
                attachments,
            }
        }

        fn setup(tag: &str, messages: &[MessageFull]) -> (Store, std::path::PathBuf, Vec<i64>) {
            let dir = std::env::temp_dir().join(format!("aiowa-compose-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let store = Store::open(dir.join("mail.db")).unwrap();
            let acc = store.account("h", "u").unwrap();
            let folder = Folder {
                id: FolderId { id: "F".into(), change_key: None },
                parent_id: None,
                display_name: "Черновики".into(),
                distinguished: Some("drafts".into()),
                folder_class: Some("IPF.Note".into()),
                total_count: 1,
                unread_count: 0,
                child_folder_count: 0,
            };
            store.upsert_folders(acc, &[folder]).unwrap();
            let (fpk, _) = store.folder_by_distinguished(acc, "drafts").unwrap().unwrap();
            store.store_full_messages(acc, fpk, messages).unwrap();
            let pks = messages.iter().map(|m| store.item_pk_by_server_id(acc, &m.meta.item_id.id).unwrap().unwrap().0).collect();
            (store, dir, pks)
        }

        fn att(id: &str, name: &str, inline: bool) -> AttachmentMeta {
            AttachmentMeta { id: id.into(), name: Some(name.into()), content_type: Some("application/pdf".into()), size: Some(100), is_inline: inline, content_id: inline.then(|| format!("{id}@cid")) }
        }

        #[test]
        fn local_draft_takes_full_body_recipients_and_file_attachments() {
            let messages = [
                full("D1", true, "<p>новый текст</p>", Some(r#"<p>новый текст</p><hr><p>цитата истории</p><img src="cid:img@cid">"#), vec![att("f1", "смета.pdf", false), att("img", "картинка.png", true)]),
                full("D2", true, "<p>только уникальная часть</p>", None, vec![]),
                full("M1", false, "<p>обычное письмо</p>", None, vec![]),
            ];
            let (store, dir, pks) = setup("local", &messages);

            let d = local_draft(&store, pks[0]).unwrap();
            assert_eq!((d.id.id.as_str(), d.id.change_key.as_deref(), d.subject.as_str()), ("D1", Some("ck1"), "Смета на ремонт"));
            let body = d.body_html.as_deref().unwrap();
            assert!(body.contains("цитата истории"), "в редактор идёт полное тело с цитатой: {body}");
            assert!(body.contains(r#"src="cid:img@cid""#), "cid: остаётся в теле, а не превращается в data: или пропадает: {body}");
            assert!(d.inline_images.is_empty(), "картинки нет в кэше — показывать нечем, но ссылка цела");
            assert_eq!(d.to.iter().map(|r| r.email.as_str()).collect::<Vec<_>>(), vec!["a@y.ru"], "адрес Exchange без @ не берётся");
            assert_eq!((d.cc[0].email.as_str(), d.bcc[0].email.as_str(), d.bcc[0].name.as_deref()), ("v@z.ru", "hidden@y.ru", Some("Скрытый")));
            assert_eq!(d.files.iter().map(|a| a.server_id.as_str()).collect::<Vec<_>>(), vec!["f1"], "инлайновые картинки не показываются как файлы");

            // Полного тела локально нет — нужен сервер (UniqueBody у ответа без цитаты не годится для правки).
            assert!(local_draft(&store, pks[1]).unwrap().body_html.is_none());
            assert_eq!(local_draft(&store, pks[2]).err().as_deref(), Some("Это письмо не черновик"));
            assert_eq!(local_draft(&store, 999_999).err().as_deref(), Some("Черновик удалён"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn contact_with_is_draft_does_not_open_in_composer() {
            // Сервер отдаёт контакт с IsDraft (он никогда не отправлялся): в композер его не пускать.
            let mut contact = full("C1", true, "<p>заметки</p>", Some("<p>заметки</p>"), vec![]);
            contact.meta.item_class = Some("IPM.Contact".into());
            let (store, dir, pks) = setup("contact", &[contact]);
            assert_eq!(local_draft(&store, pks[0]).err().as_deref(), Some("Это не письмо: в композере открываются только черновики писем"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn quote_reference_refuses_when_the_original_is_deleted() {
            let (store, dir, pks) = setup("quote-ref", &[full("M1", false, "<p>оригинал</p>", None, vec![])]);
            assert_eq!(quote_reference(&store, pks[0]).unwrap().id, "M1");
            // Оригинал удалён в базе: черновик ответа не обновляется одним новым текстом без цитаты.
            store.mark_deleted_local(&[pks[0]]).unwrap();
            assert_eq!(quote_reference(&store, pks[0]).unwrap_err(), ComposeFail::original_gone());
            assert_eq!(quote_reference(&store, 999_999).unwrap_err(), ComposeFail::original_gone());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn quote_comes_from_the_full_body_and_keeps_cid() {
            let m = full("M1", false, "<p>новый текст</p>", Some(r#"<p>новый текст</p><p>старая переписка</p><img src="cid:logo@x"><img src="https://x.ru/a.png">"#), vec![]);
            let q = quote_from_full(&m).unwrap();
            assert!(q.contains("<b>От:</b> Иван Петров &lt;ivan@x.ru&gt;"), "{q}");
            assert!(q.contains("<b>Кому:</b> Анна &lt;a@y.ru&gt;; Старый адрес &lt;/o=Org/cn=old&gt;"), "{q}");
            assert!(q.contains("<b>Копия:</b> Вера &lt;v@z.ru&gt;") && !q.contains("hidden@y.ru"), "скрытые получатели в цитату не попадают: {q}");
            assert!(q.contains("<b>Тема:</b> Смета на ремонт"), "{q}");
            assert!(q.contains("старая переписка"), "цитируется полное тело с историей: {q}");
            assert!(q.contains("<b>Отправлено:</b> 16.10.2026"), "{q}");
            assert!(q.contains(r#"src="cid:logo@x""#) && q.contains(r#"src="https://x.ru/a.png""#) && !q.contains("data:"), "картинки остаются ссылками: {q}");
            // Только UniqueBody (полного тела нет), обрезанное тело или тела нет вовсе — отказ, а не урезанная цитата.
            assert!(quote_from_full(&full("M2", false, "<p>x</p>", None, vec![])).unwrap_err().contains("полный текст"));
            let mut cut = full("M3", false, "<p>x</p>", Some("<p>x</p><p>история</p>"), vec![]);
            cut.body.as_mut().unwrap().is_truncated = true;
            assert!(quote_from_full(&cut).unwrap_err().contains("обрезана"));
            let mut bodyless = full("M4", false, "<p>x</p>", Some("<p>x</p>"), vec![]);
            bodyless.body = None;
            assert!(quote_from_full(&bodyless).is_err());
        }
    }
}
