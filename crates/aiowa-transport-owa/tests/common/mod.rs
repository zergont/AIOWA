//! Фейковый сервер Exchange OWA для интеграционных тестов транспорта.
//!
//! Минимальный HTTP/1.1 на `127.0.0.1:0` без TLS (каждый ответ с `Connection: close`) и ящик
//! в памяти с выдуманными письмами «[AIOWA test]». Формы запросов и ответов — по
//! `docs/protocol-owa-json.md` (номера параграфов в комментариях); где документ форму не
//! фиксирует, ответ строится так, как его разбирает `src/protocol.rs`, и это сказано рядом.
//!
//! Сервер сам проверяет то, на чём держится настоящий: вход формой и cookies сессии (§ 1, § 5b),
//! канарейку `X-OWA-CANARY` в заголовке (§ 5b; у GET вложения — в query, § 5), `__type` первым
//! ключом каждого объекта (§ 5b),
//! конверт `<Op>JsonRequest`/`<Op>Request` (§ 3.2), лимит сессий на ящик (§ 5c). Режимы сбоя —
//! поля [`Faults`]; всё, что дошло до операции, пишется в [`Mailbox::calls`].
//!
//! Серверная часть hyper не взята: в локальном кэше крейтов нет `httpdate`, без которого она не
//! собирается, а сборка тестов идёт без сети.

use base64::prelude::*;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Barrier, Notify};
use tokio::task::JoinHandle;

/// Сборка сервера в `x-owa-version` (§ 3.3, Exchange 2019 CU15).
pub const BUILD: &str = "15.2.1748.39";
pub const USER: &str = "AIOWA\\tester";
pub const PASSWORD: &str = "fake-owa-password";
pub const OWNER_NAME: &str = "Тестовый Пользователь";
pub const OWNER: &str = "tester@aiowa.test";

pub const ROOT: &str = "AQMkFolder-msgfolderroot";
pub const INBOX: &str = "AQMkFolder-inbox";
pub const SENT: &str = "AQMkFolder-sentitems";
pub const DRAFTS: &str = "AQMkFolder-drafts";
pub const DELETED: &str = "AQMkFolder-deleteditems";
pub const PROJECTS: &str = "AQMkFolder-projects";

/// Текст отказа сервера, когда `__type` не первый ключ или его нет у абстрактного типа (§ 5b).
const ABSTRACT_CLASS: &str = "MemberAccessException: Невозможно создать абстрактный класс.";

/// Как сервер сообщает об истёкшей сессии (§ 2, § 5c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Status440,
    Status401,
    RedirectToLogon,
}

/// Режимы сбоя.
#[derive(Debug, Default)]
pub struct Faults {
    /// Следующий запрос к сессии (`service.svc`, `GetFileAttachment`) застаёт её истёкшей: сервер
    /// забывает все открытые сессии один раз.
    pub expire_next: Option<Expiry>,
    /// Каждый запрос к сессии застаёт её истёкшей: повторный вход не помогает.
    pub expire_every: Option<Expiry>,
    /// HTTP 500 на операцию: имя операции → сколько раз ещё (`u32::MAX` — всегда).
    pub fail_op: HashMap<String, u32>,
    /// GET `GetFileAttachment` отвечает HTTP 500: клиент должен перейти на операцию `GetAttachment` (§ 5).
    /// Каждая такая попытка пишется в [`Mailbox::rejected`] как `file_get`.
    pub reject_file_get: bool,
    /// GET `GetFileAttachment` отвечает 200 `text/html` без `Content-Disposition` — страница входа
    /// или ошибки IIS вместо файла: клиент должен перейти на операцию `GetAttachment`.
    pub html_page_file_get: bool,
    /// GET `GetFileAttachment` отвечает без `Content-Length`, с `Transfer-Encoding: chunked`: предел
    /// размера клиент может проверить только счётчиком прочитанных байт (§ 5).
    pub chunked_file_get: bool,
    /// Следующий `POST auth.owa` ждёт сигнала теста ([`Hold`]): так воспроизводится `logoff`,
    /// пришедший посреди повторного входа.
    pub hold_auth: Option<Arc<Hold>>,
    /// Первые запросы операции ждут друг друга ([`Gather`]): параллельные вызовы гарантированно
    /// застают один и тот же сбой (`fail_op`), в каком бы порядке ни дошли.
    pub gather: Option<Gather>,
    /// `GetItem` с `message:ReplyTo` в `AdditionalProperties` отвечает HTTP 500 (сервер, который не
    /// принимает это свойство); без него — как обычно.
    pub reject_reply_to: bool,
    /// Перед следующим запросом к `service.svc` сервер меняет канарейку сессии: старая в заголовке
    /// клиента становится недействительной (отказ CSRF-защиты, § 5b).
    pub rotate_canary_next: bool,
    /// Первый запрос этой операции ждёт сигнала теста ([`Hold`]), остальные запросы идут своим
    /// ходом: долгий запрос синхронизации не должен задерживать команды окна.
    pub hold_op: Option<(String, Arc<Hold>)>,
}

/// Запрос, задержанный сервером до сигнала теста.
#[derive(Debug, Default)]
pub struct Hold {
    /// Запрос дошёл до сервера и ждёт.
    pub arrived: Notify,
    /// Тест разрешил его обработать.
    pub release: Notify,
}

/// Первые `n` запросов операции `op` обрабатываются, только когда дошли все `n`.
#[derive(Debug)]
pub struct Gather {
    op: String,
    left: usize,
    barrier: Arc<Barrier>,
}

impl Gather {
    pub fn new(op: &str, n: usize) -> Self {
        assert!(n > 0, "Gather хотя бы на один запрос");
        Self { op: op.into(), left: n, barrier: Arc::new(Barrier::new(n)) }
    }
}

