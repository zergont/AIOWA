//! owa-probe: спайк протокола OWA JSON для Exchange 2019.
//!
//! Что умеет: вход через форму OWA, загрузка данных сессии, список папок,
//! список бесед папки, проверки транспорта и замер жизни сессии.
//! Пароль хранится в диспетчере учётных данных Windows и никогда не печатается.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::blocking::{Client, Response};
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, HeaderValue, LOCATION, SET_COOKIE};
use reqwest::redirect::Policy;
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

const KEYRING_SERVICE: &str = "AIOWA";

#[derive(Parser)]
#[command(name = "owa-probe", version, about = "Спайк протокола OWA JSON: вход, папки, беседы, проверки транспорта")]
struct Cli {
    /// Хост OWA, например mail.example.com
    #[arg(long, env = "AIOWA_HOST")]
    host: String,
    /// Имя пользователя: DOMAIN\user или user@domain
    #[arg(long, env = "AIOWA_USER")]
    user: String,
    /// Версия клиента в заголовке X-OWA-ClientBuildVersion
    #[arg(long, default_value = "15.2.1748.39")]
    build: String,
    /// Часовой пояс Windows для заголовка запроса
    #[arg(long, default_value = "Russian Standard Time")]
    tz: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Запросить пароль в консоли и сохранить в диспетчере учётных данных Windows
    SetPassword,
    /// Удалить сохранённый пароль
    ForgetPassword,
    /// Войти и показать, какие cookies поставил сервер (без значений)
    Login,
    /// Войти и показать дерево папок с числом элементов
    Folders,
    /// Войти и показать последние беседы папки
    Inbox {
        /// Сколько бесед показать
        #[arg(long, default_value_t = 20)]
        count: usize,
        /// Известная папка: inbox, sentitems, drafts, deleteditems, junkemail, archive
        #[arg(long, default_value = "inbox")]
        folder: String,
    },
    /// Проверки транспорта: тело vs X-OWA-UrlPostData, минимальные заголовки, SyncFolderItems, GetConversationItems
    Probe,
    /// Жизнь сессии: вход, запрос, пауза N минут без активности, повторный запрос
    Soak {
        #[arg(long, default_value_t = 20)]
        idle_min: u64,
    },
    /// Снять формы ответов: SyncFolderItems (структура Changes) и варианты GetItem
    Shapes,
}

// ---------------------------------------------------------------------------
// Пароль

fn keyring_entry(user: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, user).map_err(|e| anyhow!("keyring: {e}"))
}

fn load_password(user: &str) -> Result<String> {
    keyring_entry(user)?
        .get_password()
        .map_err(|e| anyhow!("пароль не найден в диспетчере учётных данных ({e}); выполните: owa-probe set-password"))
}

// ---------------------------------------------------------------------------
// Сессия

