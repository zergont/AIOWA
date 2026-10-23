//! Транспорт поверх внутреннего JSON API Outlook Web App (Exchange 2016/2019).
//!
//! Факты, на которых всё держится (см. `docs/protocol-owa-json.md`):
//! вход через форму `/owa/auth.owa`; cookie `X-OWA-CANARY` дублируется заголовком;
//! `__type` первым ключом каждого объекта; системный TLS; одна сессия на процесс и
//! явный `logoff.owa`; истечение сессии = 440/401/редирект на `logon.aspx`.

pub mod notify;
pub mod protocol;

use aiowa_core::{CalendarEvent, CalendarEventDetails, ComposeKind, FolderChanges, FolderRef, ItemChanges, ItemId, OutgoingAttachment, OutgoingMessage, Signature, SyncState};
use aiowa_transport::{
    async_trait, AttachmentContent, Credentials, DeleteMode, FolderCounts, ItemFetchShape, ItemResult, MailTransport, NotificationEvent, NotificationKind, Result, ServerInfo, StreamEnd, SubscriptionRequest,
    SubscriptionResult, TransportError, WriteResult,
};
use base64::prelude::*;
use jiff::Timestamp;
use notify::SubscribeForm;
use protocol::CalendarItemShape;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode, Url};
use secrecy::ExposeSecret;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, info, warn};

/// Настройки транспорта.
#[derive(Debug, Clone)]
pub struct OwaConfig {
    /// Часовой пояс Windows в заголовке запросов.
    pub time_zone: String,
    /// Одновременных запросов к серверу.
    pub concurrency: usize,
    /// Таймаут одного HTTP-запроса.
    pub timeout: Duration,
    /// Размер пачки `GetItem`.
    pub get_item_batch: usize,
    /// Максимальный размер тела письма в символах.
    pub max_body_size: u64,
    /// Строка User-Agent. Честная, без маскировки под браузер.
    pub user_agent: String,
    /// Таймаут одного запроса `CreateAttachment`: файл до 25 МиБ в base64 на медленном канале
    /// не укладывается в общий `timeout`.
    pub upload_timeout: Duration,
}

impl Default for OwaConfig {
    fn default() -> Self {
        Self {
            // Все даты в UTC: пояс пользователя нужен только интерфейсу и календарю.
            time_zone: "UTC".to_string(),
            concurrency: 2,
            timeout: Duration::from_secs(90),
            get_item_batch: 25,
            max_body_size: 2_097_152,
            user_agent: format!("AIOWA/{}", env!("CARGO_PKG_VERSION")),
            upload_timeout: Duration::from_secs(600),
        }
    }
}

struct Inner {
    client: Client,
    jar: Arc<Jar>,
    base: Url,
    creds: Credentials,
    config: OwaConfig,
    canary: RwLock<Option<String>>,
    build: RwLock<Option<String>>,
    action_id: AtomicI64,
    /// Растёт при каждом успешном входе; нужен для single-flight повторного входа.
    generation: AtomicU64,
    relogin: Mutex<()>,
    /// Выставляется в `logoff`: клоны транспорта, оставшиеся у синхронизации или команды окна,
    /// больше не входят заново, иначе разлогиненный транспорт воскрешал бы сессию, которую
    /// никто не отслеживает и не завершает (а лимит сессий на ящик мал).
    closed: AtomicBool,
    permits: Semaphore,
    /// Какой путь скачивания вложений принял сервер: 0 не известно, 1 GET `service.svc/s/GetFileAttachment`,
    /// 2 операция `GetAttachment` с base64 в JSON.
    attachment_mode: AtomicU8,
    /// Какую форму запроса календаря принял сервер (`CALENDAR_MODE_*`).
    calendar_mode: AtomicU8,
    /// Какую форму `ItemShape` для `GetItem` события принял сервер (`CALENDAR_SHAPE_*`).
    calendar_shape: AtomicU8,
    /// Какую форму `SubscribeToNotification` принял сервер (`SubscribeForm::code`, 0 не известно).
    subscribe_form: AtomicU8,
    /// Подписки текущей сессии: вид по идентификатору, чтобы разбирать события потока.
    subscriptions: RwLock<HashMap<String, NotificationKind>>,
    /// Подписки потеряны при повторном входе, а вызывающий об этом ещё не знает: следующий
    /// открытый поток первым делом отдаёт `Reload`.
    subscriptions_stale: AtomicBool,
    /// Счётчик `n` запросов `PendingNotificationRequest`.
    pending_counter: AtomicU64,
}

/// Тишина в потоке уведомлений дольше этого — обрыв (`Network`). Сервер шлёт keep-alive каждые
/// 40 с (между ними пустые блоки), штатный цикл он закрывает через 320 с: в живом потоке такой
/// тишины не бывает, а мёртвое соединение после сна или смены сети (FIN не приходит) иначе
/// висело бы до таймаута, пока окно держит редкий страховочный таймер.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(150);
/// Общий предел одного цикла потока (сервер закрывает его примерно через 5–6 минут).
const STREAM_TOTAL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

const ATTACHMENT_MODE_UNKNOWN: u8 = 0;
const ATTACHMENT_MODE_GET: u8 = 1;
const ATTACHMENT_MODE_JSON: u8 = 2;

/// Формы запроса окна календаря в порядке проб: `FindItem` с `CalendarPageView` и датами `…Z`,
/// то же без `Z`, операция OWA `GetCalendarView`.
const CALENDAR_MODE_UNKNOWN: u8 = 0;
const CALENDAR_MODE_FIND_ITEM_Z: u8 = 1;
const CALENDAR_MODE_FIND_ITEM_NAIVE: u8 = 2;
const CALENDAR_MODE_GET_CALENDAR_VIEW: u8 = 3;
const CALENDAR_MODES: [u8; 3] = [CALENDAR_MODE_FIND_ITEM_Z, CALENDAR_MODE_FIND_ITEM_NAIVE, CALENDAR_MODE_GET_CALENDAR_VIEW];

const CALENDAR_SHAPE_UNKNOWN: u8 = 0;
const CALENDAR_SHAPE_EXPLICIT: u8 = 1;
const CALENDAR_SHAPE_ALL: u8 = 2;

/// Самое узкое окно, на которое делится выдача календаря при `IncludesLastItemInRange: false`.
const CALENDAR_MIN_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_hours(1);
/// Предел запросов на одно окно календаря (защита от бесконечного деления).
const CALENDAR_MAX_REQUESTS: usize = 64;

/// Транспорт OWA JSON. Дёшево клонируется (Arc внутри).
#[derive(Clone)]
pub struct OwaTransport {
    inner: Arc<Inner>,
}