/// Чего запрос ждёт до обработки.
enum Wait {
    Hold(Arc<Hold>),
    Gather(Arc<Barrier>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addr {
    pub name: Option<String>,
    pub email: String,
}

impl Addr {
    pub fn new(name: &str, email: &str) -> Self {
        Self { name: Some(name.to_string()), email: email.to_string() }
    }
}

#[derive(Debug, Clone)]
pub struct FakeAttachment {
    /// Номер вложения внутри письма; идентификатор строится из идентификатора письма (§ 5).
    pub seq: u32,
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub is_inline: bool,
    pub content_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FakeItem {
    pub id: String,
    pub folder: String,
    /// Номер `ChangeKey`: растёт при каждой записи.
    pub ck: u64,
    /// Ревизия содержимого без признака прочтения: по ней `SyncFolderItems` отличает `Update`
    /// от `ReadFlagChange`.
    pub rev: u64,
    pub subject: String,
    pub from: Addr,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub bcc: Vec<Addr>,
    pub reply_to: Vec<Addr>,
    pub body_html: String,
    /// Момент получения, ISO с `Z` (§ 5b').
    pub received: String,
    pub is_read: bool,
    /// `NotFlagged` | `Flagged` | `Complete`.
    pub flag: String,
    pub is_draft: bool,
    pub importance: String,
    pub attachments: Vec<FakeAttachment>,
    pub internet_message_id: String,
    pub conversation: String,
}

impl FakeItem {
    /// Полный ключ, как в `SyncFolderItems`/`GetItem`/`CreateItem`: 40 символов (§ 5e, § 5f).
    pub fn change_key(&self) -> String {
        format!("CQAAABYAAAD{:029}", self.ck)
    }

    /// Усечённый ключ ответа `UpdateItem` письма: 8 символов (§ 5e).
    pub fn short_change_key(&self) -> String {
        format!("CQ{:06}", self.ck)
    }

    /// Идентификатор вложения включает идентификатор письма и меняется вместе с ним (§ 5).
    pub fn attachment_id(&self, a: &FakeAttachment) -> String {
        format!("{}:ATT{}", self.id, a.seq)
    }

    fn key_matches(&self, ck: &str) -> bool {
        ck == self.change_key() || ck == self.short_change_key()
    }
}

#[derive(Debug, Clone)]
pub struct FakeFolder {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub distinguished: Option<String>,
    pub class: String,
    pub rev: u64,
}

impl FakeFolder {
    fn change_key(&self) -> String {
        format!("AQAAABYAAAD{:029}", self.rev)
    }
}

/// Попытка входа формой `auth.owa`: поля формы, кроме пароля.
#[derive(Debug, Clone)]
pub struct LoginAttempt {
    pub username: String,
    pub destination: String,
    pub flags: String,
    pub forcedownlevel: String,
    pub is_utf8: String,
    pub content_type: String,
    pub ok: bool,
}

/// Вызов `service.svc`, прошедший проверки сессии, канарейки и конверта.
#[derive(Debug, Clone)]
pub struct Call {
    pub op: String,
    pub version: String,
    pub time_zone: String,
    /// `Body` запроса.
    pub body: Value,
    /// Заголовок `X-OWA-ClientBuildVersion` (клиент берёт его из `x-owa-version` ответов).
    pub client_build: Option<String>,
    /// Сессия (значение cookie `cadata`).
    pub session: String,
}

/// Письмо, ушедшее через `SendItem`.
#[derive(Debug, Clone)]
pub struct SentMail {
    pub subject: String,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub bcc: Vec<Addr>,
    pub body_html: String,
    pub attachments: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Default)]
struct Session {
    /// Сессия открыта (`GET /OWA` в пределах лимита) и не закрыта, не истекла.
    active: bool,
    canary: Option<String>,
}

/// Состояние сервера: ящик, сессии, журнал запросов.
#[derive(Debug)]
pub struct Mailbox {
    pub max_sessions: usize,
    pub faults: Faults,
    pub folders: Vec<FakeFolder>,
    pub items: Vec<FakeItem>,
    pub sent: Vec<SentMail>,
    pub logins: Vec<LoginAttempt>,
    /// Запросы `logoff.owa`, закрывшие открытую сессию.
    pub logoffs: usize,
    pub calls: Vec<Call>,
    /// Запросы, отвергнутые до операции: `session` (истёкшая сессия), `csrf`, `envelope: …`.
    pub rejected: Vec<String>,
    /// Идентификаторы вложений, скачанных GET `GetFileAttachment`.
    pub downloads: Vec<String>,
    sessions: HashMap<String, Session>,
    next_session: u64,
    next_item: u64,
    next_canary: u64,
}

fn received(day: u32, hour: u32) -> String {
    format!("2026-10-{day:02}T{hour:02}:15:00Z")
}

fn folder(id: &str, parent: Option<&str>, name: &str, distinguished: Option<&str>) -> FakeFolder {
    FakeFolder { id: id.into(), parent: parent.map(String::from), name: name.into(), distinguished: distinguished.map(String::from), class: "IPF.Note".into(), rev: 1 }
}

fn owner() -> Addr {
    Addr::new(OWNER_NAME, OWNER)
}

/// Признак PNG для инлайновой картинки: содержимое серверу безразлично.
const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];

impl Mailbox {
    /// Ящик с папками «Входящие», «Отправленные», «Черновики», «Удалённые» и подпапкой
    /// «Проекты» во «Входящих»; пять писем.
    pub fn seeded() -> Self {
        let vasya = Addr::new("Вася Пупкин", "vasya@aiowa.test");
        let petr = Addr::new("Пётр Петров", "petr@aiowa.test");
        let anna = Addr::new("Анна Смирнова", "anna@aiowa.test");
        let base = |n: u64, folder_id: &str, subject: &str, from: &Addr, body: &str, day: u32| FakeItem {
            id: format!("AAMkItem-{n:04}"),
            folder: folder_id.into(),
            ck: 1,
            rev: 1,
            subject: subject.into(),
            from: from.clone(),
            to: vec![owner()],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            body_html: body.into(),
            received: received(day, 8),
            is_read: false,
            flag: "NotFlagged".into(),
            is_draft: false,
            importance: "Normal".into(),
            attachments: vec![],
            internet_message_id: format!("<item-{n}@aiowa.test>"),
            conversation: format!("AAQkConv-{n:04}"),
        };
        let mut shipment = base(1, INBOX, "[AIOWA test] Отгрузка запчастей на Колыму", &vasya, "<div>Отгрузили <b>запчасти</b> на Колыму, спецификация во вложении.</div><img src=\"cid:logo@aiowa.test\">", 20);
        shipment.cc = vec![petr.clone()];
        shipment.reply_to = vec![Addr::new("Отдел снабжения", "supply@aiowa.test")];
        shipment.attachments = vec![
            FakeAttachment { seq: 1, name: "спецификация.txt".into(), content_type: "text/plain".into(), bytes: "Поставка шлёпанцев, 40 пар".as_bytes().to_vec(), is_inline: false, content_id: None },
            FakeAttachment { seq: 2, name: "logo.png".into(), content_type: "image/png".into(), bytes: PNG.to_vec(), is_inline: true, content_id: Some("logo@aiowa.test".into()) },
        ];
        let mut budget = base(2, INBOX, "[AIOWA test] Совещание по бюджету", &petr, "<div>Совещание по бюджету в четверг.</div>", 19);
        budget.is_read = true;
        let report = base(3, INBOX, "[AIOWA test] Отчёт за сентябрь", &anna, "<div>Отчёт за сентябрь готов.</div>", 18);
        let mut answer = base(4, SENT, "[AIOWA test] Re: Отгрузка", &owner(), "<div>Спасибо, получили.</div>", 20);
        answer.to = vec![vasya.clone()];
        answer.is_read = true;
        let mut plan = base(5, PROJECTS, "[AIOWA test] План проекта", &anna, "<div>План проекта на квартал.</div>", 15);
        plan.is_read = true;
        Self {
            max_sessions: 16,
            faults: Faults::default(),
            folders: vec![
                folder(ROOT, None, "Корень", Some("msgfolderroot")),
                folder(INBOX, Some(ROOT), "Входящие", Some("inbox")),
                folder(SENT, Some(ROOT), "Отправленные", Some("sentitems")),
                folder(DRAFTS, Some(ROOT), "Черновики", Some("drafts")),
                folder(DELETED, Some(ROOT), "Удалённые", Some("deleteditems")),
                folder(PROJECTS, Some(INBOX), "Проекты", None),
            ],
            items: vec![shipment, budget, report, answer, plan],
            sent: vec![],
            logins: vec![],
            logoffs: 0,
            calls: vec![],
            rejected: vec![],
            downloads: vec![],
            sessions: HashMap::new(),
            next_session: 1,
            next_item: 6,
            next_canary: 1,
        }
    }

    // --- для тестов ----------------------------------------------------------

    /// Открытые сессии (вход завершён, `logoff` не было, сессия не истекла).
    pub fn active_sessions(&self) -> usize {
        self.sessions.values().filter(|s| s.active).count()
    }

    pub fn logins_ok(&self) -> usize {
        self.logins.iter().filter(|l| l.ok).count()
    }

    pub fn calls_of(&self, op: &str) -> Vec<&Call> {
        self.calls.iter().filter(|c| c.op == op).collect()
    }

    pub fn item(&self, id: &str) -> Option<&FakeItem> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn item_by_subject(&self, subject: &str) -> Option<&FakeItem> {
        self.items.iter().find(|i| i.subject == subject)
    }

    pub fn items_in(&self, folder_id: &str) -> Vec<&FakeItem> {
        self.items.iter().filter(|i| i.folder == folder_id).collect()
    }

    /// Чужая правка (другой клиент): прочитано или нет.
    pub fn set_read_elsewhere(&mut self, id: &str, read: bool) {
        let it = self.items.iter_mut().find(|i| i.id == id).expect("письмо есть");
        it.is_read = read;
        it.ck += 1;
    }

    /// Чужое удаление навсегда.
    pub fn remove_elsewhere(&mut self, id: &str) {
        self.items.retain(|i| i.id != id);
    }

    /// Новое письмо во «Входящих» от внешнего отправителя; возвращает идентификатор.
    pub fn deliver(&mut self, subject: &str, from: Addr, body_html: &str) -> String {
        let id = self.new_item_id();
        let n = self.next_item;
        self.items.push(FakeItem {
            id: id.clone(),
            folder: INBOX.into(),
            ck: 1,
            rev: 1,
            subject: subject.into(),
            from,
            to: vec![owner()],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            body_html: body_html.into(),
            received: received(22, 9),
            is_read: false,
            flag: "NotFlagged".into(),
            is_draft: false,
            importance: "Normal".into(),
            attachments: vec![],
            internet_message_id: format!("<item-{n}@aiowa.test>"),
            conversation: format!("AAQkConv-{n:04}"),
        });
        id
    }

    fn new_item_id(&mut self) -> String {
        let id = format!("AAMkItem-{:04}", self.next_item);
        self.next_item += 1;
        id
    }

    fn new_canary(&mut self) -> String {
        // 76 символов, как у настоящей cookie (§ 1).
        let c = format!("{:X<76}", format!("fakeCanary{:04}-", self.next_canary));
        self.next_canary += 1;
        c
    }

    fn folder_by_id(&self, id: &str) -> Option<&FakeFolder> {
        self.folders.iter().find(|f| f.id == id)
    }

    // --- HTTP ----------------------------------------------------------------