struct Session {
    client: Client,
    jar: Arc<Jar>,
    base: Url,
    canary: String,
    build: String,
    tz: String,
    action_id: Cell<i64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    /// JSON в теле POST, без EP в query
    Body,
    /// JSON в заголовке X-OWA-UrlPostData, тело пустое, EP=1
    UrlPostData,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Headers {
    /// Полный набор, как у браузерной OWA
    Full,
    /// Только Action, X-OWA-CANARY, Content-Type
    Minimal,
    /// Без X-OWA-CANARY: ожидаем отказ
    NoCanary,
}

#[derive(Debug)]
enum CallError {
    SessionExpired(StatusCode, Option<String>),
    Http(StatusCode, String),
    Other(anyhow::Error),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::SessionExpired(s, loc) => write!(f, "сессия истекла: HTTP {s}{}", loc.as_deref().map(|l| format!(" → {l}")).unwrap_or_default()),
            CallError::Http(s, body) => write!(f, "HTTP {s}: {}", body.chars().take(200).collect::<String>()),
            CallError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<anyhow::Error> for CallError {
    fn from(e: anyhow::Error) -> Self {
        CallError::Other(e)
    }
}
impl From<reqwest::Error> for CallError {
    fn from(e: reqwest::Error) -> Self {
        CallError::Other(e.into())
    }
}

fn now_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn correlation_id() -> String {
    format!("{:032X}_{}", now_millis() ^ 0x5A5A_5A5A_5A5A_5A5A_u128, now_millis())
}

/// Печатает имена и атрибуты Set-Cookie без значений.
fn report_set_cookies(label: &str, r: &Response) {
    let status = r.status();
    let loc = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    println!("{label} → {status}{}", if loc.is_empty() { String::new() } else { format!("  Location: {loc}") });
    for v in r.headers().get_all(SET_COOKIE) {
        if let Ok(s) = v.to_str() {
            let mut parts = s.split(';');
            let name = parts.next().unwrap_or("").split('=').next().unwrap_or("").trim();
            let attrs: Vec<&str> = parts.map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
            println!("    Set-Cookie: {name}  [{}]", attrs.join("; "));
        }
    }
}

fn cookie_names(jar: &Jar, url: &Url) -> Vec<String> {
    jar.cookies(url)
        .and_then(|h| h.to_str().ok().map(String::from))
        .map(|s| s.split(';').filter_map(|c| c.trim().split('=').next().map(String::from)).collect())
        .unwrap_or_default()
}

fn cookie_value(jar: &Jar, url: &Url, name: &str) -> Option<String> {
    let header = jar.cookies(url)?;
    let s = header.to_str().ok()?;
    s.split(';').map(|c| c.trim()).find_map(|c| {
        let (k, v) = c.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

fn login(cli: &Cli, password: &str, verbose: bool) -> Result<Session> {
    let jar = Arc::new(Jar::default());
    let client = Client::builder()
        .cookie_provider(jar.clone())
        .redirect(Policy::none())
        .user_agent("AIOWA-probe/0.1 (+spike)")
        .timeout(Duration::from_secs(60))
        .build()?;
    let base = Url::parse(&format!("https://{}/", cli.host))?;
    let owa = base.join("owa")?;

    let logon = base.join("owa/auth/logon.aspx")?;
    let t0 = Instant::now();
    let r = client.get(logon).query(&[("url", owa.as_str()), ("reason", "0")]).send()?;
    if verbose {
        report_set_cookies("GET /owa/auth/logon.aspx", &r);
    }
    let _ = r.text();

    let form = [
        ("destination", owa.as_str()),
        ("flags", "4"),
        ("forcedownlevel", "0"),
        ("username", cli.user.as_str()),
        ("password", password),
        ("passwordText", ""),
        ("isUtf8", "1"),
    ];
    let r = client.post(base.join("owa/auth.owa")?).form(&form).send()?;
    if verbose {
        report_set_cookies("POST /owa/auth.owa", &r);
    }
    let status = r.status();
    let location = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).map(String::from);
    if let Some(loc) = &location {
        if loc.contains("logon.aspx") {
            let reason = loc.split("reason=").nth(1).unwrap_or("?").chars().take_while(|c| c.is_ascii_digit()).collect::<String>();
            bail!("вход отклонён: редирект на страницу входа, reason={reason} (2 = неверные учётные данные)");
        }
    }
    if !status.is_redirection() {
        let text = r.text().unwrap_or_default();
        if text.contains("logon.aspx") || text.contains("name=\"password\"") {
            bail!("вход отклонён: сервер вернул страницу входа");
        }
        bail!("неожиданный ответ auth.owa: HTTP {status}");
    }

    // Идём по редиректам вручную, собирая cookies.
    let mut next = location;
    for _ in 0..5 {
        let Some(loc) = next.take() else { break };
        let url = base.join(&loc)?;
        let r = client.get(url.clone()).send()?;
        if verbose {
            report_set_cookies(&format!("GET {}", url.path()), &r);
        }
        if r.status().is_redirection() {
            next = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).map(String::from);
        } else {
            let _ = r.text();
        }
    }

    let canary = cookie_value(&jar, &owa, "X-OWA-CANARY").ok_or_else(|| anyhow!("после входа нет cookie X-OWA-CANARY: сессия не установлена"))?;
    println!("Вход выполнен за {} мс; cookies: {}", t0.elapsed().as_millis(), cookie_names(&jar, &owa).join(", "));

    Ok(Session { client, jar, base, canary, build: cli.build.clone(), tz: cli.tz.clone(), action_id: Cell::new(-1) })
}

impl Session {
    fn request_header(&self, version: &str) -> Value {
        json!({
            "__type": "JsonRequestHeaders:#Exchange",
            "RequestServerVersion": version,
            "TimeZoneContext": {
                "__type": "TimeZoneContext:#Exchange",
                "TimeZoneDefinition": { "__type": "TimeZoneDefinitionType:#Exchange", "Id": self.tz }
            }
        })
    }

    fn envelope(&self, op: &str, version: &str, body: Value) -> Value {
        json!({
            "__type": format!("{op}JsonRequest:#Exchange"),
            "Header": self.request_header(version),
            "Body": body,
        })
    }

    fn check_auth(r: &Response) -> Result<(), CallError> {
        let s = r.status();
        if s == StatusCode::from_u16(440).unwrap_or(StatusCode::UNAUTHORIZED) || s == StatusCode::UNAUTHORIZED {
            return Err(CallError::SessionExpired(s, None));
        }
        if s.is_redirection() {
            let loc = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).map(String::from);
            return Err(CallError::SessionExpired(s, loc));
        }
        Ok(())
    }

    fn session_data(&self) -> Result<Value, CallError> {
        let corr = correlation_id();
        let r = self
            .client
            .post(self.base.join("owa/sessiondata.ashx?appcacheclient=0").map_err(anyhow::Error::from)?)
            .header("X-OWA-CorrelationId", &corr)
            .header("client-request-id", &corr)
            .header("X-Requested-With", "XMLHttpRequest")
            .body("")
            .send()?;
        Self::check_auth(&r)?;
        if !r.status().is_success() {
            let s = r.status();
            return Err(CallError::Http(s, r.text().unwrap_or_default()));
        }
        let text = r.text()?;
        serde_json::from_str(&text).map_err(|e| CallError::Other(anyhow!("sessiondata не JSON ({e}): {}", text.chars().take(200).collect::<String>())))
    }

    /// Вызов операции service.svc. Возвращает (статус, JSON ответа, длительность).
    fn call(&self, op: &str, payload: &Value, transport: Transport, headers: Headers) -> Result<(StatusCode, Value, Duration), CallError> {
        let id = self.action_id.get();
        self.action_id.set(id - 1);
        let mut url = self.base.join("owa/service.svc").map_err(anyhow::Error::from)?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("action", op);
            if transport == Transport::UrlPostData {
                q.append_pair("EP", "1");
            }
            q.append_pair("ID", &id.to_string());
            q.append_pair("AC", "1");
        }
        let json_text = serde_json::to_string(payload).map_err(anyhow::Error::from)?;
        let corr = correlation_id();

        let mut h = HeaderMap::new();
        h.insert("Action", HeaderValue::from_str(op).map_err(anyhow::Error::from)?);
        h.insert("Content-Type", HeaderValue::from_static("application/json; charset=utf-8"));
        if headers != Headers::NoCanary {
            h.insert("X-OWA-CANARY", HeaderValue::from_str(&self.canary).map_err(anyhow::Error::from)?);
        }
        if headers == Headers::Full || headers == Headers::NoCanary {
            h.insert("X-OWA-ActionName", HeaderValue::from_str(&format!("{op}Action")).map_err(anyhow::Error::from)?);
            h.insert("X-OWA-ActionId", HeaderValue::from_str(&id.to_string()).map_err(anyhow::Error::from)?);
            h.insert("X-OWA-Attempt", HeaderValue::from_static("1"));
            h.insert("X-OWA-ClientBuildVersion", HeaderValue::from_str(&self.build).map_err(anyhow::Error::from)?);
            h.insert("X-OWA-CorrelationId", HeaderValue::from_str(&corr).map_err(anyhow::Error::from)?);
            h.insert("client-request-id", HeaderValue::from_str(&corr).map_err(anyhow::Error::from)?);
            h.insert("X-Requested-With", HeaderValue::from_static("XMLHttpRequest"));
        }
        let mut req = self.client.post(url).headers(h);
        req = match transport {
            Transport::Body => req.body(json_text),
            Transport::UrlPostData => {
                let encoded = utf8_percent_encode(&json_text, NON_ALPHANUMERIC).to_string();
                req.header("X-OWA-UrlPostData", HeaderValue::from_str(&encoded).map_err(anyhow::Error::from)?).body("")
            }
        };
        let t0 = Instant::now();
        let r = req.send()?;
        let took = t0.elapsed();
        Self::check_auth(&r)?;
        let status = r.status();
        let text = r.text()?;
        if !status.is_success() {
            return Err(CallError::Http(status, text));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| CallError::Other(anyhow!("ответ {op} не JSON ({e}): {}", text.chars().take(200).collect::<String>())))?;
        Ok((status, v, took))
    }

