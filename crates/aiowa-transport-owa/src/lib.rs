//! Транспорт поверх внутреннего JSON API Outlook Web App (Exchange 2016/2019).
//!
//! Факты, на которых всё держится (см. `docs/protocol-owa-json.md`):
//! вход через форму `/owa/auth.owa`; cookie `X-OWA-CANARY` дублируется заголовком;
//! `__type` первым ключом каждого объекта; системный TLS; одна сессия на процесс и
//! явный `logoff.owa`; истечение сессии = 440/401/редирект на `logon.aspx`.

pub mod protocol;

use aiowa_core::{FolderChanges, FolderRef, ItemChanges, ItemId, SyncState};
use aiowa_transport::{async_trait, Credentials, FolderCounts, ItemFetchShape, ItemResult, MailTransport, Result, ServerInfo, TransportError};
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, HeaderValue, LOCATION};
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode, Url};
use secrecy::ExposeSecret;
use serde_json::Value;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
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
    permits: Semaphore,
}

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
        });
        let t = Self { inner };
        t.login().await?;
        Ok(t)
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
        info!(host = %inner.creds.host, "вход в OWA выполнен");
        Ok(())
    }

    /// Повторный вход один раз на поколение сессии: параллельные вызовы не плодят входов.
    async fn relogin_once(&self, seen_generation: u64) -> Result<()> {
        let _guard = self.inner.relogin.lock().await;
        if self.inner.generation.load(Ordering::SeqCst) != seen_generation {
            return Ok(());
        }
        warn!("сессия OWA истекла, повторный вход");
        self.login().await
    }

    /// Вызов операции `service.svc` с одной попыткой повторного входа.
    pub async fn call(&self, op: &str, version: &str, body: Value) -> Result<Value> {
        let _permit = self.inner.permits.acquire().await.map_err(|_| TransportError::Cancelled)?;
        let env = protocol::envelope(op, version, &self.inner.config.time_zone, body);
        let payload = serde_json::to_vec(&env).map_err(|e| TransportError::Protocol { op: op.into(), status: 0, raw: e.to_string() })?;
        for attempt in 0..2 {
            let generation = self.inner.generation.load(Ordering::SeqCst);
            match self.call_once(op, payload.clone()).await {
                Err(e) if e.needs_reauth() && attempt == 0 => {
                    self.relogin_once(generation).await?;
                }
                other => return other,
            }
        }
        Err(TransportError::SessionExpired)
    }

    async fn call_once(&self, op: &str, payload: Vec<u8>) -> Result<Value> {
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
        let r = inner.client.post(url).headers(headers).body(payload).send().await.map_err(net)?;
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

    async fn logoff(&self) -> Result<()> {
        let url = self.inner.base.join("owa/logoff.owa").expect("url");
        let r = self.inner.client.get(url).send().await.map_err(net)?;
        debug!(status = %r.status(), "logoff.owa");
        *self.inner.canary.write().expect("canary lock") = None;
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
    fn server_busy_maps_to_throttled() {
        let msg: Value = serde_json::json!({"ResponseClass":"Error","ResponseCode":"ErrorServerBusy","MessageXml":{"Value":{"Name":"BackOffMilliseconds","Value":"2500"}}});
        match server_error("ErrorServerBusy".into(), String::new(), &msg) {
            TransportError::Throttled { retry_after } => assert_eq!(retry_after, Some(Duration::from_millis(2500))),
            other => panic!("{other:?}"),
        }
    }
}
