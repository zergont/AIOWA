//! ИИ-слой AIOWA, шаг И1 (`docs/ai-layer-design.md` § 3): шлюз моделей.
//!
//! Все обращения к языковым моделям идут через [`Gateway`]: политика приватности → запрос к
//! провайдеру (Anthropic или OpenAI-совместимый сервер) → поток ответа → запись в журнал
//! ([`CallRecord`], без содержимого). Функции ИИ («Совет», разбор запроса) не знают, куда уходит
//! запрос. Ключи сюда передаются на время вызова и нигде не сохраняются.

pub mod advice;
pub mod anthropic;
pub mod digest;
pub mod mailtext;
pub mod nlsearch;
pub mod openai;
pub mod profile;
pub mod sse;

pub use profile::{Place, Profile, ProfileError, ProviderKind};

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sse::SseParser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Режим приватности (CONCEPT § 5.6): какие адресаты разрешены. Проверяется в шлюзе перед
/// каждым обращением, а не в окне.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    /// A: любые модели — облако и свои серверы (MVP, решение автора 06.11.2026).
    #[default]
    Any,
    /// B: только этот компьютер и свои серверы в сети.
    OwnOnly,
}

impl PrivacyMode {
    pub fn allows(self, place: Place) -> bool {
        match self {
            PrivacyMode::Any => true,
            PrivacyMode::OwnOnly => place != Place::Cloud,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into() }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, text: text.into() }
    }
}

/// Глубина рассуждения (только Anthropic, `output_config.effort`); у OpenAI-совместимых не передаётся.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

/// Схема JSON, по которой модель должна ответить (`docs/ai-layer-design.md` § 2, принцип 7).
#[derive(Debug, Clone, PartialEq)]
pub struct JsonSchema {
    /// Имя схемы для провайдера: латиница, цифры, подчёркивание.
    pub name: &'static str,
    pub schema: Value,
}

/// Запрос к модели.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Функция для журнала: «проверка», «совет», «разбор», «кратко».
    pub feature: &'static str,
    pub system: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    pub effort: Option<Effort>,
    /// Ответ — JSON по схеме: у Anthropic — `output_config.format`, у OpenAI-совместимых —
    /// `response_format`. Сервер, который этого не понимает, отвечает отказом — см.
    /// [`Gateway::chat_json`].
    pub json_schema: Option<JsonSchema>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Модель закончила сама.
    End,
    /// Упёрлась в предел длины ответа.
    MaxTokens,
    /// Отказалась отвечать (фильтры провайдера).
    Refusal,
    Other(String),
}

impl StopReason {
    fn code(&self) -> &str {
        match self {
            StopReason::End => "ok",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
            StopReason::Other(_) => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatOutcome {
    pub text: String,
    pub stop: StopReason,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Модель, которую назвал сервер.
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    pub id: String,
    /// Имя для людей (Anthropic `display_name`).
    pub name: Option<String>,
    /// Окно контекста, токенов, если сервер его называет.
    pub context: Option<u64>,
}

/// Отказ шлюза. Тексты — для окна; в журнал приложения идёт только [`AiError::code`].
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("{0}")]
    Profile(String),
    #[error("Режим «Только свои модели»: облачные подключения выключены")]
    Policy,
    #[error("Не выбрана модель подключения")]
    NoModel,
    #[error("{0}")]
    Network(String),
    #[error("Сервер модели не ответил вовремя")]
    Timeout,
    #[error("Ключ не подошёл: сервер модели отказал во входе")]
    Auth,
    #[error("Не найдено: {0}")]
    NotFound(String),
    #[error("Сервер модели перегружен или превышен лимит запросов{}", retry_after_s.map(|s| format!(", повторите через {s} с")).unwrap_or_default())]
    RateLimited { retry_after_s: Option<u64> },
    #[error("Сервер модели отклонил запрос: {0}")]
    BadRequest(String),
    #[error("Ошибка сервера модели{}{}", if *status > 0 { format!(" (HTTP {status})") } else { String::new() }, if message.is_empty() { String::new() } else { format!(": {message}") })]
    Server { status: u16, message: String },
    #[error("Непонятный ответ сервера модели: {0}")]
    Protocol(String),
    #[error("Отменено")]
    Cancelled,
}

impl AiError {
    /// Короткий код для журналов (без текста провайдера: в нём бывают куски запроса).
    pub fn code(&self) -> &'static str {
        match self {
            AiError::Profile(_) => "profile",
            AiError::Policy => "policy",
            AiError::NoModel => "no_model",
            AiError::Network(_) => "network",
            AiError::Timeout => "timeout",
            AiError::Auth => "auth",
            AiError::NotFound(_) => "not_found",
            AiError::RateLimited { .. } => "rate_limited",
            AiError::BadRequest(_) => "bad_request",
            AiError::Server { .. } => "server",
            AiError::Protocol(_) => "protocol",
            AiError::Cancelled => "cancelled",
        }
    }
}

