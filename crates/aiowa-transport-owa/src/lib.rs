//! Транспорт поверх внутреннего JSON API Outlook Web App (Exchange 2016/2019).
//!
//! Факты, на которых всё держится (см. `docs/protocol-owa-json.md`):
//! вход через форму `/owa/auth.owa`; cookie `X-OWA-CANARY` дублируется заголовком;
//! `__type` первым ключом каждого объекта; системный TLS; одна сессия на процесс и
//! явный `logoff.owa`; истечение сессии = 440/401/редирект на `logon.aspx`.

pub mod mailbox;
pub mod notify;
pub mod protocol;

use aiowa_core::{
    CalendarEvent, CalendarEventDetails, CalendarItemChanges, ComposeKind, ContactChange, ContactChanges, ContactItem, DirectoryPerson, DraftContent, FolderChanges, FolderId, FolderRef, ItemChanges, ItemId, MeetingRequestInfo, MeetingResponse, MessageMeta,
    NewCalendarItem, OutgoingAttachment, OutgoingMessage, Signature, SyncState,
};
use aiowa_transport::{
    async_trait, AttachmentContent, Credentials, DeleteMode, FolderCounts, ItemFetchShape, ItemResult, MailTransport, NotificationEvent, NotificationKind, Result, ServerInfo, StreamEnd, SubscriptionRequest,
    SubscriptionResult, TransportError, WriteResult,
 ResolvedPerson,};
use base64::prelude::*;
use jiff::Timestamp;
use notify::SubscribeForm;
use protocol::{CalendarItemShape, DraftShape};
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
    /// Только для тестов: базовый URL сервера вместо `https://{Credentials::host}/`, например
    /// `http://127.0.0.1:порт/` фейкового сервера OWA (`tests/common`). С ним клиент не берёт
    /// системный прокси, чтобы тестовый трафик не уходил дальше петлевого адреса. `None` (по
    /// умолчанию) — обычная работа по HTTPS.
    pub base_url: Option<String>,
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
            base_url: None,
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
    /// Какую форму `ItemShape` для `GetItem` письма о встрече принял сервер (`CALENDAR_SHAPE_*`).
    meeting_shape: AtomicU8,
    /// Какую форму `GetItem` черновика принял сервер (`DRAFT_SHAPE_*`).
    draft_shape: AtomicU8,
    /// Принимает ли сервер `message:ReplyTo` в `AdditionalProperties` поверх `ShapeName` у
    /// `GetItem` писем (`REPLY_TO_*`): при отказе по форме тела запрашиваются без него.
    reply_to_mode: AtomicU8,
    /// Сколько раз подряд `GetItem` с `message:ReplyTo` получил HTTP 500, а та же пачка без него
    /// прошла. Отказ по форме признаётся на `REPLY_TO_STRIKES`-м разе: разовый сбой сервера не
    /// должен отключать Reply-To до конца работы процесса (транспорт живёт столько же). Ответ 200
    /// на запрос со свойством сбрасывает счёт; параллельные пачки одного сбоя — один удар.
    reply_to_strikes: AtomicU8,
    /// Какую `RequestServerVersion` принял сервер для операций с папками (`FOLDER_VERSION_*`).
    folder_version: AtomicU8,
    /// Какую форму `SubscribeToNotification` принял сервер (`SubscribeForm::code`, 0 не известно).
    subscribe_form: AtomicU8,
    /// Сервер отверг дополнительные свойства записи `FindPeople` (`protocol::PERSONA_EXTRA_FIELDS`):
    /// поиск по справочнику идёт формой `Default` без них до конца жизни транспорта.
    persona_extra_rejected: AtomicBool,
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
/// Таймаут попутного `logoff.owa` при повторном входе: исход не важен, а ждать общий таймаут
/// под мьютексом входа и с занятым разрешением семафора дорого.
const LOGOFF_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Форма `GetItem` черновика: не проверено, `AllProperties` с `BodyType: HTML`, `ItemNormalizedBody`.
const DRAFT_SHAPE_UNKNOWN: u8 = 0;
const DRAFT_SHAPE_ALL: u8 = 1;
const DRAFT_SHAPE_NORMALIZED: u8 = 2;

/// `message:ReplyTo` в `GetItem` писем: не проверено, принято, отвергнуто сервером.
const REPLY_TO_UNKNOWN: u8 = 0;
const REPLY_TO_ACCEPTED: u8 = 1;
const REPLY_TO_REJECTED: u8 = 2;
/// Сколько отказов HTTP 500 подряд (при успешном повторе без свойства) считать отказом по форме.
const REPLY_TO_STRIKES: u8 = 2;

/// Версия схемы операций с папками: сначала `V2017_08_18` (как у операций записи писем), при
/// HTTP 500 на форму запроса — `Exchange2013` (как у `SyncFolderHierarchy`/`GetFolder`).
const FOLDER_VERSION_UNKNOWN: u8 = 0;
const FOLDER_VERSION_2017: u8 = 1;
const FOLDER_VERSION_2013: u8 = 2;

/// Самое узкое окно, на которое делится выдача календаря при `IncludesLastItemInRange: false`.
const CALENDAR_MIN_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_hours(1);
/// Предел запросов на одно окно календаря (защита от бесконечного деления).
const CALENDAR_MAX_REQUESTS: usize = 64;

/// Транспорт OWA JSON. Дёшево клонируется (Arc внутри).
#[derive(Clone)]
pub struct OwaTransport {
    inner: Arc<Inner>,
}

fn net(mut e: reqwest::Error) -> TransportError {
    // reqwest дописывает в текст ошибки адрес запроса, а в query бывает X-OWA-CANARY (значение
    // cookie сессии, GetFileAttachment): в ошибку окна и журнал он не идёт, хост и путь остаются.
    if let Some(u) = e.url_mut() {
        u.set_query(None);
    }
    // Соединение не установилось — запрос не ушёл; всё остальное — после отправки.
    if e.is_connect() { TransportError::Connect(Box::new(e)) } else { TransportError::Network(Box::new(e)) }
}

/// Таймаут установления соединения. Windows повторяет SYN на 1, 3, 7 и 15 с; прежние 10 с
/// отсекали последний повтор и почти не оставляли времени на TLS (docs/obkatka.md п. 22).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Сколько держать простаивающее соединение в пуле. Чуть больше прежних 90 с по умолчанию:
/// таймеры календаря и синхронизации реже, и почти каждый их запрос открывал соединение заново.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(105);
/// TCP keepalive простаивающих соединений: сетевое оборудование по пути не закрывает их молча.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
/// Повторов, когда соединение не установилось (запрос не ушёл).
const CONNECT_RETRIES: u32 = 2;

/// Можно ли повторить операцию после обрыва, когда неизвестно, дошла ли она до сервера.
/// Чтения — да; `UpdateItem` идёт с `AlwaysOverwrite` — да (кроме записи календаря с рассылкой
/// участникам, `update_calendar_item`); создание, отправка, перенос, удаление и подписки — нет
/// (повтор дал бы дубль или второе действие).
pub(crate) fn is_idempotent(op: &str) -> bool {
    !matches!(
        op,
        "CreateItem"
            | "SendItem"
            | "CreateAttachment"
            | "DeleteAttachment"
            | "MoveItem"
            | "DeleteItem"
            | "CopyItem"
            | "CreateFolder"
            | "DeleteFolder"
            | "MoveFolder"
            | "UpdateFolder"
            | "EmptyFolder"
            | "SubscribeToNotification"
            | "UnsubscribeFromNotification"
            // Персоны (личные контакты, § 5r): повтор создания — второй контакт, правки — запись
            // поверх с уже устаревшим `OldValue`.
            | "CreatePersona"
            | "UpdatePersona"
            | "DeletePersonas"
    )
}

/// Отправка запроса с повторами. Соединение не установилось (запрос не ушёл): до
/// `CONNECT_RETRIES` повторов для любой операции с паузой 1 и 2 с. Обрыв после отправки
/// (сброс, ошибка чтения ответа): один повтор только для идемпотентных операций; таймаут ответа
/// не повторяется — ждать ещё раз столько же нет смысла. Если после обрыва после отправки
/// следующая попытка не соединилась, ошибка — `Network`, не `Connect`: первая попытка могла дойти
/// до сервера. В журнал — вид сбоя, длительность и цепочка причин; адрес запроса без query (`net`).
pub(crate) async fn send_retrying(req: reqwest::RequestBuilder, op: &str, idempotent: bool) -> Result<reqwest::Response> {
    let mut attempt = 0u32;
    // Была попытка, оборванная после отправки: исход запроса неизвестен.
    let mut sent = false;
    loop {
        let Some(this) = req.try_clone() else {
            // Тело-поток клонировать нельзя: без повторов.
            return req.send().await.map_err(net);
        };
        let started = Instant::now();
        match this.send().await {
            Ok(r) => {
                if attempt > 0 {
                    info!(op, attempt, "запрос прошёл после повтора");
                }
                return Ok(r);
            }
            Err(e) => {
                let connect = e.is_connect();
                let timeout = e.is_timeout();
                let retry = if connect { attempt < CONNECT_RETRIES } else { idempotent && !timeout && attempt < 1 };
                let err = match net(e) {
                    TransportError::Connect(e) if sent => TransportError::Network(e),
                    err => err,
                };
                sent |= !connect;
                warn!(op, attempt, connect, timeout, idempotent, retry, ms = started.elapsed().as_millis() as u64, error = %err.chain(), "сетевой сбой запроса");
                if !retry {
                    return Err(err);
                }
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
            }
        }
    }
}