    fn handle(&mut self, req: &Request) -> Response {
        let resp = match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/owa/auth/logon.aspx") => Response::html(200, "<html><form action=\"/owa/auth.owa\" method=\"post\"><input name=\"username\"><input type=\"password\" name=\"password\"></form></html>"),
            ("POST", "/owa/auth.owa") => self.auth_post(req),
            // IIS не различает регистр пути: `/OWA` и `/owa` — одно и то же.
            ("GET", "/owa") => self.owa_entry(req),
            ("GET", "/owa/") => self.owa_page(req),
            ("POST", "/owa/service.svc") => self.service(req),
            ("GET", "/owa/service.svc/s/getfileattachment") => self.file_attachment(req),
            ("GET", "/owa/logoff.owa") => self.logoff(req),
            _ => Response::html(404, "<html>Not Found</html>"),
        };
        resp.header("X-OWA-Version", BUILD)
    }

    fn logon_redirect(req: &Request, reason: u32) -> Response {
        let host = req.header("host").unwrap_or("127.0.0.1");
        let back: String = url::form_urlencoded::byte_serialize(format!("http://{host}/owa").as_bytes()).collect();
        Response::redirect(302, &format!("/owa/auth/logon.aspx?url={back}&reason={reason}"))
    }

    /// § 1, § 5b: форма входа. Неверные учётные данные — снова на страницу входа с `reason=2`;
    /// верные — cookies `cadata*` и редирект на `/OWA`, где проверяется лимит сессий.
    fn auth_post(&mut self, req: &Request) -> Response {
        let form: HashMap<String, String> = url::form_urlencoded::parse(&req.body).into_owned().collect();
        let field = |k: &str| form.get(k).cloned().unwrap_or_default();
        let ok = field("username") == USER && field("password") == PASSWORD;
        self.logins.push(LoginAttempt {
            username: field("username"),
            destination: field("destination"),
            flags: field("flags"),
            forcedownlevel: field("forcedownlevel"),
            is_utf8: field("isUtf8"),
            content_type: req.header("content-type").unwrap_or("").to_string(),
            ok,
        });
        if !ok {
            return Self::logon_redirect(req, 2);
        }
        let token = format!("fakeCadata{:04}", self.next_session);
        self.next_session += 1;
        self.sessions.insert(token.clone(), Session::default());
        Response::redirect(302, "/OWA")
            .header("Set-Cookie", &format!("cadata={token}; path=/; HttpOnly"))
            .header("Set-Cookie", "cadataTTL=fakeTTL; path=/; HttpOnly")
            .header("Set-Cookie", "cadataSig=fakeSig; path=/; HttpOnly")
    }

    /// § 5b, § 5c: `GET /OWA` открывает сессию; сверх лимита — редирект на `errorfe.aspx`
    /// с `MapiExceptionSessionLimit`.
    fn owa_entry(&mut self, req: &Request) -> Response {
        let active = self.active_sessions();
        let Some(s) = req.cookie("cadata").and_then(|t| self.sessions.get_mut(&t)) else {
            return Self::logon_redirect(req, 0);
        };
        if !s.active {
            if active >= self.max_sessions {
                let token = req.cookie("cadata").unwrap_or_default();
                self.sessions.remove(&token);
                return Response::redirect(302, "/owa/auth/errorfe.aspx?httpCode=500&owaError=Microsoft.Exchange.Data.Storage.TooManyObjectsOpenedException&inex=Microsoft.Mapi.MapiExceptionSessionLimit");
            }
            s.active = true;
        }
        Response::redirect(301, "/owa/").header("Set-Cookie", "ClientId=FAKECLIENTID; path=/").header("Set-Cookie", "X-BackEndCookie=fake-backend; path=/")
    }

    /// § 5b: `GET /owa/` ставит cookie `X-OWA-CANARY`.
    fn owa_page(&mut self, req: &Request) -> Response {
        let token = req.cookie("cadata").unwrap_or_default();
        if !self.sessions.get(&token).is_some_and(|s| s.active) {
            return Self::logon_redirect(req, 0);
        }
        let canary = self.new_canary();
        self.sessions.get_mut(&token).expect("сессия есть").canary = Some(canary.clone());
        Response::html(200, "<html><body>OWA</body></html>").header("Set-Cookie", &format!("X-OWA-CANARY={canary}; path=/")).header("Set-Cookie", "UserContext=fake-user-context; path=/")
    }

    /// § 5c: `logoff.owa` закрывает сессию и отвечает 302.
    fn logoff(&mut self, req: &Request) -> Response {
        if let Some(token) = req.cookie("cadata")
            && self.sessions.remove(&token).is_some_and(|s| s.active)
        {
            self.logoffs += 1;
        }
        Response::redirect(302, "/owa/auth/logoff.aspx?Cmd=logoff&src=exch").header("Set-Cookie", "cadata=; expires=Thu, 01-Jan-1970 00:00:00 GMT; path=/")
    }

    fn expired(&mut self, req: &Request, style: Expiry) -> Response {
        self.rejected.push("session".into());
        match style {
            Expiry::Status440 => Response::html(440, ""),
            Expiry::Status401 => Response::html(401, ""),
            Expiry::RedirectToLogon => Self::logon_redirect(req, 0),
        }
    }

    /// Сессия запроса по cookie `cadata` с учётом режимов истечения; `Err` — готовый ответ.
    fn session_of(&mut self, req: &Request) -> Result<String, Response> {
        if let Some(style) = self.faults.expire_every.or_else(|| self.faults.expire_next.take()) {
            for s in self.sessions.values_mut() {
                s.active = false;
            }
            return Err(self.expired(req, style));
        }
        match req.cookie("cadata").filter(|t| self.sessions.get(t).is_some_and(|s| s.active)) {
            Some(t) => Ok(t),
            None => Err(self.expired(req, Expiry::Status440)),
        }
    }

    /// § 5b: без верной канарейки — редирект на `errorFE.aspx` (отказ CSRF-защиты).
    fn check_canary(&mut self, token: &str, presented: Option<&str>) -> Result<(), Response> {
        let expected = self.sessions.get(token).and_then(|s| s.canary.clone());
        if expected.is_none() || presented != expected.as_deref() {
            self.rejected.push("csrf".into());
            return Err(Response::redirect(302, "/owa/auth/errorFE.aspx?httpCode=500"));
        }
        Ok(())
    }

    /// § 5: `GET service.svc/s/GetFileAttachment?id=…&X-OWA-CANARY=…` — файл как есть, имя в
    /// процентном кодировании прямо в `filename=`. Канарейка — только из query, как у браузерной
    /// OWA: заголовок клиент дублирует, но сервер на него здесь не смотрит.
    fn file_attachment(&mut self, req: &Request) -> Response {
        let token = match self.session_of(req) {
            Ok(t) => t,
            Err(r) => return r,
        };
        if let Err(r) = self.check_canary(&token, req.query("X-OWA-CANARY")) {
            return r;
        }
        if self.faults.reject_file_get {
            self.rejected.push("file_get".into());
            return Response::html(500, "<html>Ошибка сервера</html>");
        }
        if self.faults.html_page_file_get {
            self.rejected.push("file_get_page".into());
            return Response::html(200, "<html><title>Outlook</title>страница входа</html>");
        }
        let id = req.query("id").unwrap_or("");
        let found = self.items.iter().find_map(|it| it.attachments.iter().find(|a| it.attachment_id(a) == id));
        let Some(a) = found else {
            return Response::html(404, "<html>Not Found</html>");
        };
        let name = utf8_percent_encode(&a.name, NON_ALPHANUMERIC).to_string();
        let resp = Response { status: 200, headers: vec![("Content-Type".into(), a.content_type.clone()), ("Content-Disposition".into(), format!("attachment; filename={name}"))], body: a.bytes.clone(), chunked: self.faults.chunked_file_get };
        self.downloads.push(id.to_string());
        resp
    }

    /// Чего запрос ждёт до обработки: `POST auth.owa` при [`Faults::hold_auth`] (один раз),
    /// операция [`Faults::gather`] — пока не дойдут все её запросы.
    fn wait_for(&mut self, req: &Request) -> Option<Wait> {
        if req.method == "POST" && req.path == "/owa/auth.owa" {
            return self.faults.hold_auth.take().map(Wait::Hold);
        }
        if req.method != "POST" || req.path != "/owa/service.svc" {
            return None;
        }
        if self.faults.hold_op.as_ref().is_some_and(|(op, _)| req.query("action") == Some(op.as_str())) {
            return self.faults.hold_op.take().map(|(_, h)| Wait::Hold(h));
        }
        let g = self.faults.gather.as_mut().filter(|g| req.query("action") == Some(g.op.as_str()))?;
        g.left -= 1;
        let barrier = g.barrier.clone();
        if g.left == 0 {
            self.faults.gather = None;
        }
        Some(Wait::Gather(barrier))
    }

    /// § 3: `POST service.svc?action=<Op>` с JSON в теле.
    fn service(&mut self, req: &Request) -> Response {
        let token = match self.session_of(req) {
            Ok(t) => t,
            Err(r) => return r,
        };
        if std::mem::take(&mut self.faults.rotate_canary_next) {
            let c = self.new_canary();
            self.sessions.get_mut(&token).expect("сессия есть").canary = Some(c);
        }
        if let Err(r) = self.check_canary(&token, req.header("x-owa-canary")) {
            return r;
        }
        let op = req.query("action").unwrap_or("").to_string();
        // Минимальный набор заголовков (§ 5b): `Action`, `X-OWA-CANARY`, `Content-Type`.
        if req.header("action") != Some(op.as_str()) || op.is_empty() {
            self.rejected.push(format!("envelope: заголовок Action {:?} не совпадает с action={op}", req.header("action")));
            return Response::html(400, "<html>Bad Request</html>");
        }
        if !req.header("content-type").is_some_and(|c| c.to_ascii_lowercase().starts_with("application/json")) {
            self.rejected.push(format!("envelope: Content-Type {:?}", req.header("content-type")));
            return Response::html(415, "<html>Unsupported Media Type</html>");
        }
        let v: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => return self.envelope_fault(format!("тело не JSON: {e}")),
        };
        if let Err(path) = type_first_everywhere(&v, "$") {
            return self.envelope_fault(format!("{ABSTRACT_CLASS} (__type не первый ключ: {path})"));
        }
        let deserialize = format!("Cannot deserialize object of type {op}JsonRequest");
        if ty(&v) != Some(format!("{op}JsonRequest:#Exchange").as_str()) || v.pointer("/Header/__type").and_then(Value::as_str) != Some("JsonRequestHeaders:#Exchange") {
            return self.envelope_fault(deserialize);
        }
        let Some(body) = v.get("Body").filter(|b| ty(b) == Some(format!("{op}Request:#Exchange").as_str())) else {
            return self.envelope_fault(deserialize);
        };
        let version = v.pointer("/Header/RequestServerVersion").and_then(Value::as_str).unwrap_or("").to_string();
        let time_zone = v.pointer("/Header/TimeZoneContext/TimeZoneDefinition/Id").and_then(Value::as_str).unwrap_or("").to_string();
        self.calls.push(Call { op: op.clone(), version, time_zone, body: body.clone(), client_build: req.header("x-owa-clientbuildversion").map(String::from), session: token });
        if let Some(left) = self.faults.fail_op.get_mut(&op)
            && *left > 0
        {
            if *left != u32::MAX {
                *left -= 1;
            }
            return fault(&format!("фейк: сбой операции {op} по режиму fail_op"));
        }
        if op == "GetItem" && self.faults.reject_reply_to && body.pointer("/ItemShape/AdditionalProperties").is_some_and(|p| p.to_string().contains("message:ReplyTo")) {
            return fault("фейк: свойство message:ReplyTo отвергнуто по режиму reject_reply_to");
        }
        let result = match op.as_str() {
            "SyncFolderHierarchy" => self.sync_folder_hierarchy(body),
            "SyncFolderItems" => self.sync_folder_items(body),
            "FindItem" => self.find_item(body),
            "GetFolder" => self.get_folder(body),
            "GetItem" => self.get_item(body),
            "GetAttachment" => self.get_attachment(body),
            "UpdateItem" => self.update_item(body),
            "MoveItem" => self.move_item(body),
            "DeleteItem" => self.delete_item(body),
            "CreateItem" => self.create_item(body),
            "CreateAttachment" => self.create_attachment(body),
            "SendItem" => self.send_item(body),
            other => Err(format!("фейк: операция {other} не поддерживается")),
        };
        match result {
            Ok(messages) => Response::json(200, &json!({
                "Header": { "ServerVersionInfo": { "MajorVersion": 15, "MinorVersion": 2, "MajorBuildNumber": 1748, "MinorBuildNumber": 39, "Version": "V2017_08_18" } },
                "Body": { "ResponseMessages": { "Items": messages } }
            })),
            Err(text) => fault(&text),
        }
    }

    fn envelope_fault(&mut self, text: String) -> Response {
        self.rejected.push(format!("envelope: {text}"));
        fault(&text)
    }

    // --- папки ---------------------------------------------------------------

    /// Папка по `BaseFolderId` (`FolderId` или `DistinguishedFolderId`).
    fn resolve_base_folder(&self, base: &Value) -> Result<Option<String>, String> {
        let id = base.get("Id").and_then(Value::as_str).ok_or("BaseFolderId без Id")?;
        Ok(match ty(base) {
            Some("DistinguishedFolderId:#Exchange") => self.folders.iter().find(|f| f.distinguished.as_deref() == Some(id)).map(|f| f.id.clone()),
            Some("FolderId:#Exchange") => self.folder_by_id(id).map(|f| f.id.clone()),
            None => return Err(ABSTRACT_CLASS.into()),
            Some(other) => return Err(format!("фейк: тип папки {other} не поддерживается")),
        })
    }

    /// Папка по `TargetFolderId { BaseFolderId }`.
    fn resolve_target_folder(&self, target: Option<&Value>) -> Result<Option<String>, String> {
        let base = target.and_then(|t| t.get("BaseFolderId")).ok_or("нет TargetFolderId.BaseFolderId")?;
        self.resolve_base_folder(base)
    }

    fn folder_json(&self, f: &FakeFolder, all: bool, fields: &[String]) -> Value {
        let items: Vec<&FakeItem> = self.items_in(&f.id);
        let mut o = Map::new();
        o.insert("__type".into(), json!("Folder:#Exchange"));
        o.insert("FolderId".into(), json!({ "Id": f.id, "ChangeKey": f.change_key() }));
        let wants = |name: &str| all || fields.iter().any(|x| x == name || x == &format!("folder:{name}"));
        if all {
            if let Some(p) = &f.parent {
                o.insert("ParentFolderId".into(), json!({ "Id": p, "ChangeKey": self.folder_by_id(p).map(FakeFolder::change_key) }));
            }
            o.insert("FolderClass".into(), json!(f.class));
            // § 5b': с `IdOnly` сервер имени не отдаёт даже по `AdditionalProperties`.
            o.insert("DisplayName".into(), json!(f.name));
            o.insert("ChildFolderCount".into(), json!(self.folders.iter().filter(|c| c.parent.as_deref() == Some(f.id.as_str())).count()));
        }
        if wants("TotalCount") {
            o.insert("TotalCount".into(), json!(items.len()));
        }
        if wants("UnreadCount") {
            o.insert("UnreadCount".into(), json!(items.iter().filter(|i| !i.is_read).count()));
        }
        // Известное имя — строкой, как его читает `protocol::parse_folder`; только по запросу
        // (клиент умеет повторить без него, если сервер отвергнет свойство).
        if fields.iter().any(|x| x == "DistinguishedFolderId" || x == "folder:DistinguishedFolderId")
            && let Some(d) = &f.distinguished
        {
            o.insert("DistinguishedFolderId".into(), json!(d));
        }
        Value::Object(o)
    }

    /// § 5b', § 5j: `SyncFolderHierarchy` от `msgfolderroot`; сам корень в изменениях не приходит.
    /// Состояние — снимок известных клиенту папок (base64 JSON), изменения — разница с ним.
    fn sync_folder_hierarchy(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        let shape = body.get("FolderShape").ok_or("нет FolderShape")?;
        let all = match shape.get("BaseShape").and_then(Value::as_str) {
            Some("AllProperties") => true,
            Some("IdOnly") => false,
            other => return Err(format!("фейк: BaseShape {other:?} не поддерживается")),
        };
        let fields = additional_properties(shape)?;
        for f in &fields {
            if !matches!(f.as_str(), "DistinguishedFolderId" | "folder:DistinguishedFolderId" | "TotalCount" | "UnreadCount" | "folder:DisplayName" | "DisplayName") {
                return Err(format!("Invalid value '{f}' for enum PropertyUriEnum"));
            }
        }
        const TYPE: &str = "SyncFolderHierarchyResponseMessage:#Exchange";
        if self.resolve_target_folder(body.get("SyncFolderId"))?.as_deref() != Some(ROOT) {
            return Ok(vec![error_message(TYPE, "ErrorFolderNotFound", "фейк: синхронизация иерархии только от msgfolderroot")]);
        }
        let known: BTreeMap<String, u64> = match body.get("SyncState").and_then(Value::as_str) {
            None => BTreeMap::new(),
            Some(s) => match decode_state(s, "H").and_then(|v| serde_json::from_value(v).ok()) {
                Some(k) => k,
                None => return Ok(vec![error_message(TYPE, "ErrorInvalidSyncStateData", "Synchronization state data is corrupt or otherwise invalid.")]),
            },
        };
        let mut changes = Vec::new();
        let mut next: BTreeMap<String, u64> = BTreeMap::new();
        for id in known.keys() {
            if self.folder_by_id(id).is_none() {
                changes.push(json!({ "__type": "SyncFolderHierarchyDeleteType:#Exchange", "ChangeType": "Delete", "FolderId": { "Id": id } }));
            }
        }
        // Родители раньше детей: папки идут в порядке списка (он так и заполнен).
        for f in self.folders.iter().filter(|f| f.id != ROOT) {
            next.insert(f.id.clone(), f.rev);
            let kind = match known.get(&f.id) {
                None => "Create",
                Some(rev) if *rev != f.rev => "Update",
                Some(_) => continue,
            };
            changes.push(json!({ "__type": "SyncFolderHierarchyCreateOrUpdateType:#Exchange", "ChangeType": kind, "Folder": self.folder_json(f, all, &fields) }));
        }
        Ok(vec![json!({
            "__type": TYPE, "ResponseClass": "Success", "ResponseCode": "NoError",
            "SyncState": encode_state("H", &json!(next)),
            "IncludesLastFolderInRange": true,
            "Changes": { "Changes": changes }
        })])
    }

    /// `GetFolder` счётчиков (§ 3.4): `Folders[0]` с `FolderId`, `TotalCount`, `UnreadCount`.
    fn get_folder(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        let shape = body.get("FolderShape").ok_or("нет FolderShape")?;
        let fields = additional_properties(shape)?;
        let ids = body.get("FolderIds").and_then(Value::as_array).ok_or("нет FolderIds")?;
        let mut out = Vec::new();
        for base in ids {
            out.push(match self.resolve_base_folder(base)? {
                Some(id) => json!({ "__type": "GetFolderResponseMessage:#Exchange", "ResponseClass": "Success", "ResponseCode": "NoError",
                    "Folders": [ self.folder_json(self.folder_by_id(&id).expect("папка есть"), false, &fields) ] }),
                None => error_message("GetFolderResponseMessage:#Exchange", "ErrorFolderNotFound", "The specified folder could not be found in the store."),
            });
        }
        Ok(out)
    }

    // --- письма: чтение ------------------------------------------------------

    /// § 5b': `SyncFolderItems`, изменения во вложенном `Changes.Changes[]`, без
    /// `IncludesLastItemInRange` (клиент узнаёт конец по короткой пачке). Состояние — снимок
    /// известных клиенту писем папки: (ревизия, прочитано) по идентификатору.
    fn sync_folder_items(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        const TYPE: &str = "SyncFolderItemsResponseMessage:#Exchange";
        let shape = body.get("ItemShape").ok_or("нет ItemShape")?;
        if shape.get("BaseShape").and_then(Value::as_str) != Some("IdOnly") {
            return Err("фейк: SyncFolderItems только с BaseShape IdOnly".into());
        }
        let fields = additional_properties(shape)?;
        for f in &fields {
            field_key(f)?;
        }
        if body.get("SyncScope").and_then(Value::as_str) != Some("NormalItems") {
            return Err("фейк: SyncScope должен быть NormalItems".into());
        }
        let max = body.get("MaxChangesReturned").and_then(Value::as_u64).ok_or("нет MaxChangesReturned")? as usize;
        let Some(folder_id) = self.resolve_target_folder(body.get("SyncFolderId"))? else {
            return Ok(vec![error_message(TYPE, "ErrorSyncFolderNotFound", "The specified folder could not be found in the store.")]);
        };
        let mut known: BTreeMap<String, (u64, bool)> = match body.get("SyncState").and_then(Value::as_str) {
            None => BTreeMap::new(),
            Some(s) => match decode_state(s, &format!("F:{folder_id}")).and_then(|v| serde_json::from_value(v).ok()) {
                Some(k) => k,
                None => return Ok(vec![error_message(TYPE, "ErrorInvalidSyncStateData", "Synchronization state data is corrupt or otherwise invalid.")]),
            },
        };
        let mut current: Vec<&FakeItem> = self.items_in(&folder_id);
        current.sort_by(|a, b| a.id.cmp(&b.id));
        let mut changes: Vec<Value> = Vec::new();
        let gone: Vec<String> = known.keys().filter(|id| !current.iter().any(|i| &i.id == *id)).cloned().collect();
        for id in gone {
            if changes.len() >= max {
                break;
            }
            changes.push(json!({ "__type": "SyncFolderItemsDeleteType:#Exchange", "ChangeType": "Delete", "ItemId": { "Id": id } }));
            known.remove(&id);
        }
        for it in current {
            if changes.len() >= max {
                break;
            }
            let change = match known.get(&it.id) {
                None => json!({ "__type": "SyncFolderItemsCreateType:#Exchange", "ChangeType": "Create", "Item": self.message_json(it, Shape::IdOnly, &fields, u64::MAX)? }),
                // Форма `Update` в документе не снята; разбор (`protocol::parse_item_changes`) тот же, что у `Create`.
                Some((rev, _)) if *rev != it.rev => json!({ "__type": "SyncFolderItemsCreateOrUpdateType:#Exchange", "ChangeType": "Update", "Item": self.message_json(it, Shape::IdOnly, &fields, u64::MAX)? }),
                // § 5b': по схеме EWS у `ReadFlagChange` только `ItemId` и `IsRead`.
                Some((_, read)) if *read != it.is_read => json!({ "__type": "SyncFolderItemsReadFlagType:#Exchange", "ChangeType": "ReadFlagChange", "ItemId": { "Id": it.id, "ChangeKey": it.change_key() }, "IsRead": it.is_read }),
                Some(_) => continue,
            };
            changes.push(change);
            known.insert(it.id.clone(), (it.rev, it.is_read));
        }
        Ok(vec![json!({
            "__type": TYPE, "ResponseClass": "Success", "ResponseCode": "NoError",
            "Changes": { "Changes": changes },
            "SyncState": encode_state(&format!("F:{folder_id}"), &json!(known)),
            "TotalCount": 0
        })])
    }

    /// § 5m: `FindItem` писем одной папки — `IdOnly` с `AdditionalProperties`, `Traversal: Shallow`,
    /// страница `IndexedPageView` (`Offset`, `MaxEntriesReturned`), сортировка `SortResults` по
    /// `DateTimeReceived`. Ответ в форме, снятой с сервера: `RootFolder` с `Items`,
    /// `TotalItemsInView`, `IncludesLastItemInRange` и `IndexedPagingOffset` (смещение следующей страницы).
    fn find_item(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        const TYPE: &str = "FindItemResponseMessage:#Exchange";
        let shape = body.get("ItemShape").ok_or("нет ItemShape")?;
        if shape.get("BaseShape").and_then(Value::as_str) != Some("IdOnly") {
            return Err("фейк: FindItem писем только с BaseShape IdOnly".into());
        }
        let fields = additional_properties(shape)?;
        for f in &fields {
            field_key(f)?;
        }
        if body.get("Traversal").and_then(Value::as_str) != Some("Shallow") {
            return Err("фейк: Traversal должен быть Shallow".into());
        }
        let parents = body.get("ParentFolderIds").and_then(Value::as_array).ok_or("нет ParentFolderIds")?;
        let [base] = parents.as_slice() else { return Err("фейк: FindItem по одной папке".into()) };
        let Some(folder_id) = self.resolve_base_folder(base)? else {
            return Ok(vec![error_message(TYPE, "ErrorFolderNotFound", "The specified folder could not be found in the store.")]);
        };
        let paging = body.get("Paging").ok_or("нет Paging")?;
        match ty(paging) {
            Some("IndexedPageView:#Exchange") => {}
            None => return Err(ABSTRACT_CLASS.into()),
            Some(other) => return Err(format!("фейк: Paging {other} не поддерживается")),
        }
        if paging.get("BasePoint").and_then(Value::as_str) != Some("Beginning") {
            return Err("фейк: BasePoint только Beginning".into());
        }
        let offset = paging.get("Offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max = paging.get("MaxEntriesReturned").and_then(Value::as_u64).ok_or("нет MaxEntriesReturned")? as usize;
        let sort = body.get("SortOrder").and_then(Value::as_array).ok_or("нет SortOrder")?;
        let [order] = sort.as_slice() else { return Err("фейк: сортировка по одному полю".into()) };
        if ty(order) != Some("SortResults:#Exchange") || order.pointer("/Path/__type").and_then(Value::as_str) != Some("PropertyUri:#Exchange") {
            return Err(ABSTRACT_CLASS.into());
        }
        if !matches!(order.pointer("/Path/FieldURI").and_then(Value::as_str), Some("item:DateTimeReceived" | "DateTimeReceived")) {
            return Err("фейк: сортировка только по DateTimeReceived".into());
        }
        let descending = match order.get("Order").and_then(Value::as_str) {
            Some("Descending") => true,
            Some("Ascending") => false,
            other => return Err(format!("фейк: Order {other:?}")),
        };
        let mut items: Vec<&FakeItem> = self.items_in(&folder_id);
        // Момент получения — ISO-строка с `Z`: лексикографический порядок совпадает с хронологическим.
        items.sort_by(|a, b| a.received.cmp(&b.received).then(a.id.cmp(&b.id)));
        if descending {
            items.reverse();
        }
        let total = items.len();
        let page = items.iter().skip(offset).take(max).map(|it| self.message_json(it, Shape::IdOnly, &fields, u64::MAX)).collect::<Result<Vec<_>, _>>()?;
        let next = offset + page.len();
        Ok(vec![json!({
            "__type": TYPE, "ResponseCode": "NoError", "ResponseClass": "Success", "HighlightTerms": null,
            "RootFolder": { "IncludesLastItemInRange": next >= total, "IndexedPagingOffset": next, "TotalItemsInView": total, "Groups": null, "Items": page }
        })])
    }

    /// § 5b', § 5j, § 5k: `GetItem` в именованной форме (`ItemPart`, `ItemNormalizedBody`,
    /// `MailListItem`) или `AllProperties`; `AdditionalProperties` действуют поверх формы.
    /// Сообщение на каждый идентификатор в порядке запроса.
    fn get_item(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        const TYPE: &str = "ItemInfoResponseMessage:#Exchange";
        let shape_v = body.get("ItemShape").ok_or("нет ItemShape")?;
        let fields = additional_properties(shape_v)?;
        for f in &fields {
            field_key(f)?;
        }
        let max_body = shape_v.get("MaximumBodySize").and_then(Value::as_u64).unwrap_or(u64::MAX);
        let shape = match (body.get("ShapeName").and_then(Value::as_str), shape_v.get("BaseShape").and_then(Value::as_str)) {
            (Some("ItemPart"), _) => Shape::ItemPart,
            (Some("ItemNormalizedBody"), _) => Shape::NormalizedBody,
            (Some("MailListItem"), _) => Shape::MailListItem,
            (None, Some("AllProperties")) => Shape::AllProperties,
            (None, Some("IdOnly")) => Shape::IdOnly,
            (name, base) => return Err(format!("фейк: форма GetItem {name:?}/{base:?} не поддерживается")),
        };
        let mut out = Vec::new();
        for (id, _) in item_ids(body.get("ItemIds"))? {
            out.push(match self.item(&id) {
                Some(it) => json!({ "__type": TYPE, "ResponseClass": "Success", "ResponseCode": "NoError", "Items": [ self.message_json(it, shape, &fields, max_body)? ] }),
                None => not_found(TYPE),
            });
        }
        Ok(out)
    }

    /// § 5: запасной путь `GetAttachment` — `Attachments[0]` типа `FileAttachment` с `Content` в base64.
    fn get_attachment(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        const TYPE: &str = "AttachmentInfoResponseMessage:#Exchange";
        let ids = body.get("AttachmentIds").and_then(Value::as_array).ok_or("нет AttachmentIds")?;
        let mut out = Vec::new();
        for a in ids {
            let id = a.get("Id").and_then(Value::as_str).ok_or("AttachmentId без Id")?;
            let found = self.items.iter().find_map(|it| it.attachments.iter().find(|x| it.attachment_id(x) == id).map(|x| (it, x)));
            out.push(match found {
                Some((it, x)) => json!({ "__type": TYPE, "ResponseClass": "Success", "ResponseCode": "NoError", "Attachments": [ {
                    "__type": "FileAttachment:#Exchange", "AttachmentId": { "Id": it.attachment_id(x) }, "Name": x.name, "ContentType": x.content_type,
                    "Size": x.bytes.len(), "IsInline": x.is_inline, "Content": BASE64_STANDARD.encode(&x.bytes) } ] }),
                None => error_message(TYPE, "ErrorInvalidAttachmentId", "The attachment could not be found."),
            });
        }
        Ok(out)
    }

    /// Элемент письма в ответе. `IdOnly` — только `ItemId` и запрошенные свойства (как в
    /// `SyncFolderItems` с `AdditionalProperties`); именованные формы — их набор полей (§ 5b').
    fn message_json(&self, it: &FakeItem, shape: Shape, fields: &[String], max_body: u64) -> Result<Value, String> {
        let mut o = Map::new();
        o.insert("__type".into(), json!("Message:#Exchange"));
        o.insert("ItemId".into(), json!({ "Id": it.id, "ChangeKey": it.change_key() }));
        let mut keys: Vec<&str> = match shape {
            Shape::IdOnly => vec![],
            Shape::MailListItem => LIST_KEYS.to_vec(),
            Shape::ItemPart => [LIST_KEYS, FULL_KEYS, &["UniqueBody"]].concat(),
            Shape::NormalizedBody => [LIST_KEYS, FULL_KEYS, &["NormalizedBody"]].concat(),
            Shape::AllProperties => [LIST_KEYS, FULL_KEYS, &["BccRecipients", "Body"]].concat(),
        };
        for f in fields {
            let k = field_key(f)?;
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        for k in keys {
            if let Some(v) = self.member(it, k, max_body) {
                o.insert(k.into(), v);
            }
        }
        Ok(Value::Object(o))
    }

    /// Значение члена элемента письма; пустые списки (получатели, `ReplyTo`, вложения) опускаются (§ 5j).
    fn member(&self, it: &FakeItem, key: &str, max_body: u64) -> Option<Value> {
        // Тело усекается до `MaximumBodySize` символов (§ 5b').
        let body = || {
            let max = usize::try_from(max_body).unwrap_or(usize::MAX);
            let truncated = it.body_html.chars().count() > max;
            let value: String = it.body_html.chars().take(max).collect();
            json!({ "BodyType": "HTML", "Value": value, "IsTruncated": truncated })
        };
        let list = |l: &[Addr]| (!l.is_empty()).then(|| Value::Array(l.iter().map(|a| mailbox_json(a, "OneOff")).collect()));
        Some(match key {
            "ParentFolderId" => json!({ "Id": it.folder, "ChangeKey": self.folder_by_id(&it.folder).map(FakeFolder::change_key) }),
            "ItemClass" => json!("IPM.Note"),
            "Subject" => json!(it.subject),
            "Sensitivity" => json!("Normal"),
            "DateTimeReceived" | "DateTimeSent" | "DateTimeCreated" | "LastModifiedTime" => json!(it.received),
            "Size" => json!(it.body_html.len() + it.attachments.iter().map(|a| a.bytes.len()).sum::<usize>()),
            "Importance" => json!(it.importance),
            "IsDraft" => json!(it.is_draft),
            "IsRead" => json!(it.is_read),
            "HasAttachments" => json!(it.attachments.iter().any(|a| !a.is_inline)),
            "DisplayTo" => json!(it.to.iter().map(|a| a.name.clone().unwrap_or_else(|| a.email.clone())).collect::<Vec<_>>().join("; ")),
            "ConversationId" => json!({ "Id": it.conversation }),
            "Preview" => json!(preview(&it.body_html)),
            "Flag" => json!({ "FlagStatus": it.flag }),
            "From" | "Sender" => json!({ "Mailbox": mailbox_json(&it.from, "Mailbox") }),
            "InternetMessageId" => json!(it.internet_message_id),
            "InstanceKey" => json!(BASE64_STANDARD.encode(it.id.as_bytes())),
            "ToRecipients" => list(&it.to)?,
            "CcRecipients" => list(&it.cc)?,
            "BccRecipients" => list(&it.bcc)?,
            "ReplyTo" => list(&it.reply_to)?,
            "Attachments" => {
                if it.attachments.is_empty() {
                    return None;
                }
                Value::Array(
                    it.attachments
                        .iter()
                        .map(|a| {
                            let mut v = json!({ "__type": "FileAttachment:#Exchange", "AttachmentId": { "Id": it.attachment_id(a) }, "Name": a.name, "ContentType": a.content_type, "Size": a.bytes.len(), "IsInline": a.is_inline });
                            if let Some(cid) = &a.content_id {
                                v["ContentId"] = json!(cid);
                            }
                            v
                        })
                        .collect(),
                )
            }
            "UniqueBody" | "NormalizedBody" | "Body" => body(),
            _ => return None,
        })
    }

    // --- письма: запись (§ 5e, § 5f) -----------------------------------------

    /// `UpdateItem`: `SetItemField`/`DeleteItemField` по полям письма. В ответе — тот же `Id`
    /// и усечённый 8-символьный `ChangeKey`, как у писем на настоящем сервере (§ 5e).
    fn update_item(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        const TYPE: &str = "UpdateItemResponseMessage:#Exchange";
        let overwrite = body.get("ConflictResolution").and_then(Value::as_str) == Some("AlwaysOverwrite");
        let disposition = body.get("MessageDisposition").and_then(Value::as_str);
        let changes = body.get("ItemChanges").and_then(Value::as_array).ok_or("нет ItemChanges")?;
        let mut out = Vec::new();
        for change in changes {
            if ty(change) != Some("ItemChange:#Exchange") {
                return Err(ABSTRACT_CLASS.into());
            }
            let (id, ck) = item_id(change.get("ItemId"))?;
            let mut updates: Vec<(String, Option<Value>)> = Vec::new();
            let mut refusal: Option<Value> = None;
            for u in change.get("Updates").and_then(Value::as_array).ok_or("нет Updates")? {
                let field = u.get("Path").filter(|p| ty(p) == Some("PropertyUri:#Exchange")).and_then(|p| p.get("FieldURI")).and_then(Value::as_str).ok_or(ABSTRACT_CLASS)?;
                match ty(u) {
                    Some("SetItemField:#Exchange") => {
                        let item = u.get("Item").and_then(Value::as_object).ok_or("SetItemField без Item")?;
                        if item.get("__type").and_then(Value::as_str) != Some("Message:#Exchange") {
                            return Err(ABSTRACT_CLASS.into());
                        }
                        let members: Vec<(&String, &Value)> = item.iter().filter(|(k, _)| k.as_str() != "__type").collect();
                        if members.len() != 1 {
                            refusal = Some(error_message(TYPE, "ErrorIncorrectUpdatePropertyCount", "An object within a change description must contain one and only one property to modify."));
                            break;
                        }
                        let key = writable_key(field)?;
                        if members[0].0 != key {
                            refusal = Some(error_message(TYPE, "ErrorUpdatePropertyMismatch", "Property for update does not match property in object."));
                            break;
                        }
                        updates.push((key.to_string(), Some(members[0].1.clone())));
                    }
                    Some("DeleteItemField:#Exchange") => {
                        let key = writable_key(field)?;
                        if !key.ends_with("Recipients") {
                            return Err(format!("фейк: DeleteItemField {field} не поддерживается"));
                        }
                        updates.push((key.to_string(), None));
                    }
                    None => return Err(ABSTRACT_CLASS.into()),
                    Some(other) => return Err(format!("фейк: изменение {other} не поддерживается")),
                }
            }
            if let Some(r) = refusal {
                out.push(r);
                continue;
            }
            let Some(idx) = self.items.iter().position(|i| i.id == id) else {
                out.push(not_found(TYPE));
                continue;
            };
            // § 5e: для писем `MessageDisposition` обязателен.
            if disposition.is_none() {
                out.push(error_message(TYPE, "ErrorMessageDispositionRequired", "MessageDisposition attribute is required."));
                continue;
            }
            if !overwrite && ck.as_deref().is_some_and(|c| !self.items[idx].key_matches(c)) {
                out.push(error_message(TYPE, "ErrorIrresolvableConflict", "The send or update operation could not be performed because the change key passed in the request does not match the current change key for the item."));
                continue;
            }
            let it = &mut self.items[idx];
            let mut content = false;
            for (key, value) in updates {
                content |= key != "IsRead";
                apply_member(it, &key, value)?;
            }
            it.ck += 1;
            if content {
                it.rev += 1;
            }
            out.push(json!({ "__type": TYPE, "ResponseCode": "NoError", "ResponseClass": "Success",
                "Items": [ { "__type": "Message:#Exchange", "ItemId": { "__type": "ItemId:#Exchange", "Id": it.id, "ChangeKey": it.short_change_key() } } ],
                "ConflictResults": { "Count": 0 } }));
        }
        Ok(out)
    }

    /// Перенос письма: новый идентификатор (и вместе с ним идентификаторы вложений, § 5).
    fn relocate(&mut self, idx: usize, folder_id: &str) -> String {
        let new_id = self.new_item_id();
        let it = &mut self.items[idx];
        it.id = new_id.clone();
        it.folder = folder_id.to_string();
        it.ck += 1;
        new_id
    }

    /// `MoveItem` (§ 5e): в ответе `Items[0].ItemId` с новым идентификатором (форма на сервере
    /// не проверялась, построена по тому, что разбирает `protocol::written_item_id`).
    fn move_item(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        const TYPE: &str = "ItemInfoResponseMessage:#Exchange";
        let target = self.resolve_target_folder(body.get("ToFolderId"))?;
        let return_ids = body.get("ReturnNewItemIds").and_then(Value::as_bool).unwrap_or(false);
        let mut out = Vec::new();
        for (id, _) in item_ids(body.get("ItemIds"))? {
            let Some(target) = target.clone() else {
                out.push(error_message(TYPE, "ErrorToFolderNotFound", "The destination folder could not be found."));
                continue;
            };
            let Some(idx) = self.items.iter().position(|i| i.id == id) else {
                out.push(not_found(TYPE));
                continue;
            };
            self.relocate(idx, &target);
            let it = &self.items[idx];
            let items = if return_ids { json!([ { "__type": "Message:#Exchange", "ItemId": { "__type": "ItemId:#Exchange", "Id": it.id, "ChangeKey": it.change_key() } } ]) } else { json!([]) };
            out.push(json!({ "__type": TYPE, "ResponseCode": "NoError", "ResponseClass": "Success", "Items": items }));
        }
        Ok(out)
    }

    /// `DeleteItem` (§ 5e, § 5i): `HardDelete` убирает письмо, `MoveToDeletedItems` переносит в
    /// «Удалённые». Сообщение ответа — `__type`, `ResponseCode`, `ResponseClass`; новый
    /// идентификатор при `ReturnMovedItemIds` с сервера не снят и отдаётся в `MovedItemId` —
    /// одном из мест, где его ищет `protocol::written_item_id`.
    fn delete_item(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        const TYPE: &str = "DeleteItemResponseMessage:#Exchange";
        let delete_type = body.get("DeleteType").and_then(Value::as_str).ok_or("нет DeleteType")?;
        let return_moved = body.get("ReturnMovedItemIds").and_then(Value::as_bool).unwrap_or(false);
        let mut out = Vec::new();
        for (id, _) in item_ids(body.get("ItemIds"))? {
            let Some(idx) = self.items.iter().position(|i| i.id == id) else {
                out.push(not_found(TYPE));
                continue;
            };
            let mut msg = json!({ "__type": TYPE, "ResponseCode": "NoError", "ResponseClass": "Success" });
            match delete_type {
                "MoveToDeletedItems" if self.items[idx].folder != DELETED => {
                    self.relocate(idx, DELETED);
                    if return_moved {
                        let it = &self.items[idx];
                        msg["MovedItemId"] = json!({ "Id": it.id, "ChangeKey": it.change_key() });
                    }
                }
                "MoveToDeletedItems" | "SoftDelete" | "HardDelete" => {
                    self.items.remove(idx);
                }
                other => return Err(format!("фейк: DeleteType {other} не поддерживается")),
            }
            out.push(msg);
        }
        Ok(out)
    }

    /// `CreateItem` нового письма-черновика (§ 5f): `MessageDisposition: SaveOnly`, папка из
    /// `SavedItemFolderId`. Ответы и пересылка (`ReplyToItem` и т. п.) фейком не поддержаны.
    fn create_item(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        const TYPE: &str = "ItemInfoResponseMessage:#Exchange";
        if body.get("MessageDisposition").and_then(Value::as_str) != Some("SaveOnly") {
            return Ok(vec![error_message(TYPE, "ErrorMessageDispositionRequired", "фейк: для писем поддержан только MessageDisposition SaveOnly")]);
        }
        let folder_id = self.resolve_target_folder(body.get("SavedItemFolderId"))?.ok_or("нет папки SavedItemFolderId")?;
        let items = body.get("Items").and_then(Value::as_array).ok_or("нет Items")?;
        let mut created = Vec::new();
        for item in items {
            match ty(item) {
                Some("Message:#Exchange") => {}
                None => return Err(ABSTRACT_CLASS.into()),
                Some(other) => return Err(format!("фейк: CreateItem {other} не поддерживается")),
            }
            let id = self.new_item_id();
            let n = self.next_item;
            let mut it = FakeItem {
                id: id.clone(),
                folder: folder_id.clone(),
                ck: 1,
                rev: 1,
                subject: String::new(),
                from: owner(),
                to: vec![],
                cc: vec![],
                bcc: vec![],
                reply_to: vec![],
                body_html: String::new(),
                received: received(23, 10),
                is_read: true,
                flag: "NotFlagged".into(),
                is_draft: true,
                importance: "Normal".into(),
                attachments: vec![],
                internet_message_id: format!("<draft-{n}@aiowa.test>"),
                conversation: format!("AAQkConv-{n:04}"),
            };
            for (k, v) in item.as_object().expect("объект").iter().filter(|(k, _)| k.as_str() != "__type") {
                apply_member(&mut it, k, Some(v.clone()))?;
            }
            created.push(json!({ "__type": TYPE, "ResponseCode": "NoError", "ResponseClass": "Success",
                "Items": [ { "__type": "Message:#Exchange", "ItemId": { "__type": "ItemId:#Exchange", "Id": it.id, "ChangeKey": it.change_key() } } ] }));
            self.items.push(it);
        }
        Ok(created)
    }

    /// `CreateAttachment` (§ 5f): по сообщению на файл; `AttachmentId` с `RootItemId` и новым
    /// `RootItemChangeKey` черновика.
    fn create_attachment(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        const TYPE: &str = "AttachmentInfoResponseMessage:#Exchange";
        let (id, _) = item_id(body.get("ParentItemId"))?;
        let atts = body.get("Attachments").and_then(Value::as_array).ok_or("нет Attachments")?;
        let Some(idx) = self.items.iter().position(|i| i.id == id) else {
            return Ok(atts.iter().map(|_| not_found(TYPE)).collect());
        };
        let mut out = Vec::new();
        for a in atts {
            match ty(a) {
                Some("FileAttachment:#Exchange") => {}
                None => return Err(ABSTRACT_CLASS.into()),
                Some(other) => return Err(format!("фейк: вложение {other} не поддерживается")),
            }
            let content = a.get("Content").and_then(Value::as_str).ok_or("FileAttachment без Content")?;
            let bytes = BASE64_STANDARD.decode(content).map_err(|e| format!("Content не base64: {e}"))?;
            let it = &mut self.items[idx];
            let seq = it.attachments.iter().map(|x| x.seq).max().unwrap_or(0) + 1;
            let att = FakeAttachment {
                seq,
                name: a.get("Name").and_then(Value::as_str).unwrap_or("").to_string(),
                content_type: a.get("ContentType").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string(),
                bytes,
                is_inline: a.get("IsInline").and_then(Value::as_bool).unwrap_or(false),
                content_id: None,
            };
            it.ck += 1;
            it.rev += 1;
            out.push(json!({ "__type": TYPE, "ResponseCode": "NoError", "ResponseClass": "Success",
                "Attachments": [ { "__type": "FileAttachment:#Exchange",
                    "AttachmentId": { "RootItemChangeKey": it.change_key(), "RootItemId": it.id, "Id": it.attachment_id(&att) },
                    "LastModifiedTime": it.received } ] }));
            it.attachments.push(att);
        }
        Ok(out)
    }

    /// `SendItem` (§ 5f): ключ черновика должен быть текущим (от последнего `CreateAttachment`);
    /// устаревший — отказ по письму (`ErrorItemNotFound`, как ожидает документ). Копия — в
    /// `SavedItemFolderId`, письмо себе приходит во «Входящие». Сообщение ответа без `__type`.
    fn send_item(&mut self, body: &Value) -> Result<Vec<Value>, String> {
        let save = body.get("SaveItemToFolder").and_then(Value::as_bool).unwrap_or(false);
        let save_to = if save { Some(self.resolve_target_folder(body.get("SavedItemFolderId"))?.ok_or("нет папки SavedItemFolderId")?) } else { None };
        let mut out = Vec::new();
        for (id, ck) in item_ids(body.get("ItemIds"))? {
            let Some(idx) = self.items.iter().position(|i| i.id == id && ck.as_deref().is_none_or(|c| i.key_matches(c))) else {
                out.push(json!({ "MessageText": "The specified object was not found in the store.", "ResponseCode": "ErrorItemNotFound", "ResponseClass": "Error" }));
                continue;
            };
            let it = self.items[idx].clone();
            self.sent.push(SentMail {
                subject: it.subject.clone(),
                to: it.to.clone(),
                cc: it.cc.clone(),
                bcc: it.bcc.clone(),
                body_html: it.body_html.clone(),
                attachments: it.attachments.iter().map(|a| (a.name.clone(), a.bytes.clone())).collect(),
            });
            let to_self = it.to.iter().chain(&it.cc).chain(&it.bcc).any(|a| a.email.eq_ignore_ascii_case(OWNER));
            match &save_to {
                Some(folder_id) => {
                    self.relocate(idx, folder_id);
                    let s = &mut self.items[idx];
                    s.is_draft = false;
                    s.rev += 1;
                }
                None => {
                    self.items.remove(idx);
                }
            }
            if to_self {
                let mut copy = it;
                copy.id = self.new_item_id();
                copy.folder = INBOX.into();
                copy.is_draft = false;
                copy.is_read = false;
                copy.ck = 1;
                copy.rev = 1;
                self.items.push(copy);
            }
            out.push(json!({ "ResponseCode": "NoError", "ResponseClass": "Success" }));
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    IdOnly,
    MailListItem,
    ItemPart,
    NormalizedBody,
    AllProperties,
}

/// Члены формы `MailListItem` (§ 5b'): без тел и получателей.
const LIST_KEYS: &[&str] = &[
    "ParentFolderId", "ItemClass", "Subject", "Sensitivity", "DateTimeReceived", "Size", "Importance", "IsDraft", "IsRead", "HasAttachments", "DateTimeSent", "DateTimeCreated", "LastModifiedTime", "DisplayTo",
    "ConversationId", "Preview", "Flag", "From", "Sender", "InternetMessageId",
];
/// Что добавляют формы с телом (`ItemPart` и др.): получатели и вложения.
const FULL_KEYS: &[&str] = &["ToRecipients", "CcRecipients", "Attachments"];

/// Член элемента ответа по `FieldURI` запроса. Имена — со стандартными префиксами EWS (§ 5b');
/// незнакомое имя сервер отвергает HTTP 500.
fn field_key(field: &str) -> Result<&'static str, String> {
    Ok(match field {
        "item:Subject" => "Subject",
        "item:DateTimeReceived" => "DateTimeReceived",
        "item:DateTimeSent" => "DateTimeSent",
        "item:DateTimeCreated" => "DateTimeCreated",
        "item:LastModifiedTime" => "LastModifiedTime",
        "message:From" => "From",
        "message:Sender" => "Sender",
        "item:DisplayTo" => "DisplayTo",
        "message:IsRead" => "IsRead",
        "item:IsDraft" => "IsDraft",
        "item:HasAttachments" => "HasAttachments",
        "item:Importance" => "Importance",
        "item:Size" => "Size",
        "item:ItemClass" => "ItemClass",
        "item:ConversationId" => "ConversationId",
        "message:InternetMessageId" => "InternetMessageId",
        "item:Preview" => "Preview",
        "item:Flag" => "Flag",
        "item:ParentFolderId" => "ParentFolderId",
        "message:ReplyTo" => "ReplyTo",
        "message:ToRecipients" => "ToRecipients",
        "message:CcRecipients" => "CcRecipients",
        "message:BccRecipients" => "BccRecipients",
        "item:Attachments" => "Attachments",
        "item:Body" => "Body",
        "item:UniqueBody" => "UniqueBody",
        _ => return Err(format!("Invalid value '{field}' for enum PropertyUriEnum")),
    })
}

/// Поля, которые фейк умеет записывать (`UpdateItem`, члены `CreateItem`).
fn writable_key(field: &str) -> Result<&'static str, String> {
    let key = field_key(field)?;
    if matches!(key, "IsRead" | "Flag" | "Subject" | "Body" | "Importance" | "ToRecipients" | "CcRecipients" | "BccRecipients") {
        Ok(key)
    } else {
        Err(format!("фейк: запись поля {field} не поддерживается"))
    }
}

/// Записывает член `Message:#Exchange` в письмо (`None` — `DeleteItemField`).
fn apply_member(it: &mut FakeItem, key: &str, value: Option<Value>) -> Result<(), String> {
    let v = value.unwrap_or(Value::Null);
    match key {
        "IsRead" => it.is_read = v.as_bool().ok_or("IsRead не булево")?,
        "Flag" => {
            if ty(&v) != Some("FlagType:#Exchange") {
                return Err(ABSTRACT_CLASS.into());
            }
            it.flag = v.get("FlagStatus").and_then(Value::as_str).ok_or("Flag без FlagStatus")?.to_string();
        }
        "Subject" => it.subject = v.as_str().ok_or("Subject не строка")?.to_string(),
        "Importance" => it.importance = v.as_str().ok_or("Importance не строка")?.to_string(),
        "Body" => {
            if ty(&v) != Some("BodyContentType:#Exchange") || v.get("BodyType").and_then(Value::as_str) != Some("HTML") {
                return Err("фейк: тело только BodyContentType HTML".into());
            }
            it.body_html = v.get("Value").and_then(Value::as_str).ok_or("Body без Value")?.to_string();
        }
        "ToRecipients" | "CcRecipients" | "BccRecipients" => {
            let list = match &v {
                Value::Null => vec![],
                Value::Array(a) => a.iter().map(parse_recipient).collect::<Result<Vec<_>, _>>()?,
                _ => return Err(format!("{key} не массив")),
            };
            match key {
                "ToRecipients" => it.to = list,
                "CcRecipients" => it.cc = list,
                _ => it.bcc = list,
            }
        }
        other => return Err(format!("фейк: член {other} письма не поддерживается")),
    }
    Ok(())
}

/// Получатель запроса: `EmailAddress:#Exchange` с `EmailAddress` и `RoutingType` (§ 5f).
fn parse_recipient(v: &Value) -> Result<Addr, String> {
    if ty(v) != Some("EmailAddress:#Exchange") {
        return Err(format!("получатель не EmailAddress:#Exchange: {v}"));
    }
    let email = v.get("EmailAddress").and_then(Value::as_str).ok_or("получатель без EmailAddress")?;
    Ok(Addr { name: v.get("Name").and_then(Value::as_str).map(String::from), email: email.to_string() })
}

fn mailbox_json(a: &Addr, mailbox_type: &str) -> Value {
    json!({ "Name": a.name.clone().unwrap_or_else(|| a.email.clone()), "EmailAddress": a.email, "RoutingType": "SMTP", "MailboxType": mailbox_type })
}

/// Превью письма: текст без тегов, до 255 символов.
fn preview(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.chars().take(255).collect()
}

fn ty(v: &Value) -> Option<&str> {
    v.get("__type").and_then(Value::as_str)
}

/// `__type` первым ключом у каждого объекта, где он есть (§ 5b); `Err` — путь до нарушения.
fn type_first_everywhere(v: &Value, path: &str) -> Result<(), String> {
    match v {
        Value::Object(o) => {
            if o.contains_key("__type") && o.keys().next().map(String::as_str) != Some("__type") {
                return Err(path.to_string());
            }
            o.iter().try_for_each(|(k, x)| type_first_everywhere(x, &format!("{path}.{k}")))
        }
        Value::Array(a) => a.iter().enumerate().try_for_each(|(i, x)| type_first_everywhere(x, &format!("{path}[{i}]"))),
        _ => Ok(()),
    }
}

/// `AdditionalProperties` формы: `[{ "__type": "PropertyUri:#Exchange", "FieldURI": … }]`.
fn additional_properties(shape: &Value) -> Result<Vec<String>, String> {
    let Some(list) = shape.get("AdditionalProperties") else { return Ok(vec![]) };
    list.as_array()
        .ok_or("AdditionalProperties не массив")?
        .iter()
        .map(|p| match ty(p) {
            Some("PropertyUri:#Exchange") => p.get("FieldURI").and_then(Value::as_str).map(String::from).ok_or_else(|| "PropertyUri без FieldURI".to_string()),
            None => Err(ABSTRACT_CLASS.to_string()),
            Some(other) => Err(format!("фейк: путь {other} не поддерживается")),
        })
        .collect()
}

/// Один `ItemId:#Exchange`: (Id, ChangeKey).
fn item_id(v: Option<&Value>) -> Result<(String, Option<String>), String> {
    let v = v.ok_or("нет ItemId")?;
    match ty(v) {
        Some("ItemId:#Exchange") => {}
        None => return Err(ABSTRACT_CLASS.into()),
        Some(other) => return Err(format!("фейк: идентификатор {other} не поддерживается")),
    }
    let id = v.get("Id").and_then(Value::as_str).ok_or("ItemId без Id")?;
    Ok((id.to_string(), v.get("ChangeKey").and_then(Value::as_str).map(String::from)))
}

fn item_ids(v: Option<&Value>) -> Result<Vec<(String, Option<String>)>, String> {
    v.and_then(Value::as_array).ok_or("нет ItemIds")?.iter().map(|x| item_id(Some(x))).collect()
}

fn error_message(ty: &str, code: &str, text: &str) -> Value {
    json!({ "__type": ty, "MessageText": text, "ResponseCode": code, "ResponseClass": "Error" })
}

fn not_found(ty: &str) -> Value {
    error_message(ty, "ErrorItemNotFound", "The specified object was not found in the store.")
}

/// Ошибка уровня сервиса: HTTP 500 с `Body.FaultMessage` (так её разбирает `OwaTransport::call_once`).
fn fault(text: &str) -> Response {
    Response::json(500, &json!({ "Body": { "ErrorCode": 500, "FaultMessage": text } }))
}

/// Состояние синхронизации: непрозрачная строка base64 с меткой (`H` — иерархия, `F:<папка>`).
fn encode_state(tag: &str, known: &Value) -> String {
    BASE64_STANDARD.encode(serde_json::to_vec(&json!({ "t": tag, "k": known })).expect("JSON"))
}

fn decode_state(s: &str, tag: &str) -> Option<Value> {
    let v: Value = serde_json::from_slice(&BASE64_STANDARD.decode(s).ok()?).ok()?;
    if v.get("t").and_then(Value::as_str) != Some(tag) {
        return None;
    }
    v.get("k").cloned()
}

// ---------------------------------------------------------------------------
// HTTP/1.1: запрос целиком (заголовки и тело по `Content-Length`), ответ и закрытие соединения.

struct Request {
    method: String,
    /// Путь в нижнем регистре, без query.
    path: String,
    query: Vec<(String, String)>,
    /// Имена в нижнем регистре.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn query(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn cookie(&self, name: &str) -> Option<String> {
        self.headers.iter().filter(|(k, _)| k == "cookie").flat_map(|(_, v)| v.split(';')).find_map(|c| {
            let (k, v) = c.trim().split_once('=')?;
            (k == name).then(|| v.to_string())
        })
    }
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Тело одним чанком `Transfer-Encoding: chunked`, без `Content-Length`.
    chunked: bool,
}