impl From<ProfileError> for AiError {
    fn from(e: ProfileError) -> Self {
        AiError::Profile(e.0)
    }
}

/// Запись журнала обращений: что, куда, сколько — без содержимого (CONCEPT § 5.6, журнал с
/// первого дня). Пишет её вызывающий (`Gateway::new(journal)`), шлюз только собирает.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    pub at_ms: i64,
    pub feature: String,
    pub profile_id: String,
    pub host: String,
    pub place: Place,
    pub model: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub duration_ms: u64,
    /// `ok`, `max_tokens`, `refusal`, `other` или код ошибки ([`AiError::code`]).
    pub outcome: String,
    /// SHA-256 тела запроса: по нему можно сверить, что ушло, не храня самого текста.
    pub request_sha256: String,
}

/// Отмена идущего обращения (смена профиля, закрытие панели, новая просьба поверх старой).
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<CancelInner>);

#[derive(Debug, Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    /// Ждать отмены.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Время на соединение с сервером модели.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Самое длинное тело отказа, которое читается для текста ошибки.
const ERROR_BODY_MAX: usize = 64 * 1024;
/// Страниц списка моделей Anthropic самое большее (по 100).
const MODEL_PAGES_MAX: usize = 5;

type Journal = dyn Fn(CallRecord) + Send + Sync;

/// Шлюз моделей: один HTTP-клиент на все подключения (системные сертификаты и прокси) и журнал.
pub struct Gateway {
    client: Client,
    journal: Arc<Journal>,
}