fn net(e: reqwest::Error) -> TransportError {
    TransportError::Network(Box::new(e))
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// Классификация редиректа после запроса к сессии.
fn classify_redirect(location: &str) -> TransportError {
    let lower = location.to_ascii_lowercase();
    if lower.contains("logon.aspx") {
        TransportError::SessionExpired
    } else if lower.contains("errorfe.aspx") {
        if lower.contains("sessionlimit") || lower.contains("toomanyobjectsopened") {
            TransportError::SessionLimit
        } else {
            TransportError::CsrfRejected
        }
    } else {
        TransportError::Protocol { op: "redirect".into(), status: 302, raw: truncate(location, 200) }
    }
}

impl OwaTransport {
    /// Входит на сервер и возвращает готовый транспорт.
    pub async fn connect(creds: Credentials, config: OwaConfig) -> Result<Self> {
        let t = Self::prepare(creds, config)?;
        t.login().await?;
        Ok(t)
    }

    /// Транспорт без входа: HTTP-клиент, cookie-банка и состояние сессии.
    fn prepare(creds: Credentials, config: OwaConfig) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        // Только HTTP/1.1: сервер работает на TLS 1.2 с CBC-шифром, а HTTP/2 поверх таких шифров запрещён.
        let client = Client::builder()
            .cookie_provider(jar.clone())
            .redirect(Policy::none())
            .user_agent(&config.user_agent)
            .http1_only()
            .connect_timeout(Duration::from_secs(10))
            .timeout(config.timeout)
            .build()
            .map_err(net)?;
        let base = Url::parse(&format!("https://{}/", creds.host)).map_err(|e| TransportError::Protocol { op: "config".into(), status: 0, raw: e.to_string() })?;
        let inner = Arc::new(Inner {
            client,
            jar,
            base,
            permits: Semaphore::new(config.concurrency.max(1)),
            creds,
            config,
            canary: RwLock::new(None),
            build: RwLock::new(None),
            action_id: AtomicI64::new(-1),
            generation: AtomicU64::new(0),
            relogin: Mutex::new(()),
            closed: AtomicBool::new(false),
            attachment_mode: AtomicU8::new(ATTACHMENT_MODE_UNKNOWN),
            calendar_mode: AtomicU8::new(CALENDAR_MODE_UNKNOWN),
            calendar_shape: AtomicU8::new(CALENDAR_SHAPE_UNKNOWN),
            subscribe_form: AtomicU8::new(0),
            subscriptions: RwLock::new(HashMap::new()),
            subscriptions_stale: AtomicBool::new(false),
            pending_counter: AtomicU64::new(0),
        });
        Ok(Self { inner })
    }

    fn owa_url(&self) -> Url {
        self.inner.base.join("owa/").expect("base url")
    }

    fn cookie_value(&self, name: &str) -> Option<String> {
        let header = self.inner.jar.cookies(&self.owa_url())?;
        let s = header.to_str().ok()?;
        s.split(';').map(str::trim).find_map(|c| {
            let (k, v) = c.split_once('=')?;
            (k == name).then(|| v.to_string())
        })
    }

    fn remember_build(&self, headers: &HeaderMap) {
        if let Some(v) = headers.get("x-owa-version").and_then(|v| v.to_str().ok()).map(|v| v.split(',').next().unwrap_or(v).trim()) {
            let mut b = self.inner.build.write().expect("build lock");
            if b.as_deref() != Some(v) {
                *b = Some(v.to_string());
            }
        }
    }

    /// Вход через форму OWA. Пароль уходит только в тело POST.
    async fn login(&self) -> Result<()> {
        let inner = &self.inner;
        let owa = inner.base.join("owa").expect("url");
        let logon = inner.base.join("owa/auth/logon.aspx").expect("url");
        let _ = inner.client.get(logon).query(&[("url", owa.as_str()), ("reason", "0")]).send().await.map_err(net)?;

        let form = [
            ("destination", owa.as_str()),
            ("flags", "4"),
            ("forcedownlevel", "0"),
            ("username", inner.creds.user.as_str()),
            ("password", inner.creds.password.expose_secret()),
            ("passwordText", ""),
            ("isUtf8", "1"),
        ];
        let r = inner.client.post(inner.base.join("owa/auth.owa").expect("url")).form(&form).send().await.map_err(net)?;
        let status = r.status();
        let location = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).map(String::from);
        if let Some(loc) = &location {
            let lower = loc.to_ascii_lowercase();
            if lower.contains("logon.aspx") {
                let reason = loc.split("reason=").nth(1).unwrap_or("?").chars().take_while(|c| c.is_ascii_digit()).collect::<String>();
                return Err(TransportError::AuthFailed(format!("сервер вернул на страницу входа, reason={reason}")));
            }
            if lower.contains("errorfe.aspx") {
                return Err(classify_redirect(loc));
            }
        }
        if !status.is_redirection() {
            let text = r.text().await.unwrap_or_default();
            if text.contains("logon.aspx") || text.contains("name=\"password\"") {
                return Err(TransportError::AuthFailed("сервер вернул страницу входа".into()));
            }
            return Err(TransportError::Protocol { op: "auth.owa".into(), status: status.as_u16(), raw: truncate(&text, 200) });
        }

        let mut next = location;
        for _ in 0..5 {
            let Some(loc) = next.take() else { break };
            let url = inner.base.join(&loc).map_err(|e| TransportError::Protocol { op: "redirect".into(), status: 0, raw: e.to_string() })?;
            let r = inner.client.get(url).send().await.map_err(net)?;
            self.remember_build(r.headers());
            if r.status().is_redirection() {
                let l = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).map(String::from);
                if let Some(l) = &l {
                    if l.to_ascii_lowercase().contains("errorfe.aspx") {
                        return Err(classify_redirect(l));
                    }
                }
                next = l;
            } else {
                let _ = r.bytes().await;
            }
        }

        let canary = self.cookie_value("X-OWA-CANARY").ok_or_else(|| TransportError::AuthFailed("после входа нет cookie X-OWA-CANARY".into()))?;
        *inner.canary.write().expect("canary lock") = Some(canary);
        inner.generation.fetch_add(1, Ordering::SeqCst);
        // Подписки на уведомления живут в сессии: в новой их нет, и поток должен сказать об этом.
        let lost = {
            let mut subs = inner.subscriptions.write().expect("subscriptions lock");
            let n = subs.len();
            subs.clear();
            n
        };
        if lost > 0 {
            inner.subscriptions_stale.store(true, Ordering::SeqCst);
            warn!(subscriptions = lost, "подписки на уведомления потеряны вместе с сессией");
        }
        info!(host = %inner.creds.host, "вход в OWA выполнен");
        Ok(())
    }

    /// Повторный вход один раз на поколение сессии: параллельные вызовы не плодят входов.
    /// После `logoff` входа нет: вызывающий получает `SessionExpired` и берёт новый транспорт.
    async fn relogin_once(&self, seen_generation: u64) -> Result<()> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(TransportError::SessionExpired);
        }
        let _guard = self.inner.relogin.lock().await;
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(TransportError::SessionExpired);
        }
        if self.inner.generation.load(Ordering::SeqCst) != seen_generation {
            return Ok(());
        }
        warn!("сессия OWA истекла, повторный вход");
        self.login().await
    }

    /// Вызов операции `service.svc` с одной попыткой повторного входа.
    pub async fn call(&self, op: &str, version: &str, body: Value) -> Result<Value> {
        self.call_with_timeout(op, version, body, None).await
    }

    /// То же с собственным таймаутом запроса вместо общего из настроек (загрузка вложений).
    pub async fn call_with_timeout(&self, op: &str, version: &str, body: Value, timeout: Option<Duration>) -> Result<Value> {
        let env = protocol::envelope(op, version, &self.inner.config.time_zone, body);
        let payload = serde_json::to_vec(&env).map_err(|e| TransportError::Protocol { op: op.into(), status: 0, raw: e.to_string() })?;
        self.call_payload(op, payload, timeout).await
    }

    /// Операция с OWA-специфичным конвертом (параметры на верхнем уровне рядом с `request`,
    /// без `Body`): `payload` уходит как есть, ответ — произвольный JSON (у подписок массив).
    pub async fn call_raw(&self, op: &str, payload: Value) -> Result<Value> {
        let payload = serde_json::to_vec(&payload).map_err(|e| TransportError::Protocol { op: op.into(), status: 0, raw: e.to_string() })?;
        self.call_payload(op, payload, None).await
    }

    /// Готовое тело запроса под разрешением семафора с одной попыткой повторного входа.
    async fn call_payload(&self, op: &str, payload: Vec<u8>, timeout: Option<Duration>) -> Result<Value> {
        let _permit = self.inner.permits.acquire().await.map_err(|_| TransportError::Cancelled)?;
        for attempt in 0..2 {
            let generation = self.inner.generation.load(Ordering::SeqCst);
            match self.call_once(op, payload.clone(), timeout).await {
                Err(e) if e.needs_reauth() && attempt == 0 => {
                    self.relogin_once(generation).await?;
                }
                other => return other,
            }
        }
        Err(TransportError::SessionExpired)
    }

    async fn call_once(&self, op: &str, payload: Vec<u8>, timeout: Option<Duration>) -> Result<Value> {
        let inner = &self.inner;
        let id = inner.action_id.fetch_sub(1, Ordering::SeqCst);
        let mut url = inner.base.join("owa/service.svc").expect("url");
        url.query_pairs_mut().append_pair("action", op).append_pair("ID", &id.to_string()).append_pair("AC", "1");
        let canary = inner.canary.read().expect("canary lock").clone().ok_or(TransportError::SessionExpired)?;
        let mut headers = HeaderMap::new();
        headers.insert("Action", HeaderValue::from_str(op).map_err(|_| TransportError::Protocol { op: op.into(), status: 0, raw: "bad op".into() })?);
        headers.insert("X-OWA-CANARY", HeaderValue::from_str(&canary).map_err(|_| TransportError::CsrfRejected)?);
        headers.insert("Content-Type", HeaderValue::from_static("application/json; charset=utf-8"));
        headers.insert("X-Requested-With", HeaderValue::from_static("XMLHttpRequest"));
        if let Some(b) = inner.build.read().expect("build lock").as_deref() {
            if let Ok(v) = HeaderValue::from_str(b) {
                headers.insert("X-OWA-ClientBuildVersion", v);
            }
        }
        let started = std::time::Instant::now();
        let mut req = inner.client.post(url).headers(headers).body(payload);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let r = req.send().await.map_err(net)?;
        let status = r.status();
        self.remember_build(r.headers());
        if status == StatusCode::from_u16(440).unwrap_or(StatusCode::UNAUTHORIZED) || status == StatusCode::UNAUTHORIZED {
            return Err(TransportError::SessionExpired);
        }
        if status.is_redirection() {
            let loc = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            return Err(classify_redirect(&loc));
        }
        let text = r.text().await.map_err(net)?;
        debug!(op, %status, ms = started.elapsed().as_millis(), bytes = text.len(), "service.svc");
        if !status.is_success() {
            // Ошибка уровня сервиса приходит как {"Body":{"ErrorCode":500,"FaultMessage":...}}
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(fault) = v.pointer("/Body/FaultMessage").and_then(Value::as_str) {
                    return Err(TransportError::Protocol { op: op.into(), status: status.as_u16(), raw: truncate(fault, 300) });
                }
            }
            return Err(TransportError::Protocol { op: op.into(), status: status.as_u16(), raw: truncate(&text, 300) });
        }
        serde_json::from_str::<Value>(&text).map_err(|e| TransportError::Protocol { op: op.into(), status: status.as_u16(), raw: format!("не JSON ({e}): {}", truncate(&text, 200)) })
    }

    /// Скачивание файлового вложения с одной попыткой повторного входа.
    async fn get_attachment_with_relogin(&self, id: &str, max_bytes: u64) -> Result<AttachmentContent> {
        let _permit = self.inner.permits.acquire().await.map_err(|_| TransportError::Cancelled)?;
        for attempt in 0..2 {
            let generation = self.inner.generation.load(Ordering::SeqCst);
            match self.get_attachment_once(id, max_bytes).await {
                Err(e) if e.needs_reauth() && attempt == 0 => {
                    self.relogin_once(generation).await?;
                }
                other => return other,
            }
        }
        Err(TransportError::SessionExpired)
    }

    /// Сначала GET `service.svc/s/GetFileAttachment` (так качает браузерная OWA), при отказе
    /// сервера по форме запроса — операция `GetAttachment` с содержимым в base64. Удачный
    /// путь запоминается на время сессии.
    async fn get_attachment_once(&self, id: &str, max_bytes: u64) -> Result<AttachmentContent> {
        let mode = self.inner.attachment_mode.load(Ordering::Relaxed);
        if mode != ATTACHMENT_MODE_JSON {
            match self.get_file_attachment_get(id, max_bytes).await {
                Ok(c) => {
                    if mode == ATTACHMENT_MODE_UNKNOWN {
                        info!(content_type = c.content_type.as_deref().unwrap_or("?"), file_name = c.file_name.as_deref().unwrap_or("?"), bytes = c.bytes.len(), "вложения: сервер принимает GET service.svc/s/GetFileAttachment");
                    }
                    self.inner.attachment_mode.store(ATTACHMENT_MODE_GET, Ordering::Relaxed);
                    return Ok(c);
                }
                Err(e) if mode == ATTACHMENT_MODE_UNKNOWN && matches!(e, TransportError::Protocol { .. }) => {
                    warn!(error = %e, "GET GetFileAttachment отвергнут, пробую операцию GetAttachment");
                }
                Err(e) => return Err(e),
            }
        }
        let c = self.get_attachment_json(id, max_bytes).await?;
        if mode == ATTACHMENT_MODE_UNKNOWN {
            info!("вложения: сервер принимает операцию GetAttachment (base64 в JSON)");
        }
        self.inner.attachment_mode.store(ATTACHMENT_MODE_JSON, Ordering::Relaxed);
        Ok(c)
    }

    /// `GET /owa/service.svc/s/GetFileAttachment?id=<AttachmentId>&X-OWA-CANARY=<канарейка>`:
    /// бинарный ответ с `Content-Type` файла и `Content-Disposition: attachment; filename=...`.
    async fn get_file_attachment_get(&self, id: &str, max_bytes: u64) -> Result<AttachmentContent> {
        let inner = &self.inner;
        let canary = inner.canary.read().expect("canary lock").clone().ok_or(TransportError::SessionExpired)?;
        let mut url = inner.base.join("owa/service.svc/s/GetFileAttachment").expect("url");
        url.query_pairs_mut().append_pair("id", id).append_pair("X-OWA-CANARY", &canary);
        let mut headers = HeaderMap::new();
        headers.insert("X-OWA-CANARY", HeaderValue::from_str(&canary).map_err(|_| TransportError::CsrfRejected)?);
        let started = std::time::Instant::now();
        let mut r = inner.client.get(url).headers(headers).send().await.map_err(net)?;
        let status = r.status();
        self.remember_build(r.headers());
        if status == StatusCode::from_u16(440).unwrap_or(StatusCode::UNAUTHORIZED) || status == StatusCode::UNAUTHORIZED {
            return Err(TransportError::SessionExpired);
        }
        if status.is_redirection() {
            let loc = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            return Err(classify_redirect(&loc));
        }
        let content_type = r.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        if !status.is_success() {
            let text = r.text().await.unwrap_or_default();
            return Err(TransportError::Protocol { op: "GetFileAttachment".into(), status: status.as_u16(), raw: truncate(&text, 300) });
        }
        if content_type.as_deref().map(|c| c.to_ascii_lowercase().starts_with("text/html")).unwrap_or(false) {
            let text = r.text().await.unwrap_or_default();
            return Err(TransportError::Protocol { op: "GetFileAttachment".into(), status: status.as_u16(), raw: format!("сервер вернул страницу вместо файла: {}", truncate(&text, 200)) });
        }
        if let Some(len) = r.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok()) {
            if len > max_bytes {
                return Err(TransportError::TooLarge { limit: max_bytes });
            }
        }
        let file_name = r.headers().get(CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()).and_then(parse_content_disposition_filename);
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = r.chunk().await.map_err(net)? {
            if bytes.len() as u64 + chunk.len() as u64 > max_bytes {
                return Err(TransportError::TooLarge { limit: max_bytes });
            }
            bytes.extend_from_slice(&chunk);
        }
        debug!(op = "GetFileAttachment", %status, ms = started.elapsed().as_millis(), bytes = bytes.len(), "service.svc/s");
        Ok(AttachmentContent { bytes, content_type: content_type.map(|c| c.split(';').next().unwrap_or(&c).trim().to_string()), file_name })
    }

    /// Операция `GetAttachment`: `Body.ResponseMessages.Items[0].Attachments[0].Content` в base64.
    async fn get_attachment_json(&self, id: &str, max_bytes: u64) -> Result<AttachmentContent> {
        let v = self.call_unpermitted("GetAttachment", protocol::V_2013, protocol::get_attachment_body(id)).await?;
        let msg = Self::first_message("GetAttachment", &v)?;
        let att = msg.pointer("/Attachments/0").ok_or_else(|| TransportError::Protocol { op: "GetAttachment".into(), status: 200, raw: format!("нет Attachments: {}", truncate(&msg.to_string(), 200)) })?;
        let content = protocol::str_of(att, "Content").ok_or_else(|| TransportError::Protocol {
            op: "GetAttachment".into(),
            status: 200,
            raw: format!("нет Content (тип {}): не файловое вложение?", protocol::str_of(att, "__type").unwrap_or("?")),
        })?;
        // base64 длиннее исходных байт на треть: проверяем до декодирования.
        if (content.len() as u64) / 4 * 3 > max_bytes {
            return Err(TransportError::TooLarge { limit: max_bytes });
        }
        let bytes = BASE64_STANDARD.decode(content).map_err(|e| TransportError::Protocol { op: "GetAttachment".into(), status: 200, raw: format!("base64: {e}") })?;
        if bytes.len() as u64 > max_bytes {
            return Err(TransportError::TooLarge { limit: max_bytes });
        }
        Ok(AttachmentContent { bytes, content_type: protocol::str_of(att, "ContentType").map(String::from), file_name: protocol::str_of(att, "Name").map(String::from) })
    }

    /// Как `call`, но без захвата семафора (вызывающий уже держит разрешение) и без повторного входа.
    async fn call_unpermitted(&self, op: &str, version: &str, body: Value) -> Result<Value> {
        let env = protocol::envelope(op, version, &self.inner.config.time_zone, body);
        let payload = serde_json::to_vec(&env).map_err(|e| TransportError::Protocol { op: op.into(), status: 0, raw: e.to_string() })?;
        self.call_once(op, payload, None).await
    }

    /// Форма сообщения ответа в журнал отладки без содержимого: тип, код, ключи сообщения и
    /// первого элемента, длины идентификаторов. Нужна для фиксации форм в документе протокола.
    fn log_shape(op: &str, msg: &Value) {
        fn keys(v: Option<&Value>) -> Vec<&str> {
            v.and_then(Value::as_object).map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default()
        }
        let msg_keys = keys(Some(msg));
        let item_keys = keys(msg.pointer("/Items/0"));
        let att_keys = keys(msg.pointer("/Attachments/0"));
        let att_id_keys = keys(msg.pointer("/Attachments/0/AttachmentId"));
        let id_len = msg.pointer("/Items/0/ItemId/Id").and_then(Value::as_str).map(str::len);
        let ck_len = msg.pointer("/Items/0/ItemId/ChangeKey").and_then(Value::as_str).map(str::len);
        debug!(op, response_type = protocol::str_of(msg, "__type").unwrap_or("?"), response_code = protocol::str_of(msg, "ResponseCode").unwrap_or("?"), ?msg_keys, ?item_keys, ?att_keys, ?att_id_keys, ?id_len, ?ck_len, "форма ответа");
    }

    /// Операция записи по элементам (`UpdateItem`, `MoveItem`, `DeleteItem`): пачками того же
    /// размера, что `GetItem`, через `call` с повторным входом; результат по позициям `ids`.
    /// Если сервер отверг всю пачку из-за занятости (`ErrorServerBusy`), это отказ вызова
    /// `Throttled`, а не отказ по элементам.
    ///
    /// `Err` — только если сервер не ответил ни на одну пачку. Отказ следующей пачки после
    /// подтверждённых не теряет их результаты (сервер те письма уже изменил, и вызывающий
    /// не должен откатывать их локально): оставшиеся элементы получают `Failed` с видом ошибки.
    async fn write_items(&self, op: &str, ids: &[ItemId], build: impl Fn(&[ItemId]) -> Value + Send + Sync) -> Result<Vec<WriteResult>> {
        let mut out: Vec<WriteResult> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(self.inner.config.get_item_batch.max(1)) {
            match self.write_chunk(op, chunk, &build).await {
                Ok(results) => out.extend(results),
                Err(e) if out.is_empty() => return Err(e),
                Err(e) => {
                    warn!(op, confirmed = out.len(), rest = ids.len() - out.len(), error = %e, "отказ пачки после подтверждённых: оставшиеся элементы отклонены");
                    fail_rest(&mut out, ids, &e);
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Одна пачка операции записи: один запрос к серверу, результат той же длины, что `chunk`.
    async fn write_chunk(&self, op: &str, chunk: &[ItemId], build: &(impl Fn(&[ItemId]) -> Value + Send + Sync)) -> Result<Vec<WriteResult>> {
        let v = self.call(op, protocol::V_2017, build(chunk)).await?;
        let msgs = protocol::response_messages(&v);
        if msgs.is_empty() {
            if let Some(code) = v.pointer("/Body/ResponseCode").and_then(Value::as_str).filter(|c| *c != "NoError") {
                return Err(TransportError::Server { code: code.to_string(), message: v.pointer("/Body/MessageText").and_then(Value::as_str).unwrap_or("").to_string() });
            }
            return Err(TransportError::Protocol { op: op.into(), status: 200, raw: format!("нет ResponseMessages: {}", truncate(&v.to_string(), 200)) });
        }
        let results = protocol::parse_write_results(&msgs, chunk);
        if results.iter().all(|r| !r.is_ok())
            && let Some(busy) = msgs.iter().find(|m| protocol::message_error(m).map(|(code, _)| code == "ErrorServerBusy").unwrap_or(false))
        {
            let (code, message) = protocol::message_error(busy).expect("проверено выше");
            return Err(server_error(code, message, busy));
        }
        // Форма ответа без содержимого: тип сообщения, код, какие ключи есть у элемента.
        if let Some(first) = msgs.first() {
            let item_keys: Vec<&str> = first.pointer("/Items/0").and_then(Value::as_object).map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
            let msg_keys: Vec<&str> = first.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
            debug!(op, response_type = protocol::str_of(first, "__type").unwrap_or("?"), response_code = protocol::str_of(first, "ResponseCode").unwrap_or("?"), ?msg_keys, ?item_keys, "форма ответа");
        }
        debug!(op, requested = chunk.len(), ok = results.iter().filter(|r| r.is_ok()).count(), "операция записи");
        Ok(results)
    }

    /// Одно окно календаря одной формой запроса: события и признак полной выдачи.
    async fn calendar_window_once(&self, mode: u8, start: Timestamp, end: Timestamp) -> Result<(Vec<CalendarEvent>, bool)> {
        let (op, body) = match mode {
            CALENDAR_MODE_FIND_ITEM_Z => ("FindItem", protocol::find_calendar_view_body(start, end, true, None)),
            CALENDAR_MODE_FIND_ITEM_NAIVE => ("FindItem", protocol::find_calendar_view_body(start, end, false, None)),
            _ => ("GetCalendarView", protocol::get_calendar_view_body(start, end)),
        };
        let v = self.call(op, protocol::V_2017, body).await?;
        let msg = Self::first_message(op, &v)?;
        let (events, includes_last) = protocol::parse_calendar_view(&msg);
        // Форма ответа без содержимого: ключи, формат даты, значения перечислений (не персональные).
        let root = msg.get("RootFolder").unwrap_or(&msg);
        let root_keys: Vec<&str> = root.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
        let first = root.pointer("/Items/0");
        let item_keys: Vec<&str> = first.and_then(Value::as_object).map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
        let start_raw = first.and_then(|i| protocol::str_of(i, "Start")).unwrap_or("");
        let enums: HashSet<String> = root
            .get("Items")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .flat_map(|i| {
                        ["CalendarItemType", "FreeBusyType", "LegacyFreeBusyStatus", "ResponseType", "MyResponseType", "Sensitivity", "AppointmentState", "IsCancelled"]
                            .into_iter()
                            .filter_map(move |k| i.get(k).filter(|x| !x.is_object() && !x.is_array()).map(|x| format!("{k}={x}")))
                    })
                    .collect()
            })
            .unwrap_or_default();
        debug!(op, mode, response_type = protocol::str_of(&msg, "__type").unwrap_or("?"), ?root_keys, ?item_keys, start_raw, items = events.len(), includes_last, total_in_view = ?protocol::i64_of(root, "TotalItemsInView"), ?enums, "форма ответа календаря");
        Ok((events, includes_last))
    }

    /// Окно календаря с подбором формы запроса (один раз на сессию) и делением окна пополам,
    /// пока сервер сообщает неполную выдачу (`IncludesLastItemInRange: false`).
    async fn calendar_window(&self, start: Timestamp, end: Timestamp) -> Result<Vec<CalendarEvent>> {
        let mut mode = self.inner.calendar_mode.load(Ordering::Relaxed);
        let mut out: Vec<CalendarEvent> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut pending: Vec<(Timestamp, Timestamp)> = vec![(start, end)];
        let mut requests = 0usize;
        while let Some((s, e)) = pending.pop() {
            if requests >= CALENDAR_MAX_REQUESTS {
                warn!(requests, "календарь: предел запросов на окно, остаток окна пропущен");
                break;
            }
            requests += 1;
            let (events, includes_last) = if mode == CALENDAR_MODE_UNKNOWN {
                let mut found = None;
                for candidate in CALENDAR_MODES {
                    match self.calendar_window_once(candidate, s, e).await {
                        Ok(r) => {
                            info!(mode = candidate, "календарь: сервер принял форму запроса ({})", calendar_mode_name(candidate));
                            self.inner.calendar_mode.store(candidate, Ordering::Relaxed);
                            mode = candidate;
                            found = Some(r);
                            break;
                        }
                        Err(e) if matches!(e, TransportError::Protocol { .. } | TransportError::Server { .. }) && !e.is_retryable() && candidate != CALENDAR_MODE_GET_CALENDAR_VIEW => {
                            warn!(mode = candidate, error = %e, "календарь: форма {} отвергнута, пробую следующую", calendar_mode_name(candidate));
                        }
                        Err(e) => return Err(e),
                    }
                }
                found.expect("последняя форма либо принята, либо вернула ошибку")
            } else {
                self.calendar_window_once(mode, s, e).await?
            };
            for ev in events {
                if seen.insert(ev.id.id.clone()) {
                    out.push(ev);
                }
            }
            if !includes_last {
                let span = e.duration_since(s);
                if span > CALENDAR_MIN_WINDOW {
                    let mid = s + span / 2;
                    debug!(%s, %e, "календарь: выдача неполная, окно делится пополам");
                    pending.push((mid, e));
                    pending.push((s, mid));
                } else {
                    warn!(%s, %e, "календарь: выдача неполная даже на окне в час, остаток пропущен");
                }
            }
        }
        out.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)).then(a.id.id.cmp(&b.id.id)));
        Ok(out)
    }

    /// Отладка форм протокола: `GetItem` события с `AllProperties`; возвращает форму элемента
    /// ответа без содержимого (`protocol::json_shape`: ключи, длины строк, длины массивов).
    pub async fn probe_calendar_item_shape(&self, id: &ItemId) -> Result<String> {
        let body = protocol::get_calendar_item_body(id, CalendarItemShape::AllProperties, 4096);
        let v = self.call("GetItem", protocol::V_2017, body).await?;
        let msg = Self::first_message("GetItem", &v)?;
        Ok(protocol::json_shape(msg.pointer("/Items/0").unwrap_or(&Value::Null), 3))
    }

    /// Статус ответа канала уведомлений: 440/401 и редирект на вход — истёкшая сессия,
    /// прочие неуспешные — `Protocol` с началом тела.
    async fn check_stream_status(op: &str, r: reqwest::Response) -> Result<reqwest::Response> {
        let status = r.status();
        if status == StatusCode::from_u16(440).unwrap_or(StatusCode::UNAUTHORIZED) || status == StatusCode::UNAUTHORIZED {
            return Err(TransportError::SessionExpired);
        }
        if status.is_redirection() {
            let loc = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            return Err(classify_redirect(&loc));
        }
        if !status.is_success() {
            let text = r.text().await.unwrap_or_default();
            return Err(TransportError::Protocol { op: op.into(), status: status.as_u16(), raw: truncate(&text, 300) });
        }
        Ok(r)
    }

    /// Открывает поток `PendingNotificationRequest`: GET с канарейкой в query (поток читается не
    /// через XHR), без сжатия (чтобы блоки не задерживались в буфере gzip), с собственным общим
    /// таймаутом вместо общего из настроек. Разрешение семафора не берётся.
    async fn open_notification_stream(&self, cid: &str) -> Result<reqwest::Response> {
        let inner = &self.inner;
        let canary = inner.canary.read().expect("canary lock").clone().ok_or(TransportError::SessionExpired)?;
        let n = inner.pending_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let mut url = inner.base.join("owa/ev.owa2").expect("url");
        url.query_pairs_mut()
            .append_pair("ns", "PendingRequest")
            .append_pair("ev", "PendingNotificationRequest")
            .append_pair("UA", "0")
            .append_pair("cid", cid)
            .append_pair("brwnm", "chrome")
            .append_pair("X-OWA-CANARY", &canary)
            .append_pair("n", &n.to_string());
        let mut headers = HeaderMap::new();
        headers.insert("X-OWA-CANARY", HeaderValue::from_str(&canary).map_err(|_| TransportError::CsrfRejected)?);
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/html, */*"));
        let started = Instant::now();
        let r = inner.client.get(url).headers(headers).timeout(STREAM_TOTAL_TIMEOUT).send().await.map_err(net)?;
        self.remember_build(r.headers());
        let r = Self::check_stream_status("PendingNotificationRequest", r).await?;
        let content_type = r.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("?").to_string();
        let encoding = r.headers().get("content-encoding").and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
        let chunked = r.headers().get("transfer-encoding").and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
        debug!(n, status = %r.status(), content_type, encoding, chunked, ms = started.elapsed().as_millis(), "ev.owa2: поток открыт");
        Ok(r)
    }

    /// Читает поток до закрытия сервером: блоки `<script>` собираются из чанков, каждый блок
    /// разбирается в события для `sink`. Тишина дольше `STREAM_IDLE_TIMEOUT` — `Network`.
    /// Обрыв соединения после хотя бы одного блока — обычный конец цикла (`finished_by_server:
    /// false`): следующий цикл откроет поток заново, а настоящая потеря сети проявится на нём.
    async fn read_notification_stream(&self, mut r: reqwest::Response, sink: &(dyn Fn(NotificationEvent) + Send + Sync), started: Instant) -> Result<StreamEnd> {
        let mut parser = notify::ScriptBlockParser::new();
        let mut events = 0usize;
        let mut blocks = 0usize;
        let mut bytes_total = 0usize;
        let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
        let finished_by_server = loop {
            let chunk = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, r.chunk()).await {
                Err(_) => {
                    return Err(TransportError::Network(Box::new(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("канал уведомлений молчит дольше {} с", STREAM_IDLE_TIMEOUT.as_secs())))));
                }
                Ok(Ok(None)) => break true,
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Err(e)) => {
                    if blocks == 0 {
                        return Err(net(e));
                    }
                    warn!(error = %e, blocks, s = started.elapsed().as_secs(), "ev.owa2: соединение оборвалось после блоков, цикл считается законченным");
                    break false;
                }
            };
            bytes_total += chunk.len();
            for block in parser.push(&chunk) {
                blocks += 1;
                // Копия списка подписок (он мал), чтобы не держать замок во время обратного вызова.
                let subs = self.inner.subscriptions.read().expect("subscriptions lock").clone();
                for ev in notify::parse_block(&block, &subs) {
                    *by_kind.entry(ev.kind_name()).or_default() += 1;
                    events += 1;
                    sink(ev);
                }
            }
        };
        if blocks == 0 && finished_by_server {
            return Err(TransportError::Protocol {
                op: "PendingNotificationRequest".into(),
                status: 200,
                raw: format!("поток закрыт сервером без единого блока за {} мс ({} байт, {} в буфере)", started.elapsed().as_millis(), bytes_total, parser.pending()),
            });
        }
        info!(blocks, events, ?by_kind, bytes = bytes_total, s = started.elapsed().as_secs(), finished_by_server, "ev.owa2: цикл потока завершён");
        Ok(StreamEnd { events, elapsed: started.elapsed(), finished_by_server })
    }

    /// `FinishNotificationRequest`: POST с канарейкой и пустым телом; ответ `{cid, syncFnshRq}`.
    async fn finish_once(&self, cid: &str) -> Result<()> {
        let inner = &self.inner;
        let canary = inner.canary.read().expect("canary lock").clone().ok_or(TransportError::SessionExpired)?;
        let mut url = inner.base.join("owa/ev.owa2").expect("url");
        url.query_pairs_mut().append_pair("ns", "PendingRequest").append_pair("ev", "FinishNotificationRequest").append_pair("UA", "0").append_pair("cid", cid);
        let mut headers = HeaderMap::new();
        headers.insert("X-OWA-CANARY", HeaderValue::from_str(&canary).map_err(|_| TransportError::CsrfRejected)?);
        headers.insert("X-Requested-With", HeaderValue::from_static("XMLHttpRequest"));
        // Пустое тело без явной длины IIS отвергает (HTTP 411 Length Required).
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
        let started = Instant::now();
        let r = inner.client.post(url).headers(headers).body(Vec::new()).send().await.map_err(net)?;
        self.remember_build(r.headers());
        let r = Self::check_stream_status("FinishNotificationRequest", r).await?;
        let status = r.status();
        let text = r.text().await.map_err(net)?;
        let shape = serde_json::from_str::<Value>(&text).map(|v| protocol::json_shape(&v, 2)).unwrap_or_else(|_| "не JSON".into());
        debug!(%status, ms = started.elapsed().as_millis(), bytes = text.len(), shape, "ev.owa2: FinishNotificationRequest");
        Ok(())
    }

    /// Первое сообщение ответа, с проверкой `ResponseClass`.
    fn first_message(op: &str, v: &Value) -> Result<Value> {
        let msgs = protocol::response_messages(v);
        let Some(first) = msgs.first() else {
            if let Some(code) = v.pointer("/Body/ResponseCode").and_then(Value::as_str) {
                if code != "NoError" {
                    return Err(TransportError::Server { code: code.to_string(), message: v.pointer("/Body/MessageText").and_then(Value::as_str).unwrap_or("").to_string() });
                }
                return Ok(v["Body"].clone());
            }
            return Err(TransportError::Protocol { op: op.into(), status: 200, raw: format!("нет ResponseMessages: {}", truncate(&v.to_string(), 200)) });
        };
        if let Some((code, message)) = protocol::message_error(first) {
            return Err(server_error(code, message, first));
        }
        Ok((*first).clone())
    }
}

fn calendar_mode_name(mode: u8) -> &'static str {
    match mode {
        CALENDAR_MODE_FIND_ITEM_Z => "FindItem CalendarPageView, даты с Z",
        CALENDAR_MODE_FIND_ITEM_NAIVE => "FindItem CalendarPageView, даты без зоны",
        CALENDAR_MODE_GET_CALENDAR_VIEW => "GetCalendarView",
        _ => "не известно",
    }
}

/// Имя файла из `Content-Disposition`: сначала `filename*=UTF-8''...`, затем `filename="..."`.
fn parse_content_disposition_filename(header: &str) -> Option<String> {
    for part in header.split(';').map(str::trim) {
        if let Some(v) = part.strip_prefix("filename*=") {
            let v = v.trim_matches('"');
            let encoded = v.rsplit("''").next().unwrap_or(v);
            let decoded = percent_encoding::percent_decode_str(encoded).decode_utf8_lossy().into_owned();
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    for part in header.split(';').map(str::trim) {
        if let Some(v) = part.strip_prefix("filename=") {
            let v = v.trim_matches('"').trim();
            if !v.is_empty() {
                // OWA кодирует имя процентами и в обычном filename= (без filename*=).
                let decoded = if v.contains('%') { percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned() } else { v.to_string() };
                return Some(decoded);
            }
        }
    }
    None
}

/// Вид ошибки транспорта как код для `WriteResult::Failed` (имя варианта, без подробностей).
fn error_kind(e: &TransportError) -> &'static str {
    match e {
        TransportError::SessionExpired => "SessionExpired",
        TransportError::CsrfRejected => "CsrfRejected",
        TransportError::SessionLimit => "SessionLimit",
        TransportError::AuthFailed(_) => "AuthFailed",
        TransportError::Throttled { .. } => "Throttled",
        TransportError::Network(_) => "Network",
        TransportError::Protocol { .. } => "Protocol",
        TransportError::Server { .. } => "Server",
        TransportError::Cancelled => "Cancelled",
        TransportError::TooLarge { .. } => "TooLarge",
    }
}

/// Дописывает в `out` отказ `Failed` по всем элементам `ids`, на которые ответа ещё нет
/// (`out` содержит результаты первых `out.len()` элементов), так что `out.len() == ids.len()`.
fn fail_rest(out: &mut Vec<WriteResult>, ids: &[ItemId], e: &TransportError) {
    let code = error_kind(e);
    let message = e.to_string();
    let rest = &ids[out.len().min(ids.len())..];
    out.extend(rest.iter().map(|id| WriteResult::Failed { id: id.clone(), code: code.into(), message: message.clone() }));
}

fn server_error(code: String, message: String, msg: &Value) -> TransportError {
    if code == "ErrorServerBusy" {
        let retry_after = msg
            .to_string()
            .split("BackOffMilliseconds")
            .nth(1)
            .and_then(|s| s.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u64>().ok())
            .map(Duration::from_millis);
        return TransportError::Throttled { retry_after };
    }
    TransportError::Server { code, message }
}

#[async_trait]
impl MailTransport for OwaTransport {
    async fn sync_folder_hierarchy(&self, state: Option<&SyncState>) -> Result<FolderChanges> {
        let mut with_distinguished = true;
        loop {
            let body = protocol::sync_folder_hierarchy_body(state, with_distinguished);
            match self.call("SyncFolderHierarchy", protocol::V_2013, body).await.and_then(|v| Self::first_message("SyncFolderHierarchy", &v)) {
                Ok(msg) => {
                    let (state, changes, includes_last) = protocol::parse_folder_changes(&msg);
                    return Ok(FolderChanges { state, changes, includes_last });
                }
                Err(TransportError::Server { code, .. }) if with_distinguished && code.contains("Property") => {
                    debug!("сервер не принял DistinguishedFolderId в FolderShape, повтор без него");
                    with_distinguished = false;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn sync_folder_items(&self, folder: &FolderRef, state: Option<&SyncState>, max_changes: u32, with_meta: bool) -> Result<ItemChanges> {
        let max = max_changes.clamp(1, 512);
        let body = protocol::sync_folder_items_body(folder, state, max, with_meta);
        let v = self.call("SyncFolderItems", protocol::V_2013, body).await?;
        let msg = Self::first_message("SyncFolderItems", &v)?;
        let (state, changes, includes_last) = protocol::parse_item_changes(&msg, max);
        Ok(ItemChanges { state, changes, includes_last })
    }

    async fn get_items(&self, ids: &[ItemId], shape: ItemFetchShape) -> Result<Vec<ItemResult>> {
        let shape_name = match shape {
            ItemFetchShape::Meta => "MailListItem",
            ItemFetchShape::WithUniqueBody => "ItemPart",
            ItemFetchShape::WithFullBody => "ItemNormalizedBody",
        };
        let mut out: Vec<ItemResult> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(self.inner.config.get_item_batch.max(1)) {
            let body = protocol::get_item_body(chunk, shape_name, self.inner.config.max_body_size);
            let v = self.call("GetItem", protocol::V_2017, body).await?;
            let msgs = protocol::response_messages(&v);
            if msgs.is_empty() {
                return Err(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: truncate(&v.to_string(), 200) });
            }
            // Сервер отвечает одним ResponseMessage на каждый id в порядке запроса. Если число не
            // сошлось, сопоставляем по ItemId и считаем отсутствующие отказом.
            let by_position = msgs.len() == chunk.len();
            let mut parsed: Vec<Option<ItemResult>> = vec![None; chunk.len()];
            let mut throttled: Option<TransportError> = None;
            for (pos, msg) in msgs.iter().enumerate() {
                let item_value = msg.get("Items").and_then(Value::as_array).and_then(|a| a.first());
                let idx = if by_position {
                    Some(pos)
                } else {
                    item_value.and_then(|iv| iv.get("ItemId")).and_then(protocol::parse_item_id).and_then(|id| chunk.iter().position(|c| c.id == id.id))
                };
                let Some(idx) = idx else { continue };
                if let Some((code, message)) = protocol::message_error(msg) {
                    if code == "ErrorServerBusy" {
                        throttled = Some(server_error(code.clone(), message.clone(), msg));
                    }
                    parsed[idx] = Some(ItemResult::Failed { id: chunk[idx].clone(), code, message });
                    continue;
                }
                if let Some(full) = item_value.and_then(protocol::parse_message_full) {
                    parsed[idx] = Some(ItemResult::Ok(full));
                }
            }
            let all_failed = parsed.iter().all(|p| matches!(p, Some(ItemResult::Failed { .. }) | None));
            if all_failed {
                if let Some(t) = throttled {
                    return Err(t);
                }
            }
            for (i, p) in parsed.into_iter().enumerate() {
                out.push(p.unwrap_or_else(|| ItemResult::Failed { id: chunk[i].clone(), code: "NoResponse".into(), message: "сервер не вернул ответ по элементу".into() }));
            }
        }
        Ok(out)
    }

    async fn get_folder_counts(&self, folders: &[FolderRef]) -> Result<Vec<FolderCounts>> {
        let v = self.call("GetFolder", protocol::V_2013, protocol::get_folder_body(folders)).await?;
        let mut out = Vec::new();
        for msg in protocol::response_messages(&v) {
            if protocol::message_error(msg).is_some() {
                continue;
            }
            if let Some(f) = msg.pointer("/Folders/0") {
                if let Some(id) = f.get("FolderId").and_then(protocol::parse_folder_id) {
                    out.push(FolderCounts { id, total: protocol::i64_of(f, "TotalCount").unwrap_or(0), unread: protocol::i64_of(f, "UnreadCount").unwrap_or(0) });
                }
            }
        }
        Ok(out)
    }

    async fn get_attachment(&self, attachment_id: &str, max_bytes: u64) -> Result<AttachmentContent> {
        self.get_attachment_with_relogin(attachment_id, max_bytes).await
    }

    async fn set_read(&self, ids: &[ItemId], read: bool) -> Result<Vec<WriteResult>> {
        self.write_items("UpdateItem", ids, |chunk| protocol::set_read_body(chunk, read)).await
    }

    async fn set_flag(&self, ids: &[ItemId], flagged: bool) -> Result<Vec<WriteResult>> {
        self.write_items("UpdateItem", ids, |chunk| protocol::set_flag_body(chunk, flagged)).await
    }

    async fn move_items(&self, ids: &[ItemId], to_folder: &FolderRef) -> Result<Vec<WriteResult>> {
        self.write_items("MoveItem", ids, |chunk| protocol::move_item_body(chunk, to_folder)).await
    }

    async fn delete_items(&self, ids: &[ItemId], mode: DeleteMode) -> Result<Vec<WriteResult>> {
        let delete_type = match mode {
            DeleteMode::ToDeletedItems => "MoveToDeletedItems",
            DeleteMode::Hard => "HardDelete",
        };
        self.write_items("DeleteItem", ids, |chunk| protocol::delete_item_body(chunk, delete_type)).await
    }

    async fn create_draft(&self, kind: &ComposeKind, msg: &OutgoingMessage) -> Result<ItemId> {
        let v = self.call("CreateItem", protocol::V_2017, protocol::create_item_body(kind, msg)).await?;
        let m = Self::first_message("CreateItem", &v)?;
        Self::log_shape("CreateItem", &m);
        protocol::parse_created_item_id(&m).ok_or_else(|| TransportError::Protocol { op: "CreateItem".into(), status: 200, raw: format!("нет Items[0].ItemId: {}", truncate(&m.to_string(), 200)) })
    }

    async fn add_attachments(&self, item: &ItemId, files: &[OutgoingAttachment]) -> Result<ItemId> {
        let mut current = item.clone();
        for (i, f) in files.iter().enumerate() {
            let body = protocol::create_attachment_body(&current, f);
            let v = self.call_with_timeout("CreateAttachment", protocol::V_2017, body, Some(self.inner.config.upload_timeout)).await?;
            let m = Self::first_message("CreateAttachment", &v)?;
            Self::log_shape("CreateAttachment", &m);
            match protocol::parse_attachment_root(&m) {
                Some(root) => {
                    debug!(index = i, bytes = f.bytes.len(), root_same = root.id == current.id, ck_len = root.change_key.as_deref().map(str::len), "вложение приложено");
                    current = root;
                }
                None => {
                    warn!(index = i, "CreateAttachment: в ответе нет RootItemId, ключ черновика не обновлён");
                }
            }
        }
        Ok(current)
    }

    async fn send_draft(&self, item: &ItemId) -> Result<()> {
        let v = self.call("SendItem", protocol::V_2017, protocol::send_item_body(item)).await?;
        let m = Self::first_message("SendItem", &v)?;
        Self::log_shape("SendItem", &m);
        Ok(())
    }

    async fn update_draft(&self, item: &ItemId, msg: &OutgoingMessage) -> Result<ItemId> {
        let v = self.call("UpdateItem", protocol::V_2017, protocol::update_draft_body(item, msg)).await?;
        let m = Self::first_message("UpdateItem", &v)?;
        Self::log_shape("UpdateItem", &m);
        Ok(protocol::written_item_id(&m).unwrap_or_else(|| item.clone()))
    }

    /// Объект `OWA.UserOptions` ищется сначала в корне ящика (`root`, как в EWS Managed API),
    /// затем в корне почтовых папок (`msgfolderroot`); `ErrorItemNotFound` в обоих — подписи нет.
    async fn user_signature(&self) -> Result<Option<Signature>> {
        for base in ["root", "msgfolderroot"] {
            let v = self.call("GetUserConfiguration", protocol::V_2013, protocol::get_user_configuration_body(base)).await?;
            match Self::first_message("GetUserConfiguration", &v) {
                Ok(m) => {
                    let dict = protocol::user_configuration_dictionary(&m);
                    let keys: Vec<&str> = dict.iter().map(|(k, _)| k.as_str()).collect();
                    debug!(base, entries = dict.len(), ?keys, shape = %protocol::json_shape(&m, 5), "GetUserConfiguration: словарь OWA.UserOptions");
                    return Ok(protocol::parse_user_signature(&m));
                }
                Err(TransportError::Server { code, message }) if code == "ErrorItemNotFound" => {
                    debug!(base, message, "GetUserConfiguration: объекта OWA.UserOptions нет в этой папке");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    async fn find_calendar_view(&self, start: Timestamp, end: Timestamp) -> Result<Vec<CalendarEvent>> {
        if end <= start {
            return Ok(Vec::new());
        }
        let started = std::time::Instant::now();
        let events = self.calendar_window(start, end).await?;
        debug!(%start, %end, events = events.len(), ms = started.elapsed().as_millis(), "календарь: окно загружено");
        Ok(events)
    }

    /// `GetItem` события: сначала `IdOnly` с явным списком свойств, при отказе сервера по форме
    /// (неизвестное свойство) — `AllProperties`; принятая форма запоминается на сессию.
    async fn get_calendar_item(&self, id: &ItemId) -> Result<CalendarEventDetails> {
        let known = self.inner.calendar_shape.load(Ordering::Relaxed);
        let candidates: &[(u8, CalendarItemShape)] = match known {
            CALENDAR_SHAPE_ALL => &[(CALENDAR_SHAPE_ALL, CalendarItemShape::AllProperties)],
            CALENDAR_SHAPE_EXPLICIT => &[(CALENDAR_SHAPE_EXPLICIT, CalendarItemShape::Explicit)],
            _ => &[(CALENDAR_SHAPE_EXPLICIT, CalendarItemShape::Explicit), (CALENDAR_SHAPE_ALL, CalendarItemShape::AllProperties)],
        };
        let mut last_err: Option<TransportError> = None;
        for (i, (code, shape)) in candidates.iter().enumerate() {
            let body = protocol::get_calendar_item_body(id, *shape, self.inner.config.max_body_size);
            let result = self.call("GetItem", protocol::V_2017, body).await.and_then(|v| Self::first_message("GetItem", &v));
            let msg = match result {
                Ok(m) => m,
                // Отказ по форме запроса (HTTP 500 на неизвестное свойство, ErrorInvalidPropertyRequest и т. п.):
                // пробуем следующую форму. Отказ по элементу (ErrorItemNotFound) и сессия — наверх.
                Err(e) if i + 1 < candidates.len() && (matches!(&e, TransportError::Protocol { status: 500, .. }) || matches!(&e, TransportError::Server { code, .. } if code.contains("Property") || code.contains("Shape"))) => {
                    warn!(error = %e, "GetItem события: форма {:?} отвергнута, пробую следующую", shape);
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            };
            Self::log_shape("GetItem", &msg);
            let Some(item) = msg.pointer("/Items/0") else {
                return Err(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: format!("нет Items[0]: {}", truncate(&msg.to_string(), 200)) });
            };
            let Some(details) = protocol::parse_calendar_details(item) else {
                let keys: Vec<&str> = item.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
                if i + 1 < candidates.len() {
                    warn!(?keys, "GetItem события: в ответе нет Start/End, пробую AllProperties");
                    last_err = Some(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: format!("нет Start/End у события, ключи {keys:?}") });
                    continue;
                }
                return Err(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: format!("нет Start/End у события, ключи {keys:?}") });
            };
            if known == CALENDAR_SHAPE_UNKNOWN {
                info!(shape = ?shape, keys = item.as_object().map(|o| o.len()).unwrap_or(0), "GetItem события: сервер принял форму");
            }
            self.inner.calendar_shape.store(*code, Ordering::Relaxed);
            let mut details = details;
            // У экземпляра повторяющейся встречи образца повторения нет: он у её «мастера»,
            // который достаётся по идентификатору экземпляра. Отказ сервера здесь не фатален.
            if details.recurrence_text.is_none() && details.event.calendar_type.is_recurring() {
                match self.call("GetItem", protocol::V_2017, protocol::get_recurring_master_body(id)).await.and_then(|v| Self::first_message("GetItem", &v)) {
                    Ok(m) => {
                        let rec = m.pointer("/Items/0/Recurrence");
                        details.recurrence_text = rec.and_then(protocol::recurrence_text);
                        let master_keys: Vec<&str> = m.pointer("/Items/0").and_then(|i| i.as_object()).map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
                        let recurrence_shape = rec.map(|r| protocol::json_shape(r, 3)).unwrap_or_default();
                        debug!(?master_keys, recurrence_shape, text = details.recurrence_text.is_some(), "GetItem мастера повторяющейся встречи");
                    }
                    Err(e) => debug!(error = %e, "GetItem мастера повторяющейся встречи отвергнут, описание повторения не будет показано"),
                }
            }
            debug!(
                body = details.body.as_ref().and_then(|b| b.full_body.as_ref()).map(|s| s.chars().count()),
                attendees = details.attendees.len(),
                reminder = ?details.reminder_minutes,
                recurrence = details.recurrence_text.is_some(),
                recurrence_shape = item.get("Recurrence").map(|r| protocol::json_shape(r, 3)).unwrap_or_default(),
                "GetItem события"
            );
            return Ok(details);
        }
        Err(last_err.unwrap_or(TransportError::Protocol { op: "GetItem".into(), status: 0, raw: "ни одна форма не принята".into() }))
    }

    /// `SubscribeToNotification` через `call_raw` с подбором формы (один раз на сессию): форма
    /// браузерной OWA, при отказе сервера по форме (HTTP 500 или ни одной созданной подписки)
    /// — запасные. Созданные подписки запоминаются для разбора событий потока.
    async fn subscribe_notifications(&self, subs: &[SubscriptionRequest]) -> Result<Vec<SubscriptionResult>> {
        if subs.is_empty() {
            return Ok(Vec::new());
        }
        let known = SubscribeForm::from_code(self.inner.subscribe_form.load(Ordering::Relaxed));
        let candidates: Vec<SubscribeForm> = match known {
            Some(f) => vec![f],
            None => SubscribeForm::ALL.to_vec(),
        };
        let mut last: Option<Result<Vec<SubscriptionResult>>> = None;
        for (i, form) in candidates.iter().enumerate() {
            let more = i + 1 < candidates.len();
            let body = notify::subscribe_body(subs, &self.inner.config.time_zone, *form);
            match self.call_raw("SubscribeToNotification", body).await {
                Ok(v) => {
                    let results = notify::parse_subscription_results(&v, subs);
                    let created = results.iter().filter(|r| r.created).count();
                    debug!(form = ?form, requested = subs.len(), created, shape = %protocol::json_shape(&v, 3), "SubscribeToNotification: форма ответа");
                    for (req, res) in subs.iter().zip(&results) {
                        if res.created {
                            debug!(kind = req.kind.name(), "подписка создана");
                        } else {
                            info!(kind = req.kind.name(), error = res.error.as_deref().unwrap_or("?"), "подписка не создана");
                        }
                    }
                    if created > 0 {
                        if known.is_none() {
                            info!(form = form.describe(), "подписки: сервер принял форму запроса");
                        }
                        self.inner.subscribe_form.store(form.code(), Ordering::Relaxed);
                        let mut map = self.inner.subscriptions.write().expect("subscriptions lock");
                        for (req, res) in subs.iter().zip(&results) {
                            if res.created {
                                map.insert(req.id.clone(), req.kind.clone());
                            }
                        }
                        self.inner.subscriptions_stale.store(false, Ordering::SeqCst);
                        return Ok(results);
                    }
                    if more {
                        warn!(form = form.describe(), "подписки: ни одна не создана, пробую следующую форму");
                    }
                    last = Some(Ok(results));
                }
                Err(e) if more && matches!(e, TransportError::Protocol { status: 500, .. } | TransportError::Server { .. }) => {
                    warn!(form = form.describe(), error = %e, "подписки: форма отвергнута, пробую следующую");
                    last = Some(Err(e));
                }
                Err(e) => return Err(e),
            }
        }
        last.unwrap_or_else(|| Err(TransportError::Protocol { op: "SubscribeToNotification".into(), status: 0, raw: "ни одна форма не принята".into() }))
    }

    /// Один цикл `PendingNotificationRequest` вне семафора. Истёкшая сессия — один повторный
    /// вход и новая попытка открыть поток; потерянные подписки объявляются `Reload` в `sink`.
    async fn notification_stream(&self, cid: &str, sink: &(dyn Fn(NotificationEvent) + Send + Sync)) -> Result<StreamEnd> {
        let started = Instant::now();
        for attempt in 0..2 {
            let generation = self.inner.generation.load(Ordering::SeqCst);
            match self.open_notification_stream(cid).await {
                Ok(r) => {
                    if self.inner.subscriptions_stale.swap(false, Ordering::SeqCst) {
                        sink(NotificationEvent::Reload);
                    }
                    return self.read_notification_stream(r, sink, started).await;
                }
                Err(e) if e.needs_reauth() && attempt == 0 => {
                    self.relogin_once(generation).await?;
                }
                Err(e) => return Err(e),
            }
        }
        Err(TransportError::SessionExpired)
    }

    async fn finish_notification_stream(&self, cid: &str) -> Result<()> {
        let _permit = self.inner.permits.acquire().await.map_err(|_| TransportError::Cancelled)?;
        for attempt in 0..2 {
            let generation = self.inner.generation.load(Ordering::SeqCst);
            match self.finish_once(cid).await {
                Err(e) if e.needs_reauth() && attempt == 0 => {
                    self.relogin_once(generation).await?;
                }
                other => return other,
            }
        }
        Err(TransportError::SessionExpired)
    }

    async fn logoff(&self) -> Result<()> {
        // Сначала защёлка, потом запрос: даже если GET не дошёл, транспорт больше не входит.
        self.inner.closed.store(true, Ordering::SeqCst);
        *self.inner.canary.write().expect("canary lock") = None;
        let url = self.inner.base.join("owa/logoff.owa").expect("url");
        let r = self.inner.client.get(url).send().await.map_err(net)?;
        debug!(status = %r.status(), "logoff.owa");
        Ok(())
    }

    fn server_info(&self) -> ServerInfo {
        ServerInfo { host: self.inner.creds.host.clone(), build: self.inner.build.read().expect("build lock").clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_classification() {
        assert!(matches!(classify_redirect("/owa/auth/logon.aspx?reason=0"), TransportError::SessionExpired));
        assert!(matches!(classify_redirect("/owa/auth/errorFE.aspx?httpCode=500"), TransportError::CsrfRejected));
        assert!(matches!(classify_redirect("/owa/auth/errorfe.aspx?httpCode=500&inex=Microsoft.Mapi.MapiExceptionSessionLimit"), TransportError::SessionLimit));
    }

    #[test]
    fn content_disposition_filename() {
        assert_eq!(parse_content_disposition_filename("attachment; filename=\"a.pdf\""), Some("a.pdf".into()));
        assert_eq!(parse_content_disposition_filename("attachment; filename=\"a.pdf\"; filename*=UTF-8''%D0%A1%D1%87%D1%91%D1%82.pdf"), Some("Счёт.pdf".into()));
        assert_eq!(parse_content_disposition_filename("attachment; filename=%d0%a1%d1%87%d1%91%d1%82.pdf"), Some("Счёт.pdf".into()), "OWA кодирует процентами обычный filename");
        assert_eq!(parse_content_disposition_filename("inline"), None);
    }

    /// После `logoff` клон транспорта не входит заново: `call` отдаёт `SessionExpired`,
    /// поколение сессии не растёт (ни одного POST auth.owa).
    #[tokio::test]
    async fn logged_off_transport_does_not_relogin() {
        let creds = Credentials { host: "127.0.0.1:1".into(), user: "u".into(), password: secrecy::SecretString::from("p".to_string()) };
        let t = OwaTransport::prepare(creds, OwaConfig { timeout: Duration::from_secs(2), ..OwaConfig::default() }).unwrap();
        let clone = t.clone();
        // Сервера нет, GET logoff.owa упадёт сетью — защёлка всё равно выставлена.
        let _ = t.logoff().await;
        assert!(t.inner.closed.load(Ordering::SeqCst));
        let r = clone.call("GetItem", protocol::V_2017, serde_json::json!({})).await;
        assert!(matches!(r, Err(TransportError::SessionExpired)), "{r:?}");
        assert_eq!(t.inner.generation.load(Ordering::SeqCst), 0, "повторного входа не было");
    }

    /// Отказ второй пачки после подтверждённой первой: результаты первой остаются, оставшиеся
    /// элементы получают `Failed` с видом ошибки, длина и порядок — как у `ids`.
    #[test]
    fn failed_batch_after_confirmed_one_keeps_results_and_fails_the_rest() {
        let ids: Vec<ItemId> = (1..=4).map(|i| ItemId { id: format!("id{i}"), change_key: None }).collect();
        let mut out = vec![WriteResult::Ok { id: ids[0].clone() }, WriteResult::Failed { id: ids[1].clone(), code: "ErrorItemNotFound".into(), message: String::new() }];
        fail_rest(&mut out, &ids, &TransportError::Throttled { retry_after: None });
        assert_eq!(out.len(), ids.len());
        assert!(out[0].is_ok(), "подтверждённый результат первой пачки сохранён");
        assert!(matches!(&out[1], WriteResult::Failed { code, .. } if code == "ErrorItemNotFound"), "отказ по элементу первой пачки сохранён");
        for (r, id) in out[2..].iter().zip(&ids[2..]) {
            match r {
                WriteResult::Failed { id: rid, code, message } => {
                    assert_eq!(rid, id, "порядок как у ids");
                    assert_eq!(code, "Throttled");
                    assert!(!message.is_empty());
                }
                other => panic!("ожидался Failed: {other:?}"),
            }
        }
        // Ничего не подтверждено — дописывать нечего сверх ids; `out` длиннее ids не становится.
        let mut full = out.clone();
        fail_rest(&mut full, &ids, &TransportError::SessionExpired);
        assert_eq!(full.len(), ids.len(), "после полного ответа ничего не добавляется");
        assert_eq!(error_kind(&TransportError::Network(Box::new(std::io::Error::other("x")))), "Network");
    }

    #[test]
    fn server_busy_maps_to_throttled() {
        let msg: Value = serde_json::json!({"ResponseClass":"Error","ResponseCode":"ErrorServerBusy","MessageXml":{"Value":{"Name":"BackOffMilliseconds","Value":"2500"}}});
        match server_error("ErrorServerBusy".into(), String::new(), &msg) {
            TransportError::Throttled { retry_after } => assert_eq!(retry_after, Some(Duration::from_millis(2500))),
            other => panic!("{other:?}"),
        }
    }
}