impl Response {
    fn html(status: u16, body: &str) -> Self {
        Self { status, headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())], body: body.as_bytes().to_vec(), chunked: false }
    }

    fn json(status: u16, v: &Value) -> Self {
        Self { status, headers: vec![("Content-Type".into(), "application/json; charset=utf-8".into())], body: serde_json::to_vec(v).expect("JSON"), chunked: false }
    }

    fn redirect(status: u16, location: &str) -> Self {
        Self { status, headers: vec![("Location".into(), location.into())], body: vec![], chunked: false }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        415 => "Unsupported Media Type",
        440 => "Login Timeout",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or("").split(' ');
    let method = start.next().unwrap_or("").to_string();
    let target = start.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let len = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let query = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    Ok(Some(Request { method, path: path.to_ascii_lowercase(), query, headers, body }))
}

async fn write_response(stream: &mut TcpStream, r: Response) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", r.status, reason(r.status));
    for (k, v) in &r.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if r.chunked {
        // Один чанк `<длина hex>\r\n<байты>\r\n` и завершающий нулевой; у пустого тела — только он.
        head.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).await?;
        if !r.body.is_empty() {
            stream.write_all(format!("{:X}\r\n", r.body.len()).as_bytes()).await?;
            stream.write_all(&r.body).await?;
            stream.write_all(b"\r\n").await?;
        }
        stream.write_all(b"0\r\n\r\n").await?;
    } else {
        head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", r.body.len()));
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&r.body).await?;
    }
    stream.flush().await?;
    stream.shutdown().await
}