impl Gateway {
    /// `user_agent` — «AIOWA/версия»; `journal` получает запись о каждом обращении к модели.
    pub fn new(user_agent: &str, journal: Arc<Journal>) -> Result<Self, AiError> {
        let client = Client::builder()
            .user_agent(user_agent)
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| AiError::Network(format!("HTTP-клиент не создан: {e}")))?;
        Ok(Self { client, journal })
    }

    /// Список моделей подключения (проверка подключения, выбор модели в настройках).
    pub async fn list_models(&self, profile: &Profile, key: Option<&SecretString>, mode: PrivacyMode, cancel: &Cancel) -> Result<Vec<ModelInfo>, AiError> {
        if !mode.allows(profile.place()) {
            return Err(AiError::Policy);
        }
        let base = profile.api_base()?;
        let timeout = Duration::from_secs(profile.timeout_s.into());
        match profile.kind {
            ProviderKind::Anthropic => {
                let mut out = Vec::new();
                let mut after: Option<String> = None;
                for _ in 0..MODEL_PAGES_MAX {
                    let mut url = endpoint(&base, "/v1/models");
                    url.query_pairs_mut().append_pair("limit", "100");
                    if let Some(a) = &after {
                        url.query_pairs_mut().append_pair("after_id", a);
                    }
                    let req = self.client.get(url).headers(headers(profile.kind, key)?);
                    let v = self.get_json(req, timeout, cancel, profile.kind).await?;
                    let (page, next) = anthropic::parse_models(&v);
                    out.extend(page);
                    match next {
                        Some(n) => after = Some(n),
                        None => break,
                    }
                }
                Ok(out)
            }
            ProviderKind::OpenAi => {
                let req = self.client.get(endpoint(&base, "/models")).headers(headers(profile.kind, key)?);
                let v = self.get_json(req, timeout, cancel, profile.kind).await?;
                Ok(openai::parse_models(&v))
            }
        }
    }

    /// Обращение к модели с потоком ответа: куски текста — в `on_text`, итог — в результате.
    /// Каждое обращение, дошедшее до сервера (успех или отказ), попадает в журнал.
    pub async fn chat(&self, profile: &Profile, key: Option<&SecretString>, mode: PrivacyMode, req: &ChatRequest, on_text: &mut (dyn FnMut(&str) + Send), cancel: &Cancel) -> Result<ChatOutcome, AiError> {
        let place = profile.place();
        if !mode.allows(place) {
            return Err(AiError::Policy);
        }
        let model = profile.model.trim();
        if model.is_empty() {
            return Err(AiError::NoModel);
        }
        let base = profile.api_base()?;
        let (url, body) = match profile.kind {
            ProviderKind::Anthropic => (endpoint(&base, "/v1/messages"), anthropic::request_body(model, req)),
            ProviderKind::OpenAi => {
                let cloud = base.host_str().is_some_and(|h| h == "api.openai.com");
                let extra = profile.extra_fields()?;
                (endpoint(&base, "/chat/completions"), openai::request_body(model, req, cloud, extra.as_ref()))
            }
        };
        let bytes = serde_json::to_vec(&body).map_err(|e| AiError::Protocol(e.to_string()))?;
        let sha = hex(&Sha256::digest(&bytes));
        let started = Instant::now();
        let at_ms = now_ms();
        let result = self.stream(profile, key, url, bytes, on_text, cancel).await;
        let outcome = match &result {
            Ok(o) => o.stop.code().to_string(),
            Err(e) => e.code().to_string(),
        };
        let (input_tokens, output_tokens) = result.as_ref().map(|o| (o.input_tokens, o.output_tokens)).unwrap_or((None, None));
        let record = CallRecord {
            at_ms,
            feature: req.feature.to_string(),
            profile_id: profile.id.clone(),
            host: profile.host(),
            place,
            model: model.to_string(),
            input_tokens,
            output_tokens,
            duration_ms: started.elapsed().as_millis() as u64,
            outcome,
            request_sha256: sha,
        };
        info!(feature = req.feature, place = ?place, outcome = %record.outcome, ms = record.duration_ms, input = ?input_tokens, output = ?output_tokens, "обращение к модели");
        (self.journal)(record);
        result
    }

    /// Обращение к модели за JSON-объектом по схеме запроса (`req.json_schema`). Сервер, который
    /// схему в запросе не понимает (отказ «неверный запрос»), спрашивается ещё раз без неё: схема
    /// должна быть описана и в промпте. Ответ — объект из текста модели; проверка полей — за
    /// вызывающим. Оба обращения попадают в журнал.
    pub async fn chat_json(&self, profile: &Profile, key: Option<&SecretString>, mode: PrivacyMode, req: &ChatRequest, cancel: &Cancel) -> Result<(Value, ChatOutcome), AiError> {
        let mut sink = |_: &str| {};
        let out = match self.chat(profile, key, mode, req, &mut sink, cancel).await {
            Err(AiError::BadRequest(_)) if req.json_schema.is_some() => {
                debug!("сервер модели не принял схему ответа в запросе: повтор без неё");
                let plain = ChatRequest { json_schema: None, ..req.clone() };
                self.chat(profile, key, mode, &plain, &mut sink, cancel).await?
            }
            other => other?,
        };
        let value = nlsearch::extract_json(&out.text).filter(Value::is_object).ok_or_else(|| AiError::Protocol("модель ответила не JSON-объектом".into()))?;
        Ok((value, out))
    }

    async fn stream(&self, profile: &Profile, key: Option<&SecretString>, url: url::Url, body: Vec<u8>, on_text: &mut (dyn FnMut(&str) + Send), cancel: &Cancel) -> Result<ChatOutcome, AiError> {
        let timeout = Duration::from_secs(profile.timeout_s.into());
        let mut h = headers(profile.kind, key)?;
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let send = self.client.post(url).headers(h).body(body).send();
        let mut resp = race(cancel, timeout, send).await?;
        if !resp.status().is_success() {
            return Err(status_error(resp, profile.kind, cancel).await);
        }
        // Сервер ответил целиком, без потока (так делают некоторые прокси): текст — одним куском.
        let content_type = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        if content_type.starts_with("application/json") {
            let v = read_json(resp, timeout, cancel).await?;
            let out = match profile.kind {
                ProviderKind::Anthropic => anthropic::parse_message(&v)?,
                ProviderKind::OpenAi => openai::parse_completion(&v)?,
            };
            if !out.text.is_empty() {
                on_text(&out.text);
            }
            return Ok(out);
        }
        let mut parser = SseParser::new();
        let mut proto = match profile.kind {
            ProviderKind::Anthropic => Proto::Anthropic(anthropic::Stream::default()),
            ProviderKind::OpenAi => Proto::OpenAi(openai::Stream::default()),
        };
        'read: loop {
            let chunk = race(cancel, timeout, resp.chunk()).await?;
            let Some(chunk) = chunk else { break };
            for ev in parser.push(&chunk).map_err(AiError::Protocol)? {
                if proto.on_event(&ev, on_text)? {
                    break 'read;
                }
            }
        }
        for ev in parser.finish() {
            if proto.on_event(&ev, on_text)? {
                break;
            }
        }
        proto.finish()
    }

    async fn get_json(&self, req: reqwest::RequestBuilder, timeout: Duration, cancel: &Cancel, kind: ProviderKind) -> Result<Value, AiError> {
        let resp = race(cancel, timeout, req.send()).await?;
        if !resp.status().is_success() {
            return Err(status_error(resp, kind, cancel).await);
        }
        read_json(resp, timeout, cancel).await
    }
}