    // --- готовые операции ---------------------------------------------------

    fn find_conversation(&self, folder: &str, count: usize, shape_name: Option<&str>, transport: Transport, headers: Headers) -> Result<(Value, Duration), CallError> {
        let mut body = json!({
            "__type": "FindConversationRequest:#Exchange",
            "ParentFolderId": { "__type": "TargetFolderId:#Exchange", "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": folder } },
            "ConversationShape": { "__type": "ConversationResponseShape:#Exchange", "BaseShape": "IdOnly" },
            "Paging": { "__type": "IndexedPageView:#Exchange", "BasePoint": "Beginning", "Offset": 0, "MaxEntriesReturned": count },
            "SortOrder": [ { "__type": "SortResults:#Exchange", "Order": "Descending", "Path": { "__type": "PropertyUri:#Exchange", "FieldURI": "ConversationLastDeliveryTime" } } ],
            "ViewFilter": "All",
            "FocusedViewFilter": -1
        });
        if let Some(s) = shape_name {
            body["ShapeName"] = json!(s);
        }
        let env = self.envelope("FindConversation", "V2016_02_03", body);
        let (_, v, took) = self.call("FindConversation", &env, transport, headers)?;
        Ok((v, took))
    }

    fn get_folder_counts(&self, folder: &str) -> Result<(Value, Duration), CallError> {
        let body = json!({
            "__type": "GetFolderRequest:#Exchange",
            "FolderIds": [ { "__type": "DistinguishedFolderId:#Exchange", "Id": folder } ],
            "FolderShape": { "__type": "FolderResponseShape:#Exchange", "BaseShape": "IdOnly",
                "AdditionalProperties": [
                    { "__type": "PropertyUri:#Exchange", "FieldURI": "UnreadCount" },
                    { "__type": "PropertyUri:#Exchange", "FieldURI": "TotalCount" } ] },
            "ShapeName": null
        });
        let env = self.envelope("GetFolder", "Exchange2013", body);
        let (_, v, took) = self.call("GetFolder", &env, Transport::Body, Headers::Full)?;
        Ok((v, took))
    }