/// Запущенный фейковый сервер; останавливается вместе со значением.
pub struct FakeOwa {
    addr: SocketAddr,
    state: Arc<Mutex<Mailbox>>,
    accept: JoinHandle<()>,
}

impl FakeOwa {
    pub async fn start() -> Self {
        Self::start_with(Mailbox::seeded()).await
    }

    pub async fn start_with(mailbox: Mailbox) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("порт на 127.0.0.1");
        let addr = listener.local_addr().expect("адрес сервера");
        let state = Arc::new(Mutex::new(mailbox));
        let shared = state.clone();
        let accept = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Ok(Some(req)) = read_request(&mut stream).await {
                        // Ожидание — без блокировки состояния: остальные запросы идут своим ходом.
                        let wait = shared.lock().expect("состояние фейка").wait_for(&req);
                        match wait {
                            Some(Wait::Hold(h)) => {
                                h.arrived.notify_one();
                                h.release.notified().await;
                            }
                            Some(Wait::Gather(b)) => {
                                b.wait().await;
                            }
                            None => {}
                        }
                        let resp = shared.lock().expect("состояние фейка").handle(&req);
                        let _ = write_response(&mut stream, resp).await;
                    }
                });
            }
        });
        Self { addr, state, accept }
    }

    /// `host:port` для `Credentials::host`.
    pub fn host(&self) -> String {
        self.addr.to_string()
    }

    /// Значение `OwaConfig::base_url`.
    pub fn base_url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    pub fn state(&self) -> MutexGuard<'_, Mailbox> {
        self.state.lock().expect("состояние фейка")
    }
}

impl Drop for FakeOwa {
    fn drop(&mut self) {
        self.accept.abort();
    }
}