/// Тело ответа как JSON (не больше 16 МБ).
async fn read_json(mut resp: Response, timeout: Duration, cancel: &Cancel) -> Result<Value, AiError> {
    let mut body = Vec::new();
    while let Some(chunk) = race(cancel, timeout, resp.chunk()).await? {
        body.extend_from_slice(&chunk);
        if body.len() > 16 * 1024 * 1024 {
            return Err(AiError::Protocol("слишком большой ответ".into()));
        }
    }
    serde_json::from_slice(&body).map_err(|_| AiError::Protocol("ответ не JSON — проверьте адрес подключения".into()))
}

/// Разбор потока ответа по протоколу подключения.
enum Proto {
    Anthropic(anthropic::Stream),
    OpenAi(openai::Stream),
}

impl Proto {
    fn on_event(&mut self, ev: &sse::SseEvent, on_text: &mut (dyn FnMut(&str) + Send)) -> Result<bool, AiError> {
        match self {
            Proto::Anthropic(s) => s.on_event(ev, on_text),
            Proto::OpenAi(s) => s.on_event(ev, on_text),
        }
    }

    fn finish(self) -> Result<ChatOutcome, AiError> {
        match self {
            Proto::Anthropic(s) => s.finish(),
            Proto::OpenAi(s) => s.finish(),
        }
    }
}

/// Ждать будущее не дольше `timeout` и до отмены; ошибки reqwest — в [`AiError`].
async fn race<T>(cancel: &Cancel, timeout: Duration, fut: impl std::future::Future<Output = reqwest::Result<T>>) -> Result<T, AiError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(AiError::Cancelled),
        r = tokio::time::timeout(timeout, fut) => match r {
            Err(_) => Err(AiError::Timeout),
            Ok(Err(e)) => Err(net_error(&e)),
            Ok(Ok(v)) => Ok(v),
        },
    }
}

fn net_error(e: &reqwest::Error) -> AiError {
    if e.is_timeout() {
        return AiError::Timeout;
    }
    // Текст ошибки reqwest содержит адрес; ключей в адресе нет (они в заголовках).
    debug!(error = %e, "сеть: обращение к модели не удалось");
    if e.is_connect() {
        return AiError::Network("Нет связи с сервером модели: проверьте адрес, сеть и что сервер запущен".into());
    }
    AiError::Network("Связь с сервером модели прервалась".into())
}