/// Начало ответа, который не разобрался как JSON, — для текста ошибки. Похожее на JSON (обрезанный
/// ответ с письмами) не показывается вовсе: в ошибку и журнал не должно попасть содержимое писем
/// (политика журналов). Страница входа или ошибки IIS — показывается началом.
fn non_json_excerpt(text: &str) -> String {
    let t = text.trim_start();
    if t.starts_with('{') || t.starts_with('[') {
        "(похоже на JSON, не показывается)".into()
    } else {
        truncate(t, 160)
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// Классификация редиректа после запроса к сессии.
/// Ответ пробного GET `service.svc/s/<op>` (`OwaTransport::service_get_probe`).
#[derive(Debug, Clone)]
pub struct ServiceGetReply {
    pub status: u16,
    pub content_type: Option<String>,
    pub etag: bool,
    pub cache_control: Option<String>,
    /// `Location` редиректа; может содержать адрес из запроса — не печатать.
    pub location: Option<String>,
    pub bytes: Vec<u8>,
}

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
        let mut builder = Client::builder()
            .cookie_provider(jar.clone())
            .redirect(Policy::none())
            .user_agent(&config.user_agent)
            .http1_only()
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .tcp_keepalive(TCP_KEEPALIVE)
            .timeout(config.timeout);
        // Тестовый сервер на петлевом адресе: прямое соединение, без системного прокси.
        if config.base_url.is_some() {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(net)?;
        let base = match config.base_url.as_deref() {
            // `join` относительных путей требует завершающего `/`.
            Some(url) if url.ends_with('/') => Url::parse(url),
            Some(url) => Url::parse(&format!("{url}/")),
            None => Url::parse(&format!("https://{}/", creds.host)),
        }
        .map_err(|e| TransportError::Protocol { op: "config".into(), status: 0, raw: e.to_string() })?;
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
            meeting_shape: AtomicU8::new(CALENDAR_SHAPE_UNKNOWN),
            draft_shape: AtomicU8::new(DRAFT_SHAPE_UNKNOWN),
            reply_to_mode: AtomicU8::new(REPLY_TO_UNKNOWN),
            reply_to_strikes: AtomicU8::new(0),
            folder_version: AtomicU8::new(FOLDER_VERSION_UNKNOWN),
            subscribe_form: AtomicU8::new(0),
            persona_extra_rejected: AtomicBool::new(false),
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
        let _ = send_retrying(inner.client.get(logon).query(&[("url", owa.as_str()), ("reason", "0")]), "logon.aspx", true).await?;

        let form = [
            ("destination", owa.as_str()),
            ("flags", "4"),
            ("forcedownlevel", "0"),
            ("username", inner.creds.user.as_str()),
            ("password", inner.creds.password.expose_secret()),
            ("passwordText", ""),
            ("isUtf8", "1"),
        ];
        // Повтор только если соединение не установилось: ушедший вход мог создать сессию.
        let r = send_retrying(inner.client.post(inner.base.join("owa/auth.owa").expect("url")).form(&form), "auth.owa", false).await?;
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
            let r = send_retrying(inner.client.get(url), "auth-redirect", true).await?;
            self.remember_build(r.headers());
            if r.status().is_redirection() {
                let l = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).map(String::from);
                if let Some(l) = &l
                    && l.to_ascii_lowercase().contains("errorfe.aspx")
                {
                    return Err(classify_redirect(l));
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
        // Перед повторным входом — `logoff.owa` прежней сессии (§ 5c): при отказе CSRF она ещё
        // жива и без этого висела бы до тайм-аута, занимая место в лимите сессий на ящик. Для
        // истёкшей сессии запрос безвреден; его исход на вход не влияет.
        self.logoff_quietly("прежняя сессия перед повторным входом").await;
        self.login().await?;
        // `logoff` мьютекс повторного входа не берёт: если он пришёл, пока шёл вход (защёлка
        // проверена до сетевых запросов, а `login` безусловно ставит канарейку), новую сессию
        // никто не отслеживает и не закроет — закрываем её здесь, вызывающему `SessionExpired`.
        if self.inner.closed.load(Ordering::SeqCst) {
            *self.inner.canary.write().expect("canary lock") = None;
            self.logoff_quietly("сессия, открытая после logoff").await;
            info!("logoff во время повторного входа: новая сессия закрыта");
            return Err(TransportError::SessionExpired);
        }
        Ok(())
    }

    /// `logoff.owa` без интереса к исходу: короткий таймаут и без чтения тела ответа — запрос
    /// идёт под мьютексом повторного входа и держит разрешение семафора.
    async fn logoff_quietly(&self, context: &'static str) {
        let url = self.inner.base.join("owa/logoff.owa").expect("url");
        match self.inner.client.get(url).timeout(LOGOFF_TIMEOUT).send().await {
            Ok(r) => debug!(status = %r.status(), context, "logoff.owa"),
            Err(e) => debug!(error = %e, context, "logoff.owa не прошёл"),
        }
    }

    /// Вызов операции `service.svc` с одной попыткой повторного входа.
    pub async fn call(&self, op: &str, version: &str, body: Value) -> Result<Value> {
        self.call_with_timeout(op, version, body, None).await
    }

    /// То же с собственным таймаутом запроса вместо общего из настроек (загрузка вложений).
    pub async fn call_with_timeout(&self, op: &str, version: &str, body: Value, timeout: Option<Duration>) -> Result<Value> {
        self.call_in_zone(op, version, None, body, timeout, is_idempotent(op)).await
    }

    /// Вызов с другим `TimeZoneContext` в заголовке только этого запроса: `time_zone` — пояс
    /// Windows пользователя (запись в календарь, § 5i); `None` — пояс из настроек (UTC).
    /// `idempotent` — можно ли повторить запрос после обрыва после отправки; решает вид записи, а
    /// не пояс: записи календаря (`CreateItem`, `UpdateItem` с рассылкой участникам) не
    /// повторяются и без пояса — повтор разослал бы приглашения или обновления второй раз.
    pub async fn call_in_zone(&self, op: &str, version: &str, time_zone: Option<&str>, body: Value, timeout: Option<Duration>, idempotent: bool) -> Result<Value> {
        let env = protocol::envelope(op, version, time_zone.unwrap_or(&self.inner.config.time_zone), body);
        let payload = serde_json::to_vec(&env).map_err(|e| TransportError::Protocol { op: op.into(), status: 0, raw: e.to_string() })?;
        self.call_payload(op, payload, timeout, idempotent).await
    }

    /// Операция с OWA-специфичным конвертом (параметры на верхнем уровне рядом с `request`,
    /// без `Body`): `payload` уходит как есть, ответ — произвольный JSON (у подписок массив).
    pub async fn call_raw(&self, op: &str, payload: Value) -> Result<Value> {
        let payload = serde_json::to_vec(&payload).map_err(|e| TransportError::Protocol { op: op.into(), status: 0, raw: e.to_string() })?;
        self.call_payload(op, payload, None, is_idempotent(op)).await
    }

    /// Готовое тело запроса под разрешением семафора с одной попыткой повторного входа.
    async fn call_payload(&self, op: &str, payload: Vec<u8>, timeout: Option<Duration>, idempotent: bool) -> Result<Value> {
        let _permit = self.inner.permits.acquire().await.map_err(|_| TransportError::Cancelled)?;
        for attempt in 0..2 {
            let generation = self.inner.generation.load(Ordering::SeqCst);
            match self.call_once(op, payload.clone(), timeout, idempotent).await {
                Err(e) if e.needs_reauth() && attempt == 0 => {
                    self.relogin_once(generation).await?;
                }
                other => return other,
            }
        }
        Err(TransportError::SessionExpired)
    }

    async fn call_once(&self, op: &str, payload: Vec<u8>, timeout: Option<Duration>, idempotent: bool) -> Result<Value> {
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
        if let Some(b) = inner.build.read().expect("build lock").as_deref()
            && let Ok(v) = HeaderValue::from_str(b)
        {
            headers.insert("X-OWA-ClientBuildVersion", v);
        }
        let started = std::time::Instant::now();
        let mut req = inner.client.post(url).headers(headers).body(payload);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let r = send_retrying(req, op, idempotent).await?;
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
            if let Ok(v) = serde_json::from_str::<Value>(&text)
                && let Some(fault) = v.pointer("/Body/FaultMessage").and_then(Value::as_str)
            {
                return Err(TransportError::Protocol { op: op.into(), status: status.as_u16(), raw: truncate(fault, 300) });
            }
            return Err(TransportError::Protocol { op: op.into(), status: status.as_u16(), raw: truncate(&text, 300) });
        }
        serde_json::from_str::<Value>(&text).map_err(|e| TransportError::Protocol { op: op.into(), status: status.as_u16(), raw: format!("не JSON ({e}), {} байт: {}", text.len(), non_json_excerpt(&text)) })
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
                        info!(content_type = c.content_type.as_deref().unwrap_or("?"), bytes = c.bytes.len(), "вложения: сервер принимает GET service.svc/s/GetFileAttachment");
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
        let mut r = send_retrying(inner.client.get(url).headers(headers), "GetFileAttachment", true).await?;
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
        // `text/html` без `Content-Disposition` файла — страница ошибки; HTML-вложение приходит
        // с `attachment; filename=…`, как любой файл. Тело в ошибку и журнал не идёт целиком:
        // только длина и начало, если оно не похоже на JSON (политика журналов).
        let is_html = content_type.as_deref().map(|c| c.to_ascii_lowercase().starts_with("text/html")).unwrap_or(false);
        let disposition = r.headers().get(CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()).map(str::to_string);
        let file_name = disposition.as_deref().and_then(parse_content_disposition_filename);
        let is_file = file_name.is_some() || disposition.as_deref().is_some_and(|d| d.to_ascii_lowercase().contains("attachment"));
        if is_html && !is_file {
            let text = r.text().await.unwrap_or_default();
            return Err(TransportError::Protocol { op: "GetFileAttachment".into(), status: status.as_u16(), raw: format!("сервер вернул страницу вместо файла, {} байт: {}", text.len(), non_json_excerpt(&text)) });
        }
        if let Some(len) = r.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok())
            && len > max_bytes
        {
            return Err(TransportError::TooLarge { limit: max_bytes });
        }
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

    /// Проба GET `service.svc/s/<op>?<params>&X-OWA-CANARY=…` (канарейка и заголовком): ответ как
    /// есть, без повторного входа и разбора — для снятия формы (фото и т. п.). Тело не больше
    /// `max_bytes`, лишнее отбрасывается.
    pub async fn service_get_probe(&self, op: &str, params: &[(&str, &str)], max_bytes: usize) -> Result<ServiceGetReply> {
        let inner = &self.inner;
        let _permit = inner.permits.acquire().await.map_err(|_| TransportError::Cancelled)?;
        let canary = inner.canary.read().expect("canary lock").clone().ok_or(TransportError::SessionExpired)?;
        let mut url = inner.base.join(&format!("owa/service.svc/s/{op}")).map_err(|e| TransportError::Protocol { op: op.into(), status: 0, raw: e.to_string() })?;
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in params {
                q.append_pair(k, v);
            }
            q.append_pair("X-OWA-CANARY", &canary);
        }
        let mut headers = HeaderMap::new();
        headers.insert("X-OWA-CANARY", HeaderValue::from_str(&canary).map_err(|_| TransportError::CsrfRejected)?);
        let mut r = send_retrying(inner.client.get(url).headers(headers), op, true).await?;
        let header = |r: &reqwest::Response, name| r.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let status = r.status().as_u16();
        let content_type = header(&r, CONTENT_TYPE);
        let etag = r.headers().contains_key(reqwest::header::ETAG);
        let cache_control = header(&r, reqwest::header::CACHE_CONTROL);
        let location = header(&r, LOCATION);
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = r.chunk().await.map_err(net)? {
            let room = max_bytes.saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        Ok(ServiceGetReply { status, content_type, etag, cache_control, location, bytes })
    }

    /// Операция `GetAttachment`: `Body.ResponseMessages.Items[0].Attachments[0].Content` в base64.
    async fn get_attachment_json(&self, id: &str, max_bytes: u64) -> Result<AttachmentContent> {
        let v = self.call_unpermitted("GetAttachment", protocol::V_2013, protocol::get_attachment_body(id)).await?;
        let msg = Self::first_message("GetAttachment", &v)?;
        let att = msg.pointer("/Attachments/0").ok_or_else(|| TransportError::Protocol { op: "GetAttachment".into(), status: 200, raw: format!("нет Attachments: {}", protocol::json_shape(&msg, 2)) })?;
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
        self.call_once(op, payload, None, is_idempotent(op)).await
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
            return Err(TransportError::Protocol { op: op.into(), status: 200, raw: format!("нет ResponseMessages: {}", protocol::json_shape(&v, 2)) });
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

    /// Проверка записи в календарь: `GetItem` события с `AllProperties` и `PidLidLocation`
    /// (`protocol::get_calendar_probe_body`), из ответа — только поля времени, поясов и места
    /// (`protocol::calendar_probe_fields`) — для своего тестового события.
    pub async fn probe_calendar_item_fields(&self, id: &ItemId) -> Result<Value> {
        let body = protocol::get_calendar_probe_body(id);
        let v = self.call("GetItem", protocol::V_2017, body).await?;
        let msg = Self::first_message("GetItem", &v)?;
        Ok(protocol::calendar_probe_fields(msg.pointer("/Items/0").unwrap_or(&Value::Null)))
    }

    /// Словарь `OWA.UserOptions` (`GetUserConfiguration`): объект ищется сначала в корне ящика
    /// (`root`, как в EWS Managed API), затем в корне почтовых папок (`msgfolderroot`);
    /// `ErrorItemNotFound` в обоих — `None`.
    async fn user_options(&self) -> Result<Option<Value>> {
        for base in ["root", "msgfolderroot"] {
            let v = self.call("GetUserConfiguration", protocol::V_2013, protocol::get_user_configuration_body(base)).await?;
            match Self::first_message("GetUserConfiguration", &v) {
                Ok(m) => {
                    let dict = protocol::user_configuration_dictionary(&m);
                    let keys: Vec<&str> = dict.iter().map(|(k, _)| k.as_str()).collect();
                    debug!(base, entries = dict.len(), ?keys, shape = %protocol::json_shape(&m, 5), "GetUserConfiguration: словарь OWA.UserOptions");
                    return Ok(Some(m));
                }
                Err(TransportError::Server { code, message }) if code == "ErrorItemNotFound" => {
                    debug!(base, message, "GetUserConfiguration: объекта OWA.UserOptions нет в этой папке");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Удалить событие календаря: в «Удалённые» (`MoveToDeletedItems`) или навсегда (`HardDelete`),
    /// с рассылкой отмены участникам или без неё. Трейтовый `delete_calendar_item` — всегда в
    /// «Удалённые»; CLI после проверки удаляет навсегда.
    pub async fn delete_calendar_item_with(&self, id: &ItemId, mode: DeleteMode, send_cancellations: bool) -> Result<()> {
        let delete_type = match mode {
            DeleteMode::ToDeletedItems => "MoveToDeletedItems",
            DeleteMode::Hard => "HardDelete",
        };
        let v = self.call("DeleteItem", protocol::V_2017, protocol::delete_calendar_item_body(id, delete_type, send_cancellations)).await?;
        let m = Self::first_message("DeleteItem", &v)?;
        Self::log_shape("DeleteItem", &m);
        debug!(delete_type, send_cancellations, "событие календаря удалено");
        Ok(())
    }

    /// Отладка форм протокола: `GetItem` письма о встрече с `AllProperties`; возвращает форму
    /// элемента ответа без содержимого (`protocol::json_shape`: ключи, длины строк и массивов).
    pub async fn probe_meeting_request_shape(&self, id: &ItemId) -> Result<String> {
        let body = protocol::get_meeting_request_body(id, CalendarItemShape::AllProperties, 4096);
        let v = self.call("GetItem", protocol::V_2017, body).await?;
        let msg = Self::first_message("GetItem", &v)?;
        Ok(protocol::json_shape(msg.pointer("/Items/0").unwrap_or(&Value::Null), 3))
    }

    /// Элемент ответа `GetItem` письма о встрече: сначала `IdOnly` с явным списком полей, при
    /// отказе сервера по форме — `AllProperties`; принятая форма запоминается на сессию.
    async fn meeting_request_item(&self, id: &ItemId) -> Result<Value> {
        let known = self.inner.meeting_shape.load(Ordering::Relaxed);
        let candidates: &[(u8, CalendarItemShape)] = match known {
            CALENDAR_SHAPE_ALL => &[(CALENDAR_SHAPE_ALL, CalendarItemShape::AllProperties)],
            CALENDAR_SHAPE_EXPLICIT => &[(CALENDAR_SHAPE_EXPLICIT, CalendarItemShape::Explicit)],
            _ => &[(CALENDAR_SHAPE_EXPLICIT, CalendarItemShape::Explicit), (CALENDAR_SHAPE_ALL, CalendarItemShape::AllProperties)],
        };
        let mut last_err: Option<TransportError> = None;
        for (i, (code, shape)) in candidates.iter().enumerate() {
            let body = protocol::get_meeting_request_body(id, *shape, 4096);
            let msg = match self.call("GetItem", protocol::V_2017, body).await.and_then(|v| Self::first_message("GetItem", &v)) {
                Ok(m) => m,
                Err(e) if i + 1 < candidates.len() && (matches!(&e, TransportError::Protocol { status: 500, .. }) || matches!(&e, TransportError::Server { code, .. } if code.contains("Property") || code.contains("Shape"))) => {
                    warn!(error = %e, "GetItem письма о встрече: форма {:?} отвергнута, пробую следующую", shape);
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            };
            Self::log_shape("GetItem", &msg);
            let Some(item) = msg.pointer("/Items/0") else {
                return Err(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: format!("нет Items[0]: {}", protocol::json_shape(&msg, 2)) });
            };
            if known == CALENDAR_SHAPE_UNKNOWN {
                info!(shape = ?shape, item_type = protocol::str_of(item, "__type").unwrap_or("?"), keys = item.as_object().map(|o| o.len()).unwrap_or(0), "GetItem письма о встрече: сервер принял форму");
            }
            self.inner.meeting_shape.store(*code, Ordering::Relaxed);
            // Форма элемента без содержимого (строки — длинами): для документации протокола.
            debug!(shape = %protocol::json_shape(item, 2), "GetItem письма о встрече: форма элемента");
            return Ok(item.clone());
        }
        Err(last_err.unwrap_or(TransportError::Protocol { op: "GetItem".into(), status: 0, raw: "ни одна форма не принята".into() }))
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

    /// Одна пачка `GetItem` в именованной форме. При `want_reply_to` к форме добавляется
    /// `message:ReplyTo` (§ 5j); если сервер отвергает такую форму — HTTP 500 на запрос или
    /// отказ по свойству (`ErrorInvalidProperty*`, `*Shape*`) в каждом сообщении ответа при
    /// HTTP 200 (`call` отдаёт `Server` только после разбора сообщений, которого здесь нет) —
    /// пачка повторяется без него, и отказ запоминается на сессию. Принятая форма (хоть одно
    /// успешное сообщение) тоже запоминается, а первый ответ с ней пишет в журнал, пришёл ли ключ
    /// `ReplyTo`. Смешанные отказы по элементам (`ErrorItemNotFound`) режим не меняют.
    async fn get_item_chunk(&self, chunk: &[ItemId], shape_name: &str, want_reply_to: bool) -> Result<Value> {
        let inner = &self.inner;
        let mode = inner.reply_to_mode.load(Ordering::SeqCst);
        let with_reply_to = want_reply_to && mode != REPLY_TO_REJECTED;
        // Снимок счёта до запроса: параллельные пачки, упавшие от одного сбоя, видят одно и то же
        // значение, и удар засчитывает только первая из них (`compare_exchange` от снимка).
        let strikes_seen = inner.reply_to_strikes.load(Ordering::SeqCst);
        let body = protocol::get_item_body(chunk, shape_name, inner.config.max_body_size, with_reply_to);
        let without_reply_to = || protocol::get_item_body(chunk, shape_name, inner.config.max_body_size, false);
        match self.call("GetItem", protocol::V_2017, body).await {
            Ok(v) if with_reply_to && mode == REPLY_TO_UNKNOWN && protocol::all_property_rejections(&v) => {
                let code = protocol::response_messages(&v).first().and_then(|m| protocol::message_error(m)).map(|(c, _)| c).unwrap_or_default();
                // Принятую параллельной пачкой форму отказ не перетирает.
                if inner.reply_to_mode.compare_exchange(REPLY_TO_UNKNOWN, REPLY_TO_REJECTED, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                    warn!(code, items = chunk.len(), "GetItem: сервер отверг message:ReplyTo по каждому элементу, тела запрашиваются без Reply-To");
                }
                self.call("GetItem", protocol::V_2017, without_reply_to()).await
            }
            Ok(v) => {
                if with_reply_to {
                    // Ответ на запрос со свойством без отказа по нему: «подряд» начинается заново.
                    inner.reply_to_strikes.store(0, Ordering::SeqCst);
                }
                if with_reply_to && mode == REPLY_TO_UNKNOWN {
                    let msgs = protocol::response_messages(&v);
                    // Успешный ответ с message:ReplyTo — прямое доказательство, что сервер его принимает:
                    // ACCEPTED ставится безусловно, в том числе поверх REJECTED от параллельных отказов.
                    if msgs.iter().any(|m| protocol::message_error(m).is_none()) && inner.reply_to_mode.swap(REPLY_TO_ACCEPTED, Ordering::SeqCst) != REPLY_TO_ACCEPTED {
                        let first = msgs.first().and_then(|m| m.pointer("/Items/0").cloned());
                        let has_key = first.as_ref().map(|i| i.get("ReplyTo").is_some()).unwrap_or(false);
                        let reply_to_shape = first.as_ref().and_then(|i| i.get("ReplyTo")).map(|r| protocol::json_shape(r, 2)).unwrap_or_default();
                        info!(shape_name, has_key, reply_to_shape, "GetItem: сервер принял message:ReplyTo в AdditionalProperties поверх ShapeName");
                    }
                }
                Ok(v)
            }
            Err(e) if with_reply_to && mode == REPLY_TO_UNKNOWN && is_shape_rejection(&e) => {
                // Пачка без свойства — сразу. Отказом по форме 500 считается, только если без
                // свойства сервер отвечает, а с ним — нет, `REPLY_TO_STRIKES` раз подряд.
                let retry = self.call("GetItem", protocol::V_2017, without_reply_to()).await;
                if retry.is_err() {
                    // Без свойства тоже отказ — это сбой сервера, а не форма; наверх уходит ошибка повтора.
                    warn!(error = %e, "GetItem с message:ReplyTo: HTTP 500, повтор без свойства тоже не прошёл");
                    return retry;
                }
                let next = strikes_seen.saturating_add(1);
                match inner.reply_to_strikes.compare_exchange(strikes_seen, next, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) if next >= REPLY_TO_STRIKES => {
                        if inner.reply_to_mode.compare_exchange(REPLY_TO_UNKNOWN, REPLY_TO_REJECTED, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                            warn!(error = %e, strikes = next, "GetItem: сервер отверг message:ReplyTo в AdditionalProperties, тела запрашиваются без Reply-To");
                        }
                    }
                    Ok(_) => info!(error = %e, strikes = next, "GetItem с message:ReplyTo: HTTP 500, без свойства пачка прошла; следующая пачка снова попробует с ним"),
                    // Счёт изменился, пока шёл запрос: этот сбой уже засчитала параллельная пачка,
                    // или между ними был успешный ответ — второго удара нет.
                    Err(current) => debug!(error = %e, strikes = current, "GetItem с message:ReplyTo: HTTP 500 параллельной пачки, удар не засчитан повторно"),
                }
                retry
            }
            Err(e) => Err(e),
        }
    }

    /// Проверка формы `Reply-To` на живом сервере (только чтение): один `GetItem` в форме
    /// `ItemPart` с `message:ReplyTo`; по каждому письму — есть ли ключ `ReplyTo` в ответе, сколько
    /// в нём адресов и форма значения без содержимого (`json_shape`). Отказ сервера по форме
    /// приходит как ошибка (без повтора без свойства).
    pub async fn probe_reply_to(&self, ids: &[ItemId]) -> Result<Vec<ReplyToProbe>> {
        let body = protocol::get_item_body(ids, "ItemPart", 4096, true);
        let v = self.call("GetItem", protocol::V_2017, body).await?;
        let msgs = protocol::response_messages(&v);
        let mut out = Vec::with_capacity(msgs.len());
        for msg in msgs {
            if let Some((code, message)) = protocol::message_error(msg) {
                out.push(ReplyToProbe { error: Some(format!("{code}: {message}")), ..ReplyToProbe::default() });
                continue;
            }
            let item = msg.pointer("/Items/0");
            let reply_to = item.and_then(|i| i.get("ReplyTo"));
            let parsed = item.and_then(protocol::parse_message_full);
            out.push(ReplyToProbe {
                has_key: reply_to.is_some(),
                count: parsed.as_ref().map(|m| m.reply_to.len()).unwrap_or(0),
                differs_from_sender: parsed.as_ref().map(|m| !m.effective_reply_to().is_empty()).unwrap_or(false),
                shape: reply_to.map(|r| protocol::json_shape(r, 2)).unwrap_or_default(),
                item_keys: item.and_then(Value::as_object).map(|o| o.len()).unwrap_or(0),
                error: None,
            });
        }
        Ok(out)
    }

    /// Операция с папкой через `call`: версия схемы подбирается один раз на сессию
    /// (`V2017_08_18`, при HTTP 500 на форму — `Exchange2013`). Возвращает первое сообщение ответа.
    async fn folder_call(&self, op: &str, body: Value) -> Result<Value> {
        let known = self.inner.folder_version.load(Ordering::Relaxed);
        let candidates: &[(u8, &str)] = match known {
            FOLDER_VERSION_2013 => &[(FOLDER_VERSION_2013, protocol::V_2013)],
            FOLDER_VERSION_2017 => &[(FOLDER_VERSION_2017, protocol::V_2017)],
            _ => &[(FOLDER_VERSION_2017, protocol::V_2017), (FOLDER_VERSION_2013, protocol::V_2013)],
        };
        let mut last_err: Option<TransportError> = None;
        for (i, (code, version)) in candidates.iter().enumerate() {
            match self.call(op, version, body.clone()).await.and_then(|v| Self::first_message(op, &v)) {
                Ok(msg) => {
                    if known == FOLDER_VERSION_UNKNOWN {
                        info!(op, version, "папки: сервер принял форму запроса");
                    }
                    self.inner.folder_version.store(*code, Ordering::Relaxed);
                    Self::log_folder_shape(op, &msg);
                    return Ok(msg);
                }
                Err(e) if i + 1 < candidates.len() && matches!(e, TransportError::Protocol { status: 500, .. }) => {
                    warn!(op, version, error = %e, "папки: форма отвергнута, пробую другую версию схемы");
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or(TransportError::Protocol { op: op.into(), status: 0, raw: "ни одна форма не принята".into() }))
    }

    /// Форма ответа операции с папкой без содержимого: тип, код, ключи сообщения и папки, длины
    /// идентификатора — для документа протокола.
    fn log_folder_shape(op: &str, msg: &Value) {
        fn keys(v: Option<&Value>) -> Vec<&str> {
            v.and_then(Value::as_object).map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default()
        }
        let msg_keys = keys(Some(msg));
        let folder_keys = keys(msg.pointer("/Folders/0"));
        let id_len = msg.pointer("/Folders/0/FolderId/Id").and_then(Value::as_str).map(str::len);
        let ck_len = msg.pointer("/Folders/0/FolderId/ChangeKey").and_then(Value::as_str).map(str::len);
        debug!(op, response_type = protocol::str_of(msg, "__type").unwrap_or("?"), response_code = protocol::str_of(msg, "ResponseCode").unwrap_or("?"), ?msg_keys, ?folder_keys, ?id_len, ?ck_len, "форма ответа");
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
            return Err(TransportError::Protocol { op: op.into(), status: 200, raw: format!("нет ResponseMessages: {}", protocol::json_shape(v, 2)) });
        };
        if let Some((code, message)) = protocol::message_error(first) {
            return Err(server_error(code, message, first));
        }
        Ok((*first).clone())
    }
}

/// Итог проверки `Reply-To` по одному письму (`OwaTransport::probe_reply_to`), без содержимого.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplyToProbe {
    /// Ключ `ReplyTo` присутствует в элементе ответа (и когда адресов нет).
    pub has_key: bool,
    /// Адресов в `ReplyTo`.
    pub count: usize,
    /// Reply-To непуст и отличается от отправителя (ответ пойдёт на него).
    pub differs_from_sender: bool,
    /// Форма значения `ReplyTo` без содержимого (`json_shape`), пусто без ключа.
    pub shape: String,
    /// Ключей у элемента ответа (сколько полей дала форма).
    pub item_keys: usize,
    /// Отказ сервера по этому письму (`код: текст`).
    pub error: Option<String>,
}

/// Отказ сервера по форме запроса до разбора ответа: HTTP 500 на десериализации. Отказ по
/// свойству в сообщениях ответа (HTTP 200) `call` ошибкой не отдаёт — его проверяет
/// `protocol::all_property_rejections` по успешному ответу.
fn is_shape_rejection(e: &TransportError) -> bool {
    matches!(e, TransportError::Protocol { status: 500, .. })
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
        TransportError::Connect(_) => "Connect",
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

/// Справочник организации (§ 5l): ответы как есть (`*_value`) — для CLI, который печатает их
/// форму без значений, — и их разбор (`*_result`), которым пользуются и методы трейта.
impl OwaTransport {
    /// Операция справочника: схема `V2017_08_18`, при отказе по форме (HTTP 500) — `Exchange2013`.
    async fn directory_call(&self, op: &str, body: Value) -> Result<Value> {
        match self.call(op, protocol::V_2017, body.clone()).await {
            Err(e) if is_shape_rejection(&e) => {
                debug!(op, kind = error_kind(&e), "справочник: схема V2017_08_18 отвергнута, повтор с Exchange2013");
                self.call(op, protocol::V_2013, body).await
            }
            other => other,
        }
    }

    /// Ответ `FindPeople` по справочнику целиком. Сначала с дополнительными свойствами записи
    /// (`protocol::PERSONA_EXTRA_FIELDS`: должность, офис, телефоны — в форме `Default` их нет);
    /// отказ по ним (`ErrorInvalidArgument` или код свойства в `Body`, HTTP 500) — повтор формой
    /// `Default`, и отказ запоминается до конца жизни транспорта.
    pub async fn find_people_value(&self, query: &str, max: usize) -> Result<Value> {
        let plain = protocol::find_people_body(query, max);
        if self.inner.persona_extra_rejected.load(Ordering::Relaxed) {
            return self.directory_call("FindPeople", plain).await;
        }
        match self.directory_call("FindPeople", protocol::find_people_body_with(query, max, protocol::PERSONA_EXTRA_FIELDS)).await {
            Ok(v) => match protocol::body_error(&v) {
                Some((code, _)) if code == "ErrorInvalidArgument" || protocol::is_property_code(&code) => {
                    debug!(code, "FindPeople: дополнительные свойства записи отвергнуты, повтор формой Default");
                }
                _ => return Ok(v),
            },
            Err(e) if is_shape_rejection(&e) => debug!(kind = error_kind(&e), "FindPeople: форма с дополнительными свойствами отвергнута, повтор формой Default"),
            Err(e) => return Err(e),
        }
        let v = self.directory_call("FindPeople", plain).await?;
        if protocol::body_error(&v).is_none() {
            self.inner.persona_extra_rejected.store(true, Ordering::Relaxed);
        }
        Ok(v)
    }

    /// Операция персон (`CreatePersona`, `UpdatePersona`, `GetPersona`) в конверте браузерной OWA
    /// (`protocol::persona_envelope`, пояс пользователя из настроек транспорта). Ответ — как прислал
    /// сервер: у записи — сама персона, у `GetPersona` — `{ ResponseCode, Persona }`.
    pub async fn persona_call(&self, op: &str, body: Value) -> Result<Value> {
        let payload = protocol::persona_envelope(op, &self.inner.config.time_zone, body);
        self.call_raw(op, payload).await
    }

    /// `DeletePersonas` по `PersonaId`; ответ — `true`.
    pub async fn delete_personas(&self, persona_ids: &[String]) -> Result<Value> {
        self.call_raw("DeletePersonas", protocol::delete_personas_request(persona_ids)).await
    }

    /// `GetPresence` по SIP-адресам (`{ "sipUris": [...] }` в теле POST, как у браузерной OWA): ответ —
    /// число-подтверждение, сами статусы приходят потоком уведомлений (`InstantMessageNotification`,
    /// § 5s). Входа в IM (`InstantMessageSignIn`) клиент не делает.
    pub async fn get_presence(&self, sip_uris: &[String]) -> Result<Value> {
        self.call_raw("GetPresence", serde_json::json!({ "sipUris": sip_uris })).await
    }

    /// Ответ `ResolveNames` целиком.
    pub async fn resolve_names_value(&self, query: &str, full_contact: bool) -> Result<Value> {
        self.directory_call("ResolveNames", protocol::resolve_names_body(query, full_contact)).await
    }

    /// Ответ `ExpandDL` целиком.
    pub async fn expand_dl_value(&self, email: &str) -> Result<Value> {
        self.directory_call("ExpandDL", protocol::expand_dl_body(email)).await
    }

    /// Записи из ответа `FindPeople` (не больше `max`). Отказ уровня `Body` или первого сообщения
    /// `ResponseMessages` — `Server`; успешный ответ без `People` — пустой список.
    pub fn find_people_result(v: &Value, max: usize) -> Result<Vec<DirectoryPerson>> {
        if let Some((code, message)) = protocol::body_error(v) {
            return Err(server_error(code, message, v.get("Body").unwrap_or(v)));
        }
        if let Some(first) = protocol::response_messages(v).first()
            && let Some((code, message)) = protocol::message_error(first)
        {
            return Err(server_error(code, message, first));
        }
        let Some(arr) = protocol::find_people_array(v) else {
            debug!(shape = %protocol::json_shape(v, 3), "FindPeople: в ответе нет People");
            return Ok(Vec::new());
        };
        let mut people: Vec<DirectoryPerson> = arr.iter().filter_map(protocol::parse_persona).collect();
        people.truncate(max);
        let keys: Vec<String> = protocol::key_union(arr).into_iter().map(|(k, n)| format!("{k}:{n}")).collect();
        let total = v.pointer("/Body/TotalNumberOfPeopleInView").and_then(Value::as_i64);
        debug!(entries = arr.len(), parsed = people.len(), ?total, ?keys, "FindPeople: записи справочника");
        Ok(people)
    }

    /// Записи из ответа `ResolveNames` (`protocol::parse_resolve_names` по первому сообщению):
    /// ничего не найдено — пусто, несколько совпадений — список, иной отказ — `Server`.
    pub fn resolve_names_result(v: &Value) -> Result<Vec<DirectoryPerson>> {
        let msgs = protocol::response_messages(v);
        let Some(msg) = msgs.first() else {
            if let Some((code, message)) = protocol::body_error(v) {
                return Err(server_error(code, message, v.get("Body").unwrap_or(v)));
            }
            return Err(TransportError::Protocol { op: "ResolveNames".into(), status: 200, raw: format!("нет ResponseMessages: {}", protocol::json_shape(v, 3)) });
        };
        let people = protocol::parse_resolve_names(msg).map_err(|(code, message)| server_error(code, message, msg))?;
        let keys: Vec<String> = protocol::resolutions_array(msg).map(|a| protocol::key_union(a).into_iter().map(|(k, n)| format!("{k}:{n}")).collect()).unwrap_or_default();
        debug!(code = protocol::str_of(msg, "ResponseCode").unwrap_or("?"), parsed = people.len(), ?keys, "ResolveNames: записи справочника");
        Ok(people)
    }

    /// Как `resolve_names_result`, но с фото (`Contact.Photo`).
    pub fn resolve_names_full_result(v: &Value) -> Result<Vec<ResolvedPerson>> {
        let msgs = protocol::response_messages(v);
        let Some(msg) = msgs.first() else {
            if let Some((code, message)) = protocol::body_error(v) {
                return Err(server_error(code, message, v.get("Body").unwrap_or(v)));
            }
            return Err(TransportError::Protocol { op: "ResolveNames".into(), status: 200, raw: format!("нет ResponseMessages: {}", protocol::json_shape(v, 3)) });
        };
        let found = protocol::parse_resolve_names_full(msg).map_err(|(code, message)| server_error(code, message, msg))?;
        let with_photo = found.iter().filter(|(_, p)| p.is_some()).count();
        let photo_bytes: usize = found.iter().filter_map(|(_, p)| p.as_ref().map(Vec::len)).sum();
        debug!(code = protocol::str_of(msg, "ResponseCode").unwrap_or("?"), parsed = found.len(), with_photo, photo_bytes, "ResolveNames: записи справочника с фото");
        Ok(found.into_iter().map(|(person, photo)| ResolvedPerson { person, photo }).collect())
    }

    /// Участники из ответа `ExpandDL`; отказ сервера (не список, нет доступа) — `Server`.
    pub fn expand_dl_result(v: &Value) -> Result<Vec<DirectoryPerson>> {
        let msg = Self::first_message("ExpandDL", v)?;
        let people = protocol::parse_expand_dl(&msg);
        let total = msg.pointer("/DLExpansion/TotalItemsInView").and_then(Value::as_i64);
        debug!(parsed = people.len(), ?total, "ExpandDL: участники списка");
        Ok(people)
    }
}

impl OwaTransport {
    /// Контакты и группы по идентификаторам: `GetItem` формой `AllProperties` (§ 5q) пачками по 25.
    /// Элемент, по которому сервер отказал (`ErrorItemNotFound` — удалён между вызовами) или не
    /// вернул ответ, — `None` на своей позиции. В журнал — только имена ключей первой записи.
    pub async fn get_contact_items(&self, ids: &[ItemId]) -> Result<Vec<Option<ContactItem>>> {
        const BATCH: usize = 25;
        let mut out = Vec::with_capacity(ids.len());
        let mut logged = false;
        for chunk in ids.chunks(BATCH) {
            let started = Instant::now();
            let v = self.call("GetItem", protocol::V_2017, protocol::get_contact_items_body(chunk)).await?;
            let msgs = protocol::response_messages(&v);
            let mut failed = 0usize;
            for pos in 0..chunk.len() {
                let msg = msgs.get(pos).copied();
                if let Some(m) = msg
                    && let Some((code, _)) = protocol::message_error(m)
                {
                    failed += 1;
                    debug!(code, "GetItem контакта: отказ по элементу");
                    out.push(None);
                    continue;
                }
                let item = msg.and_then(|m| m.get("Items").and_then(Value::as_array)).and_then(|a| a.first());
                if !logged && let Some(i) = item {
                    debug!(form = %protocol::contact_items_keys([i]), "GetItem контакта: форма AllProperties");
                    logged = true;
                }
                out.push(item.and_then(protocol::parse_contact_item));
            }
            debug!(requested = chunk.len(), got = msgs.len(), failed, ms = started.elapsed().as_millis() as u64, "GetItem контактов");
        }
        Ok(out)
    }
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

    async fn sync_contact_items(&self, folder: &FolderRef, state: Option<&SyncState>, max_changes: u32) -> Result<ContactChanges> {
        let max = max_changes.clamp(1, 512);
        let started = Instant::now();
        let v = self.call("SyncFolderItems", protocol::V_2013, protocol::sync_contact_items_body(folder, state, max)).await?;
        let msg = Self::first_message("SyncFolderItems", &v)?;
        let (state, mut changes, includes_last) = protocol::parse_contact_changes(&msg, max);
        if !changes.is_empty() {
            // Форма ответа — только имена ключей (§ 5q снята с живого сервера по этой записи).
            debug!(form = %protocol::contact_form_keys(&msg), changes = changes.len(), includes_last, ms = started.elapsed().as_millis() as u64, "SyncFolderItems контактов: форма AllProperties (базовые поля)");
        }
        // Полные поля — `GetItem` по созданным и изменённым: отказ всего вызова — отказ страницы
        // (состояние не продвигается, повтор в следующий прогон); отказ по одному элементу
        // оставляет базовые поля из `SyncFolderItems`.
        let ids: Vec<ItemId> = changes
            .iter()
            .filter_map(|c| match c {
                ContactChange::Create(c) | ContactChange::Update(c) => Some(c.item_id.clone()),
                ContactChange::Delete(_) => None,
            })
            .collect();
        if !ids.is_empty() {
            let mut full = self.get_contact_items(&ids).await?.into_iter();
            for ch in &mut changes {
                let (ContactChange::Create(c) | ContactChange::Update(c)) = ch else { continue };
                if let Some(Some(f)) = full.next() {
                    *c = f;
                }
            }
        }
        Ok(ContactChanges { state, changes, includes_last })
    }

    async fn find_recent_items(&self, folder: &FolderRef, max: u32) -> Result<Vec<MessageMeta>> {
        let max = max.clamp(1, 1000);
        let started = Instant::now();
        let v = self.call("FindItem", protocol::V_2017, protocol::find_recent_items_body(folder, max, 0)).await?;
        let msg = Self::first_message("FindItem", &v)?;
        let found = protocol::parse_find_items(&msg, max);
        debug!(requested = max, items = found.items.len(), raw = found.raw_count, total_in_view = ?found.total_in_view, includes_last = found.includes_last, ms = started.elapsed().as_millis() as u64, "FindItem: последние письма папки");
        Ok(found.items)
    }

    async fn get_items(&self, ids: &[ItemId], shape: ItemFetchShape) -> Result<Vec<ItemResult>> {
        let shape_name = match shape {
            ItemFetchShape::Meta => "MailListItem",
            ItemFetchShape::WithUniqueBody => "ItemPart",
            ItemFetchShape::WithFullBody => "ItemNormalizedBody",
        };
        // Reply-To нужен только формам с получателями; у списка (`MailListItem`) его не просим.
        let want_reply_to = shape != ItemFetchShape::Meta;
        let mut out: Vec<ItemResult> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(self.inner.config.get_item_batch.max(1)) {
            let v = self.get_item_chunk(chunk, shape_name, want_reply_to).await?;
            let msgs = protocol::response_messages(&v);
            if msgs.is_empty() {
                return Err(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: protocol::json_shape(&v, 2) });
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
            if all_failed && let Some(t) = throttled {
                return Err(t);
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
            if let Some(f) = msg.pointer("/Folders/0")
                && let Some(id) = f.get("FolderId").and_then(protocol::parse_folder_id)
            {
                out.push(FolderCounts { id, total: protocol::i64_of(f, "TotalCount").unwrap_or(0), unread: protocol::i64_of(f, "UnreadCount").unwrap_or(0) });
            }
        }
        Ok(out)
    }

    async fn get_attachment(&self, attachment_id: &str, max_bytes: u64) -> Result<AttachmentContent> {
        self.get_attachment_with_relogin(attachment_id, max_bytes).await
    }

    async fn refresh_item_ids(&self, ids: &[ItemId]) -> Result<Vec<WriteResult>> {
        // Та же обвязка, что у операций записи: пачки, повторный вход, результат по позициям.
        let out = self.write_items("GetItem", ids, protocol::get_item_ids_body).await?;
        let full = out.iter().filter(|r| r.is_ok() && r.id().has_full_change_key()).count();
        debug!(requested = ids.len(), ok = out.iter().filter(|r| r.is_ok()).count(), full_keys = full, "идентификаторы перечитаны");
        Ok(out)
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

    async fn create_folder(&self, parent: &FolderRef, display_name: &str) -> Result<FolderId> {
        let m = self.folder_call("CreateFolder", protocol::create_folder_body(parent, display_name)).await?;
        let id = protocol::parse_written_folder_id(&m).ok_or_else(|| TransportError::Protocol { op: "CreateFolder".into(), status: 200, raw: format!("нет Folders[0].FolderId: {}", protocol::json_shape(&m, 2)) })?;
        debug!(name_len = display_name.chars().count(), id_len = id.id.len(), ck_len = id.change_key.as_deref().map(str::len), "папка создана");
        Ok(id)
    }

    async fn rename_folder(&self, id: &FolderId, display_name: &str) -> Result<FolderId> {
        let m = self.folder_call("UpdateFolder", protocol::rename_folder_body(id, display_name)).await?;
        let new_id = protocol::parse_written_folder_id(&m);
        debug!(name_len = display_name.chars().count(), id_in_response = new_id.is_some(), same_id = new_id.as_ref().map(|n| n.id == id.id), "папка переименована");
        Ok(new_id.unwrap_or_else(|| id.clone()))
    }

    async fn move_folder(&self, id: &FolderId, parent: &FolderRef) -> Result<FolderId> {
        let m = self.folder_call("MoveFolder", protocol::move_folder_body(id, parent)).await?;
        let new_id = protocol::parse_written_folder_id(&m);
        debug!(id_in_response = new_id.is_some(), same_id = new_id.as_ref().map(|n| n.id == id.id), "папка перенесена");
        Ok(new_id.unwrap_or_else(|| id.clone()))
    }

    async fn delete_folder(&self, id: &FolderId, permanent: bool) -> Result<()> {
        let delete_type = if permanent { "HardDelete" } else { "MoveToDeletedItems" };
        self.folder_call("DeleteFolder", protocol::delete_folder_body(id, delete_type)).await?;
        debug!(delete_type, "папка удалена");
        Ok(())
    }

    async fn create_draft(&self, kind: &ComposeKind, msg: &OutgoingMessage) -> Result<ItemId> {
        let v = self.call("CreateItem", protocol::V_2017, protocol::create_item_body(kind, msg)).await?;
        let m = Self::first_message("CreateItem", &v)?;
        Self::log_shape("CreateItem", &m);
        protocol::parse_created_item_id(&m).ok_or_else(|| TransportError::Protocol { op: "CreateItem".into(), status: 200, raw: format!("нет Items[0].ItemId: {}", protocol::json_shape(&m, 2)) })
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
        let new_id = protocol::written_item_id(&m);
        debug!(
            subject_len = msg.subject.chars().count(),
            body_chars = msg.body_html.chars().count(),
            to = msg.to.len(),
            cc = msg.cc.len(),
            bcc = msg.bcc.len(),
            id_in_response = new_id.is_some(),
            same_id = new_id.as_ref().map(|n| n.id == item.id),
            ck_len = new_id.as_ref().and_then(|n| n.change_key.as_deref()).map(str::len),
            "черновик обновлён"
        );
        Ok(new_id.unwrap_or_else(|| item.clone()))
    }

    /// `DeleteAttachment` одним запросом по всем идентификаторам: ответ — сообщение на каждое
    /// вложение; отказ хотя бы по одному — `Server` с кодом первого отказа (`ErrorServerBusy` —
    /// `Throttled`); корень берётся из последнего успешного сообщения.
    async fn delete_attachments(&self, ids: &[String]) -> Result<ItemId> {
        if ids.is_empty() {
            return Err(TransportError::Protocol { op: "DeleteAttachment".into(), status: 0, raw: "список вложений для удаления пуст".into() });
        }
        let v = self.call("DeleteAttachment", protocol::V_2017, protocol::delete_attachment_body(ids)).await?;
        let msgs = protocol::response_messages(&v);
        if msgs.is_empty() {
            if let Some(code) = v.pointer("/Body/ResponseCode").and_then(Value::as_str).filter(|c| *c != "NoError") {
                return Err(TransportError::Server { code: code.to_string(), message: v.pointer("/Body/MessageText").and_then(Value::as_str).unwrap_or("").to_string() });
            }
            return Err(TransportError::Protocol { op: "DeleteAttachment".into(), status: 200, raw: format!("нет ResponseMessages: {}", protocol::json_shape(&v, 2)) });
        }
        let mut root: Option<ItemId> = None;
        let mut first_err: Option<TransportError> = None;
        let mut failed = 0usize;
        for (i, m) in msgs.iter().enumerate() {
            if let Some((code, message)) = protocol::message_error(m) {
                failed += 1;
                if first_err.is_none() {
                    first_err = Some(server_error(code, message, m));
                }
                continue;
            }
            if i == 0 {
                // Форма ответа без содержимого: ключи сообщения и форма RootItemId (строки — длинами).
                debug!(op = "DeleteAttachment", response_type = protocol::str_of(m, "__type").unwrap_or("?"), shape = %protocol::json_shape(m, 2), "форма ответа");
            }
            if let Some(r) = protocol::parse_delete_attachment_root(m) {
                root = Some(r);
            }
        }
        debug!(requested = ids.len(), messages = msgs.len(), failed, root_in_response = root.is_some(), ck_len = root.as_ref().and_then(|r| r.change_key.as_deref()).map(str::len), "вложения черновика удалены");
        if let Some(e) = first_err {
            return Err(e);
        }
        root.ok_or_else(|| TransportError::Protocol { op: "DeleteAttachment".into(), status: 200, raw: format!("нет RootItemId в ответе: {}", protocol::json_shape(msgs[0], 2)) })
    }

    /// `GetItem` черновика: сначала `AllProperties` с `BodyType: HTML`, при отказе сервера по форме
    /// (HTTP 500, ошибка по свойству) или без члена тела в ответе — `ItemNormalizedBody`; принятая
    /// форма запоминается на сессию.
    async fn get_draft(&self, id: &ItemId) -> Result<DraftContent> {
        let known = self.inner.draft_shape.load(Ordering::Relaxed);
        let candidates: &[(u8, DraftShape)] = match known {
            DRAFT_SHAPE_ALL => &[(DRAFT_SHAPE_ALL, DraftShape::AllProperties)],
            DRAFT_SHAPE_NORMALIZED => &[(DRAFT_SHAPE_NORMALIZED, DraftShape::NormalizedBody)],
            _ => &[(DRAFT_SHAPE_ALL, DraftShape::AllProperties), (DRAFT_SHAPE_NORMALIZED, DraftShape::NormalizedBody)],
        };
        let mut last_err: Option<TransportError> = None;
        for (i, (code, shape)) in candidates.iter().enumerate() {
            let more = i + 1 < candidates.len();
            let body = protocol::get_draft_body(id, *shape, self.inner.config.max_body_size);
            let msg = match self.call("GetItem", protocol::V_2017, body).await.and_then(|v| Self::first_message("GetItem", &v)) {
                Ok(m) => m,
                Err(e) if more && (is_shape_rejection(&e) || matches!(&e, TransportError::Server { code, .. } if protocol::is_property_code(code))) => {
                    warn!(error = %e, "GetItem черновика: форма {:?} отвергнута, пробую следующую", shape);
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            };
            Self::log_shape("GetItem", &msg);
            let Some(item) = msg.pointer("/Items/0") else {
                return Err(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: format!("нет Items[0]: {}", protocol::json_shape(&msg, 2)) });
            };
            let body_key = protocol::body_key_of(item);
            if body_key.is_none() && more {
                let keys: Vec<&str> = item.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
                warn!(?keys, "GetItem черновика: форма {:?} не отдала тело, пробую следующую", shape);
                last_err = Some(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: format!("нет тела у черновика, ключи {keys:?}") });
                continue;
            }
            let Some(draft) = protocol::parse_draft_content(item) else {
                return Err(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: format!("нет ItemId у элемента: {}", protocol::json_shape(item, 1)) });
            };
            if known == DRAFT_SHAPE_UNKNOWN {
                info!(shape = ?shape, keys = item.as_object().map(|o| o.len()).unwrap_or(0), body_key = body_key.unwrap_or("нет"), "GetItem черновика: сервер принял форму");
            }
            self.inner.draft_shape.store(*code, Ordering::Relaxed);
            debug!(
                subject_len = draft.subject.chars().count(),
                body_chars = draft.body_html.chars().count(),
                body_type = item.get(body_key.unwrap_or("Body")).and_then(|b| protocol::str_of(b, "BodyType")).unwrap_or("?"),
                truncated = item.get(body_key.unwrap_or("Body")).map(|b| protocol::bool_of(b, "IsTruncated")),
                to = draft.to.len(),
                cc = draft.cc.len(),
                bcc = draft.bcc.len(),
                attachments = draft.attachments.len(),
                inline = draft.attachments.iter().filter(|a| a.is_inline).count(),
                is_draft = protocol::bool_of(item, "IsDraft"),
                attachment_shape = %item.pointer("/Attachments/0").map(|a| protocol::json_shape(a, 2)).unwrap_or_default(),
                ck_len = draft.id.change_key.as_deref().map(str::len),
                "GetItem черновика"
            );
            return Ok(draft);
        }
        Err(last_err.unwrap_or(TransportError::Protocol { op: "GetItem".into(), status: 0, raw: "ни одна форма не принята".into() }))
    }

    /// Подпись из словаря `OWA.UserOptions` (`user_options`); нет объекта или подписи — `None`.
    async fn user_signature(&self) -> Result<Option<Signature>> {
        Ok(self.user_options().await?.as_ref().and_then(protocol::parse_user_signature))
    }

    /// Пояс пользователя из словаря `OWA.UserOptions` (ключ `timezone`); нет объекта или ключа — `None`.
    async fn user_time_zone(&self) -> Result<Option<String>> {
        let tz = self.user_options().await?.as_ref().and_then(protocol::parse_user_time_zone);
        debug!(known = tz.is_some(), len = tz.as_deref().map(str::len), "GetUserConfiguration: пояс пользователя");
        Ok(tz)
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
                return Err(TransportError::Protocol { op: "GetItem".into(), status: 200, raw: format!("нет Items[0]: {}", protocol::json_shape(&msg, 2)) });
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

    /// Запрос идёт в контексте пояса пользователя (`TimeZoneContext` этого запроса), у события на
    /// весь день границы — местные полуночи дат без зоны; без пояса — контекст UTC и моменты с `Z`.
    async fn create_calendar_item(&self, item: &NewCalendarItem, time_zone: Option<&str>) -> Result<ItemId> {
        let body = protocol::create_calendar_item_body(item, time_zone.is_some() && item.is_all_day);
        let v = self.call_in_zone("CreateItem", protocol::V_2017, time_zone, body, None, false).await?;
        let m = Self::first_message("CreateItem", &v)?;
        Self::log_shape("CreateItem", &m);
        let id = protocol::parse_created_item_id(&m).ok_or_else(|| TransportError::Protocol { op: "CreateItem".into(), status: 200, raw: format!("нет Items[0].ItemId: {}", protocol::json_shape(&m, 2)) })?;
        debug!(attendees = item.required.len() + item.optional.len(), all_day = item.is_all_day, reminder = ?item.reminder_minutes, zone = time_zone.is_some(), id_len = id.id.len(), ck_len = id.change_key.as_deref().map(str::len), "событие календаря создано");
        Ok(id)
    }

    async fn update_calendar_item(&self, id: &ItemId, changes: &CalendarItemChanges, send_updates: bool, time_zone: Option<&str>) -> Result<ItemId> {
        if changes.is_empty() {
            debug!("событие календаря: изменений нет, запрос не нужен");
            return Ok(id.clone());
        }
        let body = protocol::update_calendar_item_body(id, changes, send_updates, time_zone.is_some() && changes.is_all_day == Some(true));
        // С рассылкой (`SendToAllAndSaveCopy`) повтор после обрыва разослал бы обновление участникам
        // второй раз; без неё (`SendToNone`, `AlwaysOverwrite`) — повторяется, как запись писем.
        let v = self.call_in_zone("UpdateItem", protocol::V_2017, time_zone, body, None, !send_updates).await?;
        let m = Self::first_message("UpdateItem", &v)?;
        Self::log_shape("UpdateItem", &m);
        Ok(protocol::written_item_id(&m).unwrap_or_else(|| id.clone()))
    }

    async fn delete_calendar_item(&self, id: &ItemId, send_cancellations: bool) -> Result<()> {
        self.delete_calendar_item_with(id, DeleteMode::ToDeletedItems, send_cancellations).await
    }

    async fn respond_to_meeting(&self, reference: &ItemId, response: MeetingResponse, send_response: bool, body_html: Option<&str>) -> Result<()> {
        let v = self.call("CreateItem", protocol::V_2017, protocol::respond_to_meeting_body(reference, response, send_response, body_html)).await?;
        let m = Self::first_message("CreateItem", &v)?;
        Self::log_shape("CreateItem", &m);
        debug!(response = response.key(), send_response, "ответ на приглашение принят сервером");
        Ok(())
    }

    async fn get_meeting_request(&self, id: &ItemId) -> Result<MeetingRequestInfo> {
        let item = self.meeting_request_item(id).await?;
        let info = protocol::parse_meeting_request_info(&item);
        debug!(kind = info.kind.key(), has_start = info.start.is_some(), my_response = info.my_response.as_str(), out_of_date = info.is_out_of_date, associated = info.associated_calendar_item.is_some(), sender = ?info.response_type_of_sender, "GetItem письма о встрече");
        Ok(info)
    }

    async fn find_people(&self, query: &str, max: usize) -> Result<Vec<DirectoryPerson>> {
        let query = query.trim();
        if query.is_empty() || max == 0 {
            return Ok(Vec::new());
        }
        let started = Instant::now();
        let v = self.find_people_value(query, max).await?;
        let people = Self::find_people_result(&v, max)?;
        debug!(query_len = query.chars().count(), found = people.len(), ms = started.elapsed().as_millis(), "FindPeople");
        Ok(people)
    }

    async fn resolve_names_full(&self, query: &str) -> Result<Vec<ResolvedPerson>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let started = Instant::now();
        let v = self.resolve_names_value(query, true).await?;
        let out = Self::resolve_names_full_result(&v)?;
        debug!(chars = query.chars().count(), found = out.len(), ms = started.elapsed().as_millis() as u64, "ResolveNames с фото");
        Ok(out)
    }

    async fn resolve_names(&self, query: &str, full_contact: bool) -> Result<Vec<DirectoryPerson>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let started = Instant::now();
        let v = self.resolve_names_value(query, full_contact).await?;
        let people = Self::resolve_names_result(&v)?;
        debug!(query_len = query.chars().count(), full_contact, found = people.len(), ms = started.elapsed().as_millis(), "ResolveNames");
        Ok(people)
    }

    async fn expand_dl(&self, email: &str) -> Result<Vec<DirectoryPerson>> {
        let email = email.trim();
        if email.is_empty() {
            return Ok(Vec::new());
        }
        let started = Instant::now();
        let v = self.expand_dl_value(email).await?;
        let people = Self::expand_dl_result(&v)?;
        debug!(members = people.len(), ms = started.elapsed().as_millis(), "ExpandDL");
        Ok(people)
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
        assert!(is_idempotent("GetItem") && is_idempotent("UpdateItem") && is_idempotent("SyncFolderItems") && is_idempotent("FindPeople"));
        assert!(!is_idempotent("CreateItem") && !is_idempotent("SendItem") && !is_idempotent("MoveItem") && !is_idempotent("SubscribeToNotification"));
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

    /// Разбор ответов справочника: успех, отказ уровня `Body` и сообщения, «ничего не найдено»,
    /// «несколько совпадений», ответ без `ResponseMessages`.
    #[test]
    fn directory_results_map_server_errors() {
        use serde_json::json;
        let ok = json!({"Body": {"ResponseClass": "Success", "ResponseCode": "NoError", "People": [
            {"DisplayName": "А", "EmailAddress": {"EmailAddress": "a@x.ru"}}, {"DisplayName": "Б", "EmailAddress": {"EmailAddress": "b@x.ru"}}]}});
        assert_eq!(OwaTransport::find_people_result(&ok, 1).unwrap().len(), 1, "не больше max");
        assert!(OwaTransport::find_people_result(&json!({"Body": {"ResponseClass": "Success", "ResponseCode": "NoError"}}), 5).unwrap().is_empty(), "без People — пусто");
        let rejected = json!({"Body": {"ResponseClass": "Error", "ResponseCode": "ErrorInvalidRequest", "MessageText": "x"}});
        assert!(matches!(OwaTransport::find_people_result(&rejected, 5), Err(TransportError::Server { code, .. }) if code == "ErrorInvalidRequest"));
        let in_message = json!({"Body": {"ResponseMessages": {"Items": [{"ResponseClass": "Error", "ResponseCode": "ErrorServerBusy"}]}}});
        assert!(matches!(OwaTransport::find_people_result(&in_message, 5), Err(TransportError::Throttled { .. })));

        let none = json!({"Body": {"ResponseMessages": {"Items": [{"ResponseClass": "Error", "ResponseCode": "ErrorNameResolutionNoResults"}]}}});
        assert!(OwaTransport::resolve_names_result(&none).unwrap().is_empty());
        let many = json!({"Body": {"ResponseMessages": {"Items": [{"ResponseClass": "Warning", "ResponseCode": "ErrorNameResolutionMultipleResults",
            "ResolutionSet": {"Resolutions": [{"Mailbox": {"Name": "А", "EmailAddress": "a@x.ru"}}, {"Mailbox": {"Name": "Б", "EmailAddress": "b@x.ru"}}]}}]}}});
        assert_eq!(OwaTransport::resolve_names_result(&many).unwrap().len(), 2);
        let denied = json!({"Body": {"ResponseMessages": {"Items": [{"ResponseClass": "Error", "ResponseCode": "ErrorAccessDenied"}]}}});
        assert!(matches!(OwaTransport::resolve_names_result(&denied), Err(TransportError::Server { code, .. }) if code == "ErrorAccessDenied"));
        assert!(matches!(OwaTransport::resolve_names_result(&json!({"Body": {}})), Err(TransportError::Protocol { .. })));

        let dl = json!({"Body": {"ResponseMessages": {"Items": [{"ResponseClass": "Success", "ResponseCode": "NoError", "DLExpansion": {"Mailbox": [{"Name": "А", "EmailAddress": "a@x.ru", "MailboxType": "Mailbox"}]}}]}}});
        assert_eq!(OwaTransport::expand_dl_result(&dl).unwrap().len(), 1);
        let not_dl = json!({"Body": {"ResponseMessages": {"Items": [{"ResponseClass": "Error", "ResponseCode": "ErrorNameResolutionNoResults"}]}}});
        assert!(matches!(OwaTransport::expand_dl_result(&not_dl), Err(TransportError::Server { .. })));
    }

    #[test]
    fn server_busy_maps_to_throttled() {
        let msg: Value = serde_json::json!({"ResponseClass":"Error","ResponseCode":"ErrorServerBusy","MessageXml":{"Value":{"Name":"BackOffMilliseconds","Value":"2500"}}});
        match server_error("ErrorServerBusy".into(), String::new(), &msg) {
            TransportError::Throttled { retry_after } => assert_eq!(retry_after, Some(Duration::from_millis(2500))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn non_json_excerpt_hides_json_like_content() {
        assert_eq!(non_json_excerpt("{\"Body\":{\"ResponseMessages\":{\"Items\":[{\"Subject\":\"секрет"), "(похоже на JSON, не показывается)");
        assert_eq!(non_json_excerpt("  [1,2"), "(похоже на JSON, не показывается)");
        assert_eq!(non_json_excerpt("<html>вход</html>"), "<html>вход</html>");
        assert!(non_json_excerpt(&"x".repeat(500)).chars().count() <= 161);
    }

    #[tokio::test]
    async fn network_error_text_hides_query() {
        // Закрытый порт: reqwest вернёт ошибку соединения с адресом запроса в тексте.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/owa/service.svc/s/GetFileAttachment?id=x&X-OWA-CANARY=SECRET-CANARY");
        let e = reqwest::Client::builder().no_proxy().build().unwrap().get(&url).send().await.unwrap_err();
        let text = net(e).to_string();
        assert!(!text.contains("SECRET-CANARY") && !text.contains("X-OWA-CANARY"), "{text}");
        assert!(text.contains("127.0.0.1") && text.contains("GetFileAttachment"), "хост и путь для диагностики остаются: {text}");
    }

    /// Обрыв после отправки, затем повторы не соединяются: первая попытка могла дойти до сервера —
    /// ошибка `Network` (исход неизвестен), а не `Connect` («запрос не ушёл»).
    #[tokio::test]
    async fn connect_failure_after_sent_attempt_is_network() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Один запрос прочитан, соединение закрыто без ответа; слушатель закрывается вместе с ним.
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let (mut buf, mut chunk) = (Vec::new(), [0u8; 1024]);
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let r = send_retrying(client.get(format!("http://127.0.0.1:{port}/owa/service.svc")), "UpdateItem", true).await;
        server.join().unwrap();
        assert!(matches!(r, Err(TransportError::Network(_))), "{r:?}");
    }
}