    fn sync_folder_items(&self, folder: &str, max: u32, sync_state: Option<&str>) -> Result<(Value, Duration), CallError> {
        let mut body = json!({
            "__type": "SyncFolderItemsRequest:#Exchange",
            "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly" },
            "SyncFolderId": { "__type": "TargetFolderId:#Exchange", "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": folder } },
            "MaxChangesReturned": max,
            "SyncScope": "NormalItems"
        });
        if let Some(s) = sync_state {
            body["SyncState"] = json!(s);
        }
        let env = self.envelope("SyncFolderItems", "Exchange2013", body);
        let (_, v, took) = self.call("SyncFolderItems", &env, Transport::Body, Headers::Full)?;
        Ok((v, took))
    }

    fn sync_folder_hierarchy(&self) -> Result<(Value, Duration), CallError> {
        let body = json!({
            "__type": "SyncFolderHierarchyRequest:#Exchange",
            "FolderShape": { "__type": "FolderResponseShape:#Exchange", "BaseShape": "IdOnly" },
            "SyncFolderId": { "__type": "TargetFolderId:#Exchange", "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "msgfolderroot" } }
        });
        let env = self.envelope("SyncFolderHierarchy", "Exchange2013", body);
        let (_, v, took) = self.call("SyncFolderHierarchy", &env, Transport::Body, Headers::Full)?;
        Ok((v, took))
    }

    fn get_conversation_items(&self, conversation_id: &str) -> Result<(Value, Duration), CallError> {
        let body = json!({
            "__type": "GetConversationItemsRequest:#Exchange",
            "Conversations": [ { "__type": "ConversationRequestType:#Exchange", "ConversationId": { "__type": "ItemId:#Exchange", "Id": conversation_id }, "SyncState": "" } ],
            "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly", "FilterHtmlContent": true, "BlockExternalImagesIfSenderUntrusted": true, "MaximumBodySize": 2097152, "MaximumRecipientsToReturn": 20 },
            "ShapeName": "ItemPart",
            "SortOrder": "DateOrderDescending",
            "MaxItemsToReturn": 20
        });
        let env = self.envelope("GetConversationItems", "V2017_08_18", body);
        let (_, v, took) = self.call("GetConversationItems", &env, Transport::Body, Headers::Full)?;
        Ok((v, took))
    }

    /// Завершает серверную сессию. Без этого каждый вход копит MAPI-сессии на ящике,
    /// и после лимита сервер отвечает TooManyObjectsOpenedException / MapiExceptionSessionLimit.
    fn logoff(&self) {
        if let Ok(url) = self.base.join("owa/logoff.owa") {
            match self.client.get(url).send() {
                Ok(r) => println!("logoff.owa → {}", r.status()),
                Err(e) => println!("logoff.owa: ошибка {e}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Разбор ответов

fn response_code(v: &Value) -> String {
    let b = &v["Body"];
    if let Some(c) = b["ResponseCode"].as_str() {
        return c.to_string();
    }
    if let Some(items) = b["ResponseMessages"]["Items"].as_array() {
        if let Some(first) = items.first() {
            let code = first["ResponseCode"].as_str().unwrap_or("?");
            let msg = first["MessageText"].as_str().unwrap_or("");
            return if msg.is_empty() { code.to_string() } else { format!("{code}: {msg}") };
        }
    }
    if v.is_array() {
        return "array".to_string();
    }
    "?".to_string()
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

fn print_folders(sd: &Value) {
    let folders = &sd["findFolders"]["Body"]["ResponseMessages"]["Items"][0]["RootFolder"]["Folders"];
    let Some(list) = folders.as_array() else {
        println!("findFolders: папок в данных сессии не найдено");
        return;
    };
    println!("Папок: {}", list.len());
    println!("{:<40} {:>8} {:>8}  {:<14} {}", "Папка", "Всего", "Непроч.", "Класс", "Известное имя");
    let mut rows: Vec<(&Value, String)> = list.iter().map(|f| (f, f["DisplayName"].as_str().unwrap_or("").to_string())).collect();
    rows.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    for (f, name) in rows {
        println!(
            "{:<40} {:>8} {:>8}  {:<14} {}",
            truncate(&name, 40),
            f["TotalCount"].as_i64().unwrap_or(0),
            f["UnreadCount"].as_i64().unwrap_or(0),
            f["FolderClass"].as_str().unwrap_or("").replace("IPF.", ""),
            f["DistinguishedFolderId"].as_str().unwrap_or("")
        );
    }
    let ss = &sd["owaUserConfig"]["SessionSettings"];
    let quota_gb = ss["QuotaUsed"].as_f64().unwrap_or(0.0) / 1e9;
    println!("\nЯщик: {:.1} ГБ занято, архив: {}, язык: {}, макс. письмо: {} КБ", quota_gb, ss["HasArchive"], ss["UserCulture"].as_str().unwrap_or("?"), ss["MaxMessageSizeInKb"]);
}

fn print_conversations(v: &Value) {
    let Some(list) = v["Body"]["Conversations"].as_array() else {
        println!("ответ без Conversations: {}", response_code(v));
        return;
    };
    println!("{:<20} {:>5} {:>4}  {}", "Последнее", "Писем", "Непр", "Тема");
    for c in list {
        let t = c["LastDeliveryTime"].as_str().unwrap_or("").replace('T', " ");
        println!(
            "{:<20} {:>5} {:>4}  {}{}",
            truncate(&t, 19),
            c["MessageCount"].as_i64().unwrap_or(0),
            c["UnreadCount"].as_i64().unwrap_or(0),
            truncate(c["ConversationTopic"].as_str().unwrap_or("(без темы)"), 70),
            if c["HasAttachments"].as_bool().unwrap_or(false) { "  📎" } else { "" }
        );
    }
    println!("Всего бесед в папке: {}", v["Body"]["TotalConversationsInView"]);
}

/// Структура JSON без значений: строки → s(len), массивы → [array(n), форма первого].
fn shape(v: &Value, depth: usize) -> Value {
    if depth > 8 {
        return json!("…");
    }
    match v {
        Value::Array(a) => {
            if a.is_empty() {
                json!([])
            } else {
                json!([format!("array({})", a.len()), shape(&a[0], depth + 1)])
            }
        }
        Value::Object(m) => {
            let mut o = serde_json::Map::new();
            for (k, x) in m {
                o.insert(k.clone(), shape(x, depth + 1));
            }
            Value::Object(o)
        }
        Value::String(s) => json!(format!("s({})", s.chars().count())),
        Value::Number(_) => json!("n"),
        Value::Bool(_) => json!("b"),
        Value::Null => Value::Null,
    }
}

fn item_keys(item: &Value) -> Vec<String> {
    let mut keys: Vec<String> = item.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    keys.sort();
    keys
}

fn cmd_shapes(s: &Session) {
    println!("\n=== SyncFolderItems: структура ответа ===");
    let first_id: Option<(String, Option<String>)> = match s.sync_folder_items("inbox", 3, None) {
        Ok((v, took)) => {
            let item = &v["Body"]["ResponseMessages"]["Items"][0];
            println!("{} за {} мс", response_code(&v), took.as_millis());
            println!("форма Items[0]: {}", serde_json::to_string_pretty(&shape(item, 0)).unwrap_or_default());
            let changes = &item["Changes"];
            let raw = serde_json::to_string(changes).unwrap_or_default();
            println!("Changes (сырой, до 1500 символов): {}", truncate(&raw, 1500));
            // Ищем первый ItemId в любой форме Changes.
            fn find_item_id(v: &Value) -> Option<(String, Option<String>)> {
                match v {
                    Value::Object(m) => {
                        if let Some(item_id) = m.get("ItemId") {
                            if let Some(id) = item_id.get("Id").and_then(|x| x.as_str()) {
                                return Some((id.to_string(), item_id.get("ChangeKey").and_then(|x| x.as_str()).map(String::from)));
                            }
                        }
                        m.values().find_map(find_item_id)
                    }
                    Value::Array(a) => a.iter().find_map(find_item_id),
                    _ => None,
                }
            }
            find_item_id(changes)
        }
        Err(e) => {
            println!("[fail] {e}");
            None
        }
    };

    println!("\n=== SyncFolderItems с дополнительными свойствами ===");
    for (label, uri) in [("item:Subject", "item:Subject"), ("Subject", "Subject")] {
        let body = json!({
            "__type": "SyncFolderItemsRequest:#Exchange",
            "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly",
                "AdditionalProperties": [ { "__type": "PropertyUri:#Exchange", "FieldURI": uri } ] },
            "SyncFolderId": { "__type": "TargetFolderId:#Exchange", "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "inbox" } },
            "MaxChangesReturned": 2,
            "SyncScope": "NormalItems"
        });
        let env = s.envelope("SyncFolderItems", "Exchange2013", body);
        match s.call("SyncFolderItems", &env, Transport::Body, Headers::Full) {
            Ok((_, v, took)) => {
                let raw = serde_json::to_string(&v["Body"]["ResponseMessages"]["Items"][0]["Changes"]).unwrap_or_default();
                println!("[{label}] {} за {} мс; Subject в выдаче: {}", response_code(&v), took.as_millis(), raw.contains("\"Subject\""));
            }
            Err(e) => println!("[{label}] {e}"),
        }
    }

    let Some((id, change_key)) = first_id else {
        println!("\nНет ItemId для проверки GetItem");
        return;
    };
    println!("\n=== GetItem: варианты формы ===");
    let item_id = json!({ "__type": "ItemId:#Exchange", "Id": id, "ChangeKey": change_key });
    let variants: Vec<(&str, Value)> = vec![
        ("IdOnly + FieldURI с префиксами (item:Subject, message:From, item:UniqueBody…)", json!({
            "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly",
            "AdditionalProperties": [
                { "__type": "PropertyUri:#Exchange", "FieldURI": "item:Subject" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "item:DateTimeReceived" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "message:From" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "item:HasAttachments" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "item:UniqueBody" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "message:InternetMessageId" } ] })),
        ("IdOnly + FieldURI без префиксов (Subject, From, UniqueBody…)", json!({
            "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly",
            "AdditionalProperties": [
                { "__type": "PropertyUri:#Exchange", "FieldURI": "Subject" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "DateTimeReceived" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "From" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "HasAttachments" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "UniqueBody" },
                { "__type": "PropertyUri:#Exchange", "FieldURI": "InternetMessageId" } ] })),
        ("ShapeName ItemPart", json!({ "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly", "FilterHtmlContent": true, "MaximumBodySize": 2097152 })),
        ("ShapeName ItemNormalizedBody", json!({ "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly", "FilterHtmlContent": true, "MaximumBodySize": 2097152 })),
        ("ShapeName MailListItem", json!({ "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly" })),
        ("AllProperties, тело до 4 КБ", json!({ "__type": "ItemResponseShape:#Exchange", "BaseShape": "AllProperties", "FilterHtmlContent": true, "MaximumBodySize": 4096 })),
    ];
    for (label, item_shape) in variants {
        let mut body = json!({
            "__type": "GetItemRequest:#Exchange",
            "ItemShape": item_shape,
            "ItemIds": [ item_id.clone() ]
        });
        if let Some(name) = label.strip_prefix("ShapeName ") {
            body["ShapeName"] = json!(name);
        }
        let env = s.envelope("GetItem", "V2017_08_18", body);
        match s.call("GetItem", &env, Transport::Body, Headers::Full) {
            Ok((_, v, took)) => {
                let item = &v["Body"]["ResponseMessages"]["Items"][0]["Items"][0];
                let ub = item["UniqueBody"]["Value"].as_str().map(|x| x.chars().count());
                let b = item["Body"]["Value"].as_str().map(|x| x.chars().count());
                println!("[{label}]\n   {} за {} мс; ключей {}; UniqueBody {:?} симв.; Body {:?} симв.\n   ключи: {}", response_code(&v), took.as_millis(), item_keys(item).len(), ub, b, item_keys(item).join(", "));
            }
            Err(e) => println!("[{label}]\n   {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Команды

fn cmd_probe(s: &Session) -> Result<()> {
    println!("\n=== Проверки транспорта ===");
    let mut first_conversation: Option<String> = None;

    let checks: Vec<(&str, Box<dyn Fn() -> Result<String, CallError>>)> = vec![
        ("FindConversation, JSON в теле, полные заголовки, ShapeName=ConversationListView", Box::new(|| {
            let (v, took) = s.find_conversation("inbox", 5, Some("ConversationListView"), Transport::Body, Headers::Full)?;
            Ok(format!("{} за {} мс, бесед {}", response_code(&v), took.as_millis(), v["Body"]["Conversations"].as_array().map(|a| a.len()).unwrap_or(0)))
        })),
        ("FindConversation, X-OWA-UrlPostData (EP=1)", Box::new(|| {
            let (v, took) = s.find_conversation("inbox", 5, Some("ConversationListView"), Transport::UrlPostData, Headers::Full)?;
            Ok(format!("{} за {} мс", response_code(&v), took.as_millis()))
        })),
        ("FindConversation, минимальные заголовки (Action, CANARY, Content-Type)", Box::new(|| {
            let (v, took) = s.find_conversation("inbox", 5, Some("ConversationListView"), Transport::Body, Headers::Minimal)?;
            Ok(format!("{} за {} мс", response_code(&v), took.as_millis()))
        })),
        ("FindConversation без X-OWA-CANARY (ожидаем отказ)", Box::new(|| {
            match s.find_conversation("inbox", 5, Some("ConversationListView"), Transport::Body, Headers::NoCanary) {
                Ok((v, _)) => Ok(format!("НЕОЖИДАННО принято: {}", response_code(&v))),
                Err(e) => Ok(format!("отказ, как и ожидалось: {e}")),
            }
        })),
        ("FindConversation без ShapeName", Box::new(|| {
            let (v, took) = s.find_conversation("inbox", 5, None, Transport::Body, Headers::Full)?;
            let topic = v["Body"]["Conversations"][0]["ConversationTopic"].is_string();
            Ok(format!("{} за {} мс, тема в ответе: {}", response_code(&v), took.as_millis(), topic))
        })),
        ("GetFolder inbox (UnreadCount, TotalCount)", Box::new(|| {
            let (v, took) = s.get_folder_counts("inbox")?;
            let f = &v["Body"]["ResponseMessages"]["Items"][0]["Folders"][0];
            Ok(format!("{} за {} мс, всего {} / непрочитанных {}", response_code(&v), took.as_millis(), f["TotalCount"], f["UnreadCount"]))
        })),
        ("SyncFolderItems inbox, 10 изменений", Box::new(|| {
            let (v, took) = s.sync_folder_items("inbox", 10, None)?;
            let item = &v["Body"]["ResponseMessages"]["Items"][0];
            let changes = item["Changes"].as_array().map(|a| a.len()).or_else(|| item["Changes"].as_object().map(|o| o.len())).unwrap_or(0);
            Ok(format!("{} за {} мс, SyncState {} символов, изменений {}, IncludesLastItemInRange={}", response_code(&v), took.as_millis(), item["SyncState"].as_str().map(|x| x.len()).unwrap_or(0), changes, item["IncludesLastItemInRange"]))
        })),
        ("SyncFolderHierarchy", Box::new(|| {
            let (v, took) = s.sync_folder_hierarchy()?;
            let item = &v["Body"]["ResponseMessages"]["Items"][0];
            Ok(format!("{} за {} мс, SyncState {} символов", response_code(&v), took.as_millis(), item["SyncState"].as_str().map(|x| x.len()).unwrap_or(0)))
        })),
    ];

    for (name, f) in checks {
        match f() {
            Ok(msg) => println!("[ok]   {name}\n       {msg}"),
            Err(e) => println!("[fail] {name}\n       {e}"),
        }
    }

    // Отдельно: беседа и UniqueBody.
    match s.find_conversation("inbox", 1, Some("ConversationListView"), Transport::Body, Headers::Full) {
        Ok((v, _)) => first_conversation = v["Body"]["Conversations"][0]["ConversationId"]["Id"].as_str().map(String::from),
        Err(e) => println!("[fail] не удалось получить беседу для проверки: {e}"),
    }
    if let Some(id) = first_conversation {
        match s.get_conversation_items(&id) {
            Ok((v, took)) => {
                let item = &v["Body"]["ResponseMessages"]["Items"][0];
                let nodes = item["Conversation"]["ConversationNodes"].as_array().map(|a| a.len()).unwrap_or(0);
                let first_item = &item["Conversation"]["ConversationNodes"][0]["Items"][0];
                let ub = &first_item["UniqueBody"];
                println!(
                    "[ok]   GetConversationItems (ShapeName=ItemPart)\n       {} за {} мс, узлов {}, UniqueBody: тип {}, {} символов, усечено={}; Subject есть: {}, InternetMessageId есть: {}",
                    response_code(&v),
                    took.as_millis(),
                    nodes,
                    ub["BodyType"].as_str().unwrap_or("нет"),
                    ub["Value"].as_str().map(|x| x.chars().count()).unwrap_or(0),
                    ub["IsTruncated"],
                    first_item["Subject"].is_string(),
                    first_item["InternetMessageId"].is_string()
                );
            }
            Err(e) => println!("[fail] GetConversationItems\n       {e}"),
        }
    }
    Ok(())
}

fn cmd_soak(cli: &Cli, password: &str, idle_min: u64) -> Result<()> {
    let s = login(cli, password, true)?;
    let (v, took) = s.get_folder_counts("inbox").map_err(|e| anyhow!("{e}"))?;
    println!("Первый запрос: {} за {} мс. Пауза {} мин без активности...", response_code(&v), took.as_millis(), idle_min);
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(idle_min * 60));
    match s.get_folder_counts("inbox") {
        Ok((v, took)) => println!("Через {} мин сессия жива: {} за {} мс", started.elapsed().as_secs() / 60, response_code(&v), took.as_millis()),
        Err(CallError::SessionExpired(st, loc)) => println!("Через {} мин сессия истекла: HTTP {st} {}", started.elapsed().as_secs() / 60, loc.unwrap_or_default()),
        Err(e) => println!("Через {} мин ошибка: {e}", started.elapsed().as_secs() / 60),
    }
    let _ = &s.jar;
    s.logoff();
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::SetPassword => {
            let pw = rpassword::prompt_password(format!("Пароль для {} на {}: ", cli.user, cli.host)).context("не удалось прочитать пароль")?;
            if pw.is_empty() {
                bail!("пустой пароль не сохраняю");
            }
            keyring_entry(&cli.user)?.set_password(&pw).map_err(|e| anyhow!("keyring: {e}"))?;
            println!("Пароль сохранён в диспетчере учётных данных Windows (служба {KEYRING_SERVICE}, пользователь {}).", cli.user);
            return Ok(());
        }
        Cmd::ForgetPassword => {
            keyring_entry(&cli.user)?.delete_credential().map_err(|e| anyhow!("keyring: {e}"))?;
            println!("Пароль удалён.");
            return Ok(());
        }
        _ => {}
    }

    let password = load_password(&cli.user)?;
    match &cli.cmd {
        Cmd::Login => {
            let s = login(&cli, &password, true)?;
            let sd = s.session_data().map_err(|e| anyhow!("{e}"))?;
            let ss = &sd["owaUserConfig"]["SessionSettings"];
            println!("sessiondata: разделы {:?}", sd.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default());
            println!("Культура {}, пояс в профиле неизвестен, квота {:.1} ГБ, папок в дереве {}", ss["UserCulture"].as_str().unwrap_or("?"), ss["QuotaUsed"].as_f64().unwrap_or(0.0) / 1e9, sd["findFolders"]["Body"]["ResponseMessages"]["Items"][0]["RootFolder"]["Folders"].as_array().map(|a| a.len()).unwrap_or(0));
            s.logoff();
        }
        Cmd::Folders => {
            let s = login(&cli, &password, false)?;
            let sd = s.session_data().map_err(|e| anyhow!("{e}"))?;
            print_folders(&sd);
            s.logoff();
        }
        Cmd::Inbox { count, folder } => {
            let s = login(&cli, &password, false)?;
            let shape = if folder == "sentitems" { "ConversationSentItemsListView" } else { "ConversationListView" };
            let (v, took) = s.find_conversation(folder, *count, Some(shape), Transport::Body, Headers::Full).map_err(|e| anyhow!("{e}"))?;
            println!("FindConversation {folder}: {} за {} мс\n", response_code(&v), took.as_millis());
            print_conversations(&v);
            s.logoff();
        }
        Cmd::Probe => {
            let s = login(&cli, &password, true)?;
            cmd_probe(&s)?;
            s.logoff();
        }
        Cmd::Soak { idle_min } => cmd_soak(&cli, &password, *idle_min)?,
        Cmd::Shapes => {
            let s = login(&cli, &password, false)?;
            cmd_shapes(&s);
            s.logoff();
        }
        Cmd::SetPassword | Cmd::ForgetPassword => unreachable!(),
    }
    Ok(())
}