/// Отказ по коду ответа: текст ошибки провайдера — в окно (в журнал — только код).
async fn status_error(resp: Response, kind: ProviderKind, cancel: &Cancel) -> AiError {
    let status = resp.status();
    let retry_after_s = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let body = read_limited(resp, cancel).await;
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| match kind {
            ProviderKind::Anthropic => anthropic::error_message(&v),
            ProviderKind::OpenAi => openai::error_message(&v),
        })
        .unwrap_or_default();
    warn!(status = status.as_u16(), "сервер модели отказал");
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => AiError::Auth,
        StatusCode::NOT_FOUND => AiError::NotFound(if message.is_empty() { "адрес или модель — проверьте подключение".into() } else { message }),
        StatusCode::TOO_MANY_REQUESTS => AiError::RateLimited { retry_after_s },
        s if s.as_u16() == 529 => AiError::RateLimited { retry_after_s },
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY | StatusCode::PAYLOAD_TOO_LARGE => AiError::BadRequest(if message.is_empty() { format!("HTTP {}", status.as_u16()) } else { message }),
        s => AiError::Server { status: s.as_u16(), message },
    }
}

async fn read_limited(mut resp: Response, cancel: &Cancel) -> Vec<u8> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = race(cancel, Duration::from_secs(10), resp.chunk()).await {
        body.extend_from_slice(&chunk);
        if body.len() > ERROR_BODY_MAX {
            break;
        }
    }
    body
}

fn headers(kind: ProviderKind, key: Option<&SecretString>) -> Result<HeaderMap, AiError> {
    let mut h = HeaderMap::new();
    let key = key.map(|k| k.expose_secret().trim()).filter(|k| !k.is_empty());
    match kind {
        ProviderKind::Anthropic => {
            let key = key.ok_or_else(|| AiError::Profile("Для подключения к Anthropic нужен ключ".into()))?;
            let mut v = HeaderValue::from_str(key).map_err(|_| AiError::Profile("В ключе недопустимые символы".into()))?;
            v.set_sensitive(true);
            h.insert("x-api-key", v);
            h.insert("anthropic-version", HeaderValue::from_static(anthropic::API_VERSION));
        }
        ProviderKind::OpenAi => {
            // Своим серверам ключ часто не нужен.
            if let Some(key) = key {
                let mut v = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| AiError::Profile("В ключе недопустимые символы".into()))?;
                v.set_sensitive(true);
                h.insert(AUTHORIZATION, v);
            }
        }
    }
    Ok(h)
}

/// `base` + путь: у базы без пути (`https://api.anthropic.com/`) косая не удваивается.
fn endpoint(base: &url::Url, suffix: &str) -> url::Url {
    let mut u = base.clone();
    let path = format!("{}{}", base.path().trim_end_matches('/'), suffix);
    u.set_path(&path);
    u
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_and_endpoints() {
        assert!(PrivacyMode::Any.allows(Place::Cloud));
        assert!(!PrivacyMode::OwnOnly.allows(Place::Cloud));
        assert!(PrivacyMode::OwnOnly.allows(Place::OwnServer) && PrivacyMode::OwnOnly.allows(Place::Local));
        let a = url::Url::parse("https://api.anthropic.com/").unwrap();
        assert_eq!(endpoint(&a, "/v1/messages").as_str(), "https://api.anthropic.com/v1/messages");
        let o = url::Url::parse("http://gpu:8000/v1").unwrap();
        assert_eq!(endpoint(&o, "/chat/completions").as_str(), "http://gpu:8000/v1/chat/completions");
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }

    #[test]
    fn error_texts_are_russian_and_codes_stable() {
        assert_eq!(AiError::RateLimited { retry_after_s: Some(30) }.to_string(), "Сервер модели перегружен или превышен лимит запросов, повторите через 30 с");
        assert_eq!(AiError::Server { status: 502, message: String::new() }.to_string(), "Ошибка сервера модели (HTTP 502)");
        assert_eq!(AiError::Policy.code(), "policy");
        assert!(headers(ProviderKind::Anthropic, None).is_err(), "Anthropic без ключа");
        assert!(headers(ProviderKind::OpenAi, None).unwrap().get(AUTHORIZATION).is_none());
    }

    #[tokio::test]
    async fn cancel_wakes_waiter() {
        let c = Cancel::new();
        let c2 = c.clone();
        let t = tokio::spawn(async move { c2.cancelled().await });
        tokio::task::yield_now().await;
        c.cancel();
        tokio::time::timeout(Duration::from_secs(2), t).await.unwrap().unwrap();
        // Отменённый заранее — сразу.
        tokio::time::timeout(Duration::from_millis(100), c.cancelled()).await.unwrap();
    }
}
