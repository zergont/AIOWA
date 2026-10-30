//! Справочник организации: поиск адресатов (`directory_search`), карточка контакта
//! (`contact_card`) и участники списка рассылки (`directory_expand`).
//!
//! Окно спрашивает поиск дважды: сначала только кэш справочника в локальной базе
//! (`directory_people`, `cache_only`) — ответ мгновенный, — затем сервер (`FindPeople` по папке
//! `directory`; если сервер отверг операцию — `ResolveNames`), и найденное ложится в кэш. Сервер
//! спрашивается не чаще раза в 10 минут на одинаковый запрос (кэш запросов в памяти, `QueryCache`,
//! помнит адреса записей ответа): повтор отвечается теми же записями из базы, а не всей таблицей,
//! где лежат и давно удалённые из справочника. Запрос, который продолжает уже полностью полученный
//! более короткий (одно слово: «пет» → «петр»), отвечается записями того ответа, подходящими к
//! новому тексту, — если сервер нашёл их по тем же полям, что ищет кэш. Одинаковые запросы,
//! пришедшие одновременно, ждут первый под замком по ключу запроса и получают готовый ответ.
//!
//! Сеть — только через транспорт процесса (`SyncState::transport`), без новых входов, если сессия
//! уже есть. Если сессии нет, вход идёт в отдельной задаче (одной на модуль): таймаут команды не
//! обрывает вход на полпути — иначе на сервере осталась бы сессия, которую никто не разлогинит, —
//! а готовый транспорт достанется следующему запросу. Каждая команда ждёт сервер не дольше
//! 10 с (`SERVER_TIMEOUT`). Отказ, не связанный с самим запросом (нет связи, вход, таймаут),
//! на 30 с останавливает походы на сервер по любым запросам: окно получает кэш и тот же текст
//! ошибки, а не ждёт таймаут на каждую букву. Ошибка мёртвой сессии сбрасывает транспорт
//! (`SyncState::invalidate_on`).
//!
//! В журнал не пишутся ни запросы, ни имена, ни адреса — только длины, счётчики, коды ошибок
//! и длительности.

use crate::compose::looks_like_email;
use crate::ondemand::{describe_transport, timed_out, KeyedLocks};
use crate::sync::SyncState;
use crate::{err, AppState, CmdResult};
use aiowa_core::{AccountId, DirectoryPerson, PersonKind};
use aiowa_store::directory::directory_person_matches;
use aiowa_store::{ContactLocalStats, Store};
use aiowa_transport::{MailTransport, ResolvedPerson, TransportError};
use aiowa_transport_owa::OwaTransport;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::State;
use tracing::{info, warn};

/// Сколько команда ждёт сервер (вход, если нужен, и сам запрос).
const SERVER_TIMEOUT: Duration = Duration::from_secs(10);
/// Сколько ждать участников списка рассылки: большой список отдаётся дольше поиска.
const EXPAND_TIMEOUT: Duration = Duration::from_secs(20);
/// Запрос короче — пустой ответ без кэша и сервера.
const MIN_QUERY_CHARS: usize = 2;
/// Длиннее запрос обрезается: это уже не начало имени, а вставленный текст.
const MAX_QUERY_CHARS: usize = 128;
/// Подсказок по умолчанию и максимум.
const SEARCH_DEFAULT: usize = 10;
const SEARCH_MAX: usize = 50;
/// Сколько записей просить у `FindPeople`. Не зависит от `limit` окна, чтобы кэш запросов не
/// зависел от него же: ответ короче — полный, и продолжения запроса можно искать в кэше.
const SERVER_MAX: usize = 25;
/// Сколько совпадений отдаёт `ResolveNames` (ограничение сервера).
const RESOLVE_CAP: usize = 100;
/// Сколько не спрашивать сервер повторно по тому же запросу после удачного ответа.
const QUERY_TTL: Duration = Duration::from_secs(10 * 60);
/// Сколько не спрашивать сервер после отказа: по тому же запросу или, если отказ общий (нет
/// связи, вход, таймаут), по любому.
const FAILURE_TTL: Duration = Duration::from_secs(30);
/// Сколько после отказа сервера в `FindPeople` искать сразу через `ResolveNames`.
const FIND_PEOPLE_RETRY: Duration = Duration::from_secs(30 * 60);
/// Записей кэша запросов, после которых он чистится от устаревших (а затем от самых старых).
const QUERY_CACHE_MAX: usize = 512;
/// Шаг ожидания чужого входа на сервер.
const LOGIN_POLL: Duration = Duration::from_millis(100);

// --- DTO ------------------------------------------------------------------------------------

/// Вид записи для окна. Свои строки, а не `PersonKind::key`: договор с окном не должен меняться
/// вместе с ключами базы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KindDto {
    Mailbox,
    List,
    Contact,
    Room,
    Other,
}

impl From<PersonKind> for KindDto {
    fn from(k: PersonKind) -> Self {
        match k {
            PersonKind::Mailbox => KindDto::Mailbox,
            PersonKind::DistributionList => KindDto::List,
            PersonKind::Contact => KindDto::Contact,
            PersonKind::Room => KindDto::Room,
            PersonKind::Other => KindDto::Other,
        }
    }
}

/// Запись справочника для окна.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DirectoryPersonDto {
    name: Option<String>,
    email: Option<String>,
    kind: KindDto,
    title: Option<String>,
    department: Option<String>,
    company: Option<String>,
    office: Option<String>,
    business_phone: Option<String>,
    mobile_phone: Option<String>,
    /// Руководитель и подчинённые (§ 5l'): только у записи с полными данными; у записи поиска —
    /// `None` и пустой список.
    manager: Option<PersonRefDto>,
    direct_reports: Vec<PersonRefDto>,
    address: Option<String>,
    notes: Option<String>,
}

/// Человек по ссылке из записи справочника: имя и адрес для перехода к его карточке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PersonRefDto {
    name: Option<String>,
    email: Option<String>,
}

impl From<aiowa_core::Participant> for PersonRefDto {
    fn from(p: aiowa_core::Participant) -> Self {
        Self { name: p.name, email: p.email.map(|e| e.trim().to_string()) }
    }
}

impl From<DirectoryPerson> for DirectoryPersonDto {
    fn from(p: DirectoryPerson) -> Self {
        Self {
            name: p.display_name,
            email: p.email.map(|e| e.trim().to_string()),
            kind: p.kind.into(),
            title: p.title,
            department: p.department,
            company: p.company,
            office: p.office,
            business_phone: p.business_phone,
            mobile_phone: p.mobile_phone,
            manager: p.manager.map(PersonRefDto::from),
            direct_reports: p.direct_reports.unwrap_or_default().into_iter().map(PersonRefDto::from).collect(),
            address: p.address,
            notes: p.notes,
        }
    }
}

/// Ответ поиска: записи, взяты ли они из кэша (сервер не спрашивался или не ответил) и текст
/// отказа сервера — кэш при отказе всё равно отдаётся.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DirectorySearchDto {
    people: Vec<DirectoryPersonDto>,
    from_cache: bool,
    error: Option<String>,
}

impl DirectorySearchDto {
    fn new(people: Vec<DirectoryPerson>, from_cache: bool, error: Option<String>) -> Self {
        Self { people: people.into_iter().map(DirectoryPersonDto::from).collect(), from_cache, error }
    }
}

/// Переписка с адресом по локальной базе.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct ContactLocalDto {
    messages_from: i64,
    messages_to: i64,
    last_ms: Option<i64>,
}

impl From<ContactLocalStats> for ContactLocalDto {
    fn from(s: ContactLocalStats) -> Self {
        Self { messages_from: s.messages_from, messages_to: s.messages_to, last_ms: s.last_ms }
    }
}

/// Карточка контакта: запись справочника (`None` — не нашлась), из кэша ли она, отказ сервера
/// и переписка по локальной базе.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContactCardDto {
    person: Option<DirectoryPersonDto>,
    from_cache: bool,
    error: Option<String>,
    local: ContactLocalDto,
    /// Фото из справочника как `data:`-адрес (CSP окна не меняется); `None` — фото нет или не запрашивалось.
    photo_url: Option<String>,
}

/// Срок годности фото в кэше: после него карточка или заголовок письма перечитают его с сервера.
const PHOTO_TTL_MS: i64 = 7 * 24 * 3600 * 1000;

/// `data:`-адрес картинки для окна; тип — по сигнатуре (JPEG, PNG, GIF, иначе JPEG).
pub(crate) fn photo_data_url(bytes: &[u8]) -> String {
    use base64::prelude::*;
    let mime = match aiowa_transport_owa::protocol::image_kind(bytes) {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        _ => "image/jpeg",
    };
    format!("data:{mime};base64,{}", BASE64_STANDARD.encode(bytes))
}

/// Фото из кэша как `data:`-адрес, если оно есть.
fn cached_photo_url(store: &Store, account: AccountId, key: &str) -> Option<String> {
    store.directory_photo(account, key).ok().flatten().and_then(|(photo, _)| photo).map(|b| photo_data_url(&b))
}

/// Фото из найденных записей для адреса `key` (по основному адресу записи).
fn photo_for(found: &[ResolvedPerson], key: &str) -> Option<Vec<u8>> {
    found.iter().find(|r| r.person.email_key().as_deref() == Some(key)).and_then(|r| r.photo.clone()).or_else(|| found.iter().find_map(|r| r.photo.clone()))
}

// --- чистые функции (без базы и сети) --------------------------------------------------------

/// Запрос к справочнику: текст для сервера и ключ кэша запросов и замка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NormalizedQuery {
    /// Без управляющих символов, кавычек и разделителей списка по краям, пробелы схлопнуты,
    /// не длиннее `MAX_QUERY_CHARS`.
    pub(crate) text: String,
    /// `text` в нижнем регистре с ё → е (как сравнивает кэш справочника).
    pub(crate) key: String,
}

/// Нормализация запроса; `None` — искать нечего (короче `MIN_QUERY_CHARS` или ни одной буквы
/// и цифры).
pub(crate) fn normalize_query(raw: &str) -> Option<NormalizedQuery> {
    let edge = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '«' | '»' | '<' | '>' | ',' | ';');
    let clean: String = raw.chars().map(|c| if c.is_whitespace() { ' ' } else { c }).filter(|c| !c.is_control()).collect();
    let joined = clean.trim_matches(edge).split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = joined.chars().take(MAX_QUERY_CHARS).collect();
    let text = cut.trim_end_matches(edge).to_string();
    if text.chars().count() < MIN_QUERY_CHARS || !text.chars().any(char::is_alphanumeric) {
        return None;
    }
    let key = aiowa_store::text::norm_lower(&text);
    Some(NormalizedQuery { text, key })
}

/// Запомненный ответ сервера: адреса его записей (ключи кэша справочника) в порядке сервера.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub(crate) keys: Arc<[String]>,
    /// Ответ полный (короче лимита запроса): кроме этих записей, сервер ничего не нашёл.
    pub(crate) complete: bool,
}

impl Answer {
    pub(crate) fn new(keys: Vec<String>, complete: bool) -> Self {
        Self { keys: keys.into(), complete }
    }
}

/// Решение по запросу.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Ответить без сервера по его недавнему ответу. `covered` — ответ дан более короткому
    /// запросу, который этот продолжает: из его записей берутся подходящие к новому тексту.
    Cached { answer: Answer, covered: bool },
    /// Ответить кэшем и этим текстом ошибки: сервер недавно отказал.
    Failed(String),
    /// Спросить сервер.
    Server,
}

#[derive(Debug, Clone)]
enum Outcome {
    /// Сервер ответил. `coverable` — продолжения этого запроса (одно слово) можно отвечать по
    /// этому ответу: он полный, непустой и каждую его запись поиск по кэшу находит по тому же
    /// запросу (сервер не нашёл её по полям, которых в кэше нет: имя и фамилия отдельно, псевдоним).
    Done { answer: Answer, coverable: bool },
    Failed(String),
}

#[derive(Debug, Clone)]
struct QueryRecord {
    at: Instant,
    outcome: Outcome,
}

/// Кэш запросов к серверу: когда и чем кончился последний поход по ключу, плюс общий отказ.
#[derive(Debug, Default)]
pub(crate) struct QueryCache {
    entries: HashMap<String, QueryRecord>,
    /// Отказ, не связанный с запросом (нет связи, вход, таймаут): пока он не истёк, сервер не
    /// спрашивается ни по какому ключу.
    outage: Option<(Instant, String)>,
}

impl QueryCache {
    /// Решение по ключу в момент `now`. `cover` — можно ли ответить без сервера, если ключ
    /// продолжает более короткий с покрывающим ответом (только для поиска: ключи из одного слова).
    pub(crate) fn plan(&self, key: &str, now: Instant, cover: bool) -> Plan {
        let fresh = |at: Instant, ttl: Duration| now.saturating_duration_since(at) < ttl;
        if let Some(QueryRecord { at, outcome: Outcome::Done { answer, .. } }) = self.entries.get(key)
            && fresh(*at, QUERY_TTL)
        {
            return Plan::Cached { answer: answer.clone(), covered: false };
        }
        if cover && !key.contains(' ') {
            // Самый длинный покрывающий запрос: у него самый узкий ответ.
            let covering = self
                .entries
                .iter()
                .filter(|(k, r)| matches!(r.outcome, Outcome::Done { coverable: true, .. }) && fresh(r.at, QUERY_TTL) && !k.contains(' ') && k.len() < key.len() && key.starts_with(k.as_str()))
                .max_by_key(|(k, _)| k.len());
            if let Some((_, QueryRecord { outcome: Outcome::Done { answer, .. }, .. })) = covering {
                return Plan::Cached { answer: answer.clone(), covered: true };
            }
        }
        if let Some((at, why)) = &self.outage
            && fresh(*at, FAILURE_TTL)
        {
            return Plan::Failed(why.clone());
        }
        if let Some(QueryRecord { at, outcome: Outcome::Failed(why) }) = self.entries.get(key)
            && fresh(*at, FAILURE_TTL)
        {
            return Plan::Failed(why.clone());
        }
        Plan::Server
    }

    /// Сервер ответил по ключу; `coverable` — см. `Outcome::Done`.
    pub(crate) fn record_done(&mut self, key: &str, now: Instant, answer: Answer, coverable: bool) {
        self.outage = None;
        self.insert(key, QueryRecord { at: now, outcome: Outcome::Done { answer, coverable } }, now);
    }

    /// Сервер отказал по ключу; `outage` — отказ общий, не из-за запроса: он останавливает
    /// походы на сервер по всем ключам.
    pub(crate) fn record_failure(&mut self, key: &str, now: Instant, why: String, outage: bool) {
        if outage {
            self.outage = Some((now, why));
        } else {
            self.insert(key, QueryRecord { at: now, outcome: Outcome::Failed(why) }, now);
        }
    }

    fn insert(&mut self, key: &str, rec: QueryRecord, now: Instant) {
        self.entries.insert(key.to_string(), rec);
        if self.entries.len() > QUERY_CACHE_MAX {
            self.entries.retain(|_, r| {
                let ttl = if matches!(r.outcome, Outcome::Done { .. }) { QUERY_TTL } else { FAILURE_TTL };
                now.saturating_duration_since(r.at) < ttl
            });
        }
        while self.entries.len() > QUERY_CACHE_MAX {
            let Some(oldest) = self.entries.iter().min_by_key(|(_, r)| r.at).map(|(k, _)| k.clone()) else { break };
            self.entries.remove(&oldest);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Сервер отверг саму операцию (ошибка уровня операции или неожиданный ответ, не временная
/// недоступность): поиск можно повторить через `ResolveNames`.
pub(crate) fn operation_rejected(e: &TransportError) -> bool {
    matches!(e, TransportError::Server { .. } | TransportError::Protocol { .. }) && !e.is_retryable()
}

/// Почему сервер не ответил.
#[derive(Debug)]
enum Failure {
    Transport(TransportError),
    Timeout(Duration),
    /// Транспорт не получен (нет пароля, вход отклонён, приложение закрывается) — текст готов.
    Connect(String),
}

impl Failure {
    /// Текст для окна.
    fn text(&self, describe: fn(&TransportError) -> String) -> String {
        match self {
            Failure::Transport(e) => describe(e),
            Failure::Timeout(limit) => timed_out(*limit),
            Failure::Connect(why) => why.clone(),
        }
    }

    /// Отказ общий, не из-за запроса: на время `FAILURE_TTL` сервер не спрашивается ни по какому.
    fn is_outage(&self) -> bool {
        match self {
            Failure::Transport(e) => !operation_rejected(e),
            Failure::Timeout(_) | Failure::Connect(_) => true,
        }
    }

    /// Вид отказа для журнала: без текста сервера (в нём бывает запрос или адрес).
    fn kind(&self) -> String {
        match self {
            Failure::Transport(e) => error_kind(e),
            Failure::Timeout(_) => "Timeout".into(),
            Failure::Connect(_) => "Connect".into(),
        }
    }
}

/// Вид ошибки транспорта для журнала: вариант, код сервера или HTTP-статус, без текста.
pub(crate) fn error_kind(e: &TransportError) -> String {
    match e {
        TransportError::Server { code, .. } => code.clone(),
        TransportError::Protocol { status, .. } => format!("HTTP {status}"),
        TransportError::SessionExpired => "SessionExpired".into(),
        TransportError::CsrfRejected => "CsrfRejected".into(),
        TransportError::SessionLimit => "SessionLimit".into(),
        TransportError::AuthFailed(_) => "AuthFailed".into(),
        TransportError::Throttled { .. } => "Throttled".into(),
        TransportError::Network(_) => "Network".into(),
        TransportError::Connect(_) => "Connect".into(),
        TransportError::Cancelled => "Cancelled".into(),
        TransportError::TooLarge { .. } => "TooLarge".into(),
    }
}

/// Текст ошибки `ExpandDL` для окна: известные отказы словами, остальное — как везде.
/// Ключ в `sync_state`: сервер ответил, что `ExpandDL` в OWA JSON не реализован (§ 5l). Признак
/// постоянный для ящика: окно прячет «Развернуть», команда не ходит на сервер.
pub(crate) const EXPAND_UNSUPPORTED_KEY: &str = "desktop.expand_dl_unsupported";
/// Текст для окна, когда раскрытие списков на сервере недоступно.
pub(crate) const EXPAND_UNSUPPORTED: &str = "Сервер не поддерживает раскрытие списков рассылки: состав списка можно посмотреть в OWA";

/// HTTP 500 с текстом «не реализовано»: операции на сервере нет, повторять бессмысленно.
pub(crate) fn is_not_implemented(e: &TransportError) -> bool {
    match e {
        TransportError::Protocol { status: 500, raw, .. } => {
            let t = raw.to_lowercase();
            t.contains("не реализ") || t.contains("not implemented") || t.contains("notimplemented")
        }
        _ => false,
    }
}

pub(crate) fn describe_expand(e: &TransportError) -> String {
    match e {
        e if is_not_implemented(e) => EXPAND_UNSUPPORTED.into(),
        TransportError::Protocol { status: 500, .. } => "Сервер не дал развернуть список рассылки (HTTP 500)".into(),
        TransportError::Server { code, .. } if code == "ErrorNameResolutionNoResults" => "Список рассылки не найден в справочнике".into(),
        TransportError::Server { code, .. } if code == "ErrorAccessDenied" => "Нет доступа к составу списка рассылки".into(),
        other => describe_transport(other),
    }
}

/// Выдача поиска после ответа сервера: записи сервера в его порядке (без адреса SMTP — пропуск,
/// повтор адреса — пропуск), затем, если ответ сервера неполный, — совпадения из кэша, которых у
/// сервера не было. Полный ответ сервера — истина: старые записи кэша (уволенные, переименованные)
/// к нему не подмешиваются.
pub(crate) fn merge_results(server: Vec<DirectoryPerson>, cached: Vec<DirectoryPerson>, complete: bool, limit: usize) -> Vec<DirectoryPerson> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let extra = if complete { Vec::new() } else { cached };
    for p in server.into_iter().chain(extra) {
        if out.len() >= limit {
            break;
        }
        let Some(key) = p.email_key() else { continue };
        if seen.insert(key) {
            out.push(p);
        }
    }
    out
}

/// Запись карточки из ответа `ResolveNames` по адресу: совпавшая по адресу, а если такой нет и
/// запись одна — она (адрес мог быть дополнительным адресом ящика). Несколько несовпавших —
/// `None`: угадывать нельзя.
pub(crate) fn pick_card(found: Vec<DirectoryPerson>, key: &str) -> Option<DirectoryPerson> {
    if found.len() == 1 {
        return found.into_iter().next();
    }
    found.into_iter().find(|p| p.email_key().as_deref() == Some(key))
}

/// Что запомнить в кэше запросов после ответа сервера на карточку: адрес, под которым найденная
/// запись ляжет в кэш справочника (спрошенный мог быть дополнительным адресом ящика, а запись
/// ложится под основным), или пустой ответ — в справочнике не нашлось. `None` — не запоминать:
/// у записи нет SMTP-адреса, в кэш она не ляжет, и следующее открытие снова спросит сервер.
pub(crate) fn card_answer(picked: Option<&DirectoryPerson>) -> Option<Answer> {
    match picked {
        None => Some(Answer::new(Vec::new(), true)),
        Some(p) => p.email_key().map(|k| Answer::new(vec![k], true)),
    }
}

/// Адрес, под которым лежит запись карточки, если не под спрошенным `key` (запомненный ответ).
pub(crate) fn card_alias<'a>(key: &str, keys: &'a [String]) -> Option<&'a str> {
    keys.iter().map(String::as_str).find(|k| *k != key)
}

/// Вид записи карточки: `ResolveNames` знает только `MailboxType`, у переговорной это обычный
/// `Mailbox`. Поэтому переговорная из кэша (её дал `FindPeople`) остаётся переговорной, а
/// неопределённый вид уступает известному.
pub(crate) fn card_kind(fresh: PersonKind, cached: Option<PersonKind>) -> PersonKind {
    match (fresh, cached) {
        (PersonKind::Mailbox | PersonKind::Other, Some(PersonKind::Room)) => PersonKind::Room,
        (PersonKind::Other, Some(old)) => old,
        (k, _) => k,
    }
}

/// Запись кэша, похоже, получена только поиском (`FindPeople` не отдаёт отдела): у ящика человека
/// без отдела карточка один раз в `QUERY_TTL` переспрашивает сервер полными данными.
fn looks_incomplete(p: &DirectoryPerson) -> bool {
    // Без отдела — запись из поиска; без `direct_reports` — кэш до появления оргструктуры (§ 5l').
    matches!(p.kind, PersonKind::Mailbox | PersonKind::Other) && (p.department.is_none() || p.direct_reports.is_none())
}

/// Записи с адресом SMTP без повторов адреса.
fn with_unique_email(people: Vec<DirectoryPerson>) -> Vec<DirectoryPerson> {
    let mut seen = HashSet::new();
    people.into_iter().filter(|p| p.email_key().is_some_and(|k| seen.insert(k))).collect()
}

/// Ключ карточки в кэше запросов и в замках. Управляющий символ в начале не даёт совпасть с
/// ключом поиска (из запроса они выброшены), пробел — покрыть продолжение (`QueryCache::plan`).
fn card_key(email_key: &str) -> String {
    format!("\u{1}card {email_key}")
}

/// Ключ развёртывания списка в замках и в кэше запросов.
fn expand_key(email_key: &str) -> String {
    format!("\u{1}expand {email_key}")
}

// --- состояние -------------------------------------------------------------------------------

/// Вход на сервер, начатый этим модулем: одна задача на всех, результат — в `SyncState`.
#[derive(Default)]
struct LoginGate {
    pending: AtomicBool,
    /// Текст отказа последнего входа (для ожидавших его команд).
    error: Mutex<Option<String>>,
}

/// Состояние команд справочника: замки по запросу и адресу, кэш запросов (поиск и карточки —
/// общий: общий отказ останавливает и то и другое), вход и запасной путь поиска.
#[derive(Default)]
pub(crate) struct DirectoryState {
    locks: KeyedLocks<String>,
    queries: Mutex<QueryCache>,
    login: Arc<LoginGate>,
    /// Когда сервер отверг `FindPeople`: до `FIND_PEOPLE_RETRY` поиск идёт через `ResolveNames`.
    find_people_rejected: Mutex<Option<Instant>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Внутри только кэш запросов и метки времени: отравленный мьютекс не страшен.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Найденное на сервере.
struct Found {
    people: Vec<DirectoryPerson>,
    complete: bool,
    via: &'static str,
}

/// Запрос к серверу до срока: ошибка транспорта или таймаут — `Failure`.
async fn until<T>(deadline: Instant, limit: Duration, fut: impl Future<Output = aiowa_transport::Result<T>>) -> Result<T, Failure> {
    match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Failure::Transport(e)),
        Err(_) => Err(Failure::Timeout(limit)),
    }
}

impl DirectoryState {
    /// Транспорт процесса до срока. Если сессии нет, вход идёт в отдельной задаче (одной на
    /// модуль, сколько бы команд её ни ждали): таймаут не обрывает его на полпути.
    async fn transport(&self, sync: &Arc<SyncState>, deadline: Instant, limit: Duration) -> Result<OwaTransport, Failure> {
        if let Some(t) = sync.current_transport() {
            return Ok(t);
        }
        if !self.login.pending.swap(true, Ordering::SeqCst) {
            *lock(&self.login.error) = None;
            let (sync, gate) = (sync.clone(), self.login.clone());
            tauri::async_runtime::spawn(async move {
                if let Err(e) = sync.transport().await {
                    warn!(error = %e, "справочник: вход на сервер не удался");
                    *lock(&gate.error) = Some(e.to_string());
                }
                gate.pending.store(false, Ordering::SeqCst);
            });
        }
        loop {
            if let Some(t) = sync.current_transport() {
                return Ok(t);
            }
            if !self.login.pending.load(Ordering::SeqCst) {
                let why = lock(&self.login.error).clone().unwrap_or_else(|| "Вход на сервер не удался, повторите попытку".into());
                return Err(Failure::Connect(why));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Failure::Timeout(limit));
            }
            tokio::time::sleep((deadline - now).min(LOGIN_POLL)).await;
        }
    }

    fn find_people_usable(&self, now: Instant) -> bool {
        lock(&self.find_people_rejected).is_none_or(|at| now.saturating_duration_since(at) >= FIND_PEOPLE_RETRY)
    }

    /// Поиск на сервере: `FindPeople`, а если сервер отверг операцию — `ResolveNames` без данных
    /// контакта (имя, адрес и вид: полные данные на сотню совпадений весят сотни килобайт).
    async fn fetch_people(&self, sync: &Arc<SyncState>, text: &str) -> Result<Found, Failure> {
        let deadline = Instant::now() + SERVER_TIMEOUT;
        let t = self.transport(sync, deadline, SERVER_TIMEOUT).await?;
        if self.find_people_usable(Instant::now()) {
            match until(deadline, SERVER_TIMEOUT, t.find_people(text, SERVER_MAX)).await {
                Ok(people) => return Ok(Found { complete: people.len() < SERVER_MAX, people, via: "FindPeople" }),
                Err(Failure::Transport(e)) if operation_rejected(&e) => {
                    warn!(kind = %error_kind(&e), "справочник: FindPeople отвергнут, поиск через ResolveNames");
                    *lock(&self.find_people_rejected) = Some(Instant::now());
                }
                Err(f) => return Err(f),
            }
        }
        let people = until(deadline, SERVER_TIMEOUT, t.resolve_names(text, false)).await?;
        Ok(Found { complete: people.len() < RESOLVE_CAP, people, via: "ResolveNames" })
    }

    /// Отказ сервера: журнал (без текста сервера), сброс мёртвой сессии, текст для окна.
    async fn fail(&self, sync: &SyncState, what: &'static str, f: &Failure, describe: fn(&TransportError) -> String) -> String {
        warn!(op = what, kind = %f.kind(), "справочник: сервер не ответил");
        if let Failure::Transport(e) = f {
            sync.invalidate_on(e).await;
        }
        f.text(describe)
    }
}

// --- база ------------------------------------------------------------------------------------

/// Совпадения в кэше справочника (в блокирующем потоке).
async fn cached_search(store: &Arc<Store>, account: AccountId, text: &str, limit: usize) -> CmdResult<Vec<DirectoryPerson>> {
    let (store, text) = (store.clone(), text.to_string());
    tauri::async_runtime::spawn_blocking(move || store.directory_search_cached(account, &text, limit)).await.map_err(err)?.map_err(|e| format!("Ошибка локальной базы: {e}"))
}

/// Ответ сервера, записанный в кэш.
struct Stored {
    /// Выдача окну.
    shown: Vec<DirectoryPerson>,
    /// Адреса записей ответа в порядке сервера (для `Answer`).
    keys: Vec<String>,
    /// Каждую запись ответа поиск по кэшу находит по тому же запросу.
    visible: bool,
}

/// Найденное сервером — в кэш (слиянием со старыми записями), в ответ — записи сервера,
/// дополненные из кэша (отдел и телефоны от прошлых карточек), и совпадения кэша.
async fn store_found(store: &Arc<Store>, account: AccountId, text: &str, found: Vec<DirectoryPerson>, complete: bool, limit: usize) -> CmdResult<Stored> {
    let (store, text) = (store.clone(), text.to_string());
    tauri::async_runtime::spawn_blocking(move || -> aiowa_store::Result<Stored> {
        let found = with_unique_email(found);
        store.upsert_directory_people(account, &found)?;
        let mut merged = Vec::with_capacity(found.len());
        let mut keys = Vec::with_capacity(found.len());
        for p in found {
            let stored = match p.email_key() {
                Some(k) => {
                    let old = store.directory_person(account, &k)?;
                    keys.push(k);
                    old
                }
                None => None,
            };
            merged.push(match stored {
                Some(old) => p.merged_with(&old),
                None => p,
            });
        }
        let visible = merged.iter().all(|p| directory_person_matches(p, &text));
        let cached = if complete { Vec::new() } else { store.directory_search_cached(account, &text, limit)? };
        Ok(Stored { shown: merge_results(merged, cached, complete, limit), keys, visible })
    })
    .await
    .map_err(err)?
    .map_err(|e| format!("Ошибка локальной базы: {e}"))
}

/// Выдача по запомненному ответу сервера (`Plan::Cached`) — та же, что дал сам ответ: его записи
/// из кэша справочника в порядке сервера (при `covered` — только подходящие к новому тексту), а к
/// неполному ответу — совпадения кэша, как в `store_found`. Записи кэша, которых в полном ответе
/// не было (удалённые из справочника, со старым адресом), не показываются.
pub(crate) fn replay_answer(store: &Store, account: AccountId, text: &str, answer: &Answer, covered: bool, limit: usize) -> aiowa_store::Result<Vec<DirectoryPerson>> {
    let mut server = Vec::new();
    for k in answer.keys.iter() {
        if server.len() >= limit {
            break;
        }
        let Some(p) = store.directory_person(account, k)? else { continue };
        if !covered || directory_person_matches(&p, text) {
            server.push(p);
        }
    }
    let cached = if answer.complete { Vec::new() } else { store.directory_search_cached(account, text, limit)? };
    Ok(merge_results(server, cached, answer.complete, limit))
}

/// Ответ поиска без сервера, если так решил кэш запросов; `None` — идти на сервер.
async fn answer_from_cache(plan: Plan, store: &Arc<Store>, account: AccountId, text: &str, limit: usize) -> CmdResult<Option<DirectorySearchDto>> {
    match plan {
        Plan::Server => Ok(None),
        // Отказ сервера: весь кэш — осознанный запасной путь без сети.
        Plan::Failed(why) => Ok(Some(DirectorySearchDto::new(cached_search(store, account, text, limit).await?, true, Some(why)))),
        Plan::Cached { answer, covered } => {
            let (store, text) = (store.clone(), text.to_string());
            let people = tauri::async_runtime::spawn_blocking(move || replay_answer(&store, account, &text, &answer, covered, limit)).await.map_err(err)?.map_err(|e| format!("Ошибка локальной базы: {e}"))?;
            Ok(Some(DirectorySearchDto::new(people, true, None)))
        }
    }
}

// --- команды Tauri ---------------------------------------------------------------------------

/// Поиск адресатов по справочнику организации (см. описание модуля). `cache_only` — без сервера:
/// быстрый ответ окну, пока сервер ищет (запомненный ответ сервера, если он свежий, иначе кэш
/// справочника). Запрос короче двух символов — пусто. Отказ сервера — кэш и `error`; ошибка
/// локальной базы — `Err`.
#[tauri::command]
pub(crate) async fn directory_search(query: String, limit: Option<usize>, cache_only: Option<bool>, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, dir: State<'_, DirectoryState>) -> CmdResult<DirectorySearchDto> {
    let Some(q) = normalize_query(&query) else { return Ok(DirectorySearchDto::new(Vec::new(), true, None)) };
    let limit = limit.unwrap_or(SEARCH_DEFAULT).clamp(1, SEARCH_MAX);
    let (store, account) = (state.store.clone(), state.account);
    // Решение берётся под мьютексом отдельным оператором: страж не должен жить через `.await`.
    let plan = lock(&dir.queries).plan(&q.key, Instant::now(), true);
    if cache_only.unwrap_or(false) && plan == Plan::Server {
        return Ok(DirectorySearchDto::new(cached_search(&store, account, &q.text, limit).await?, true, None));
    }
    if let Some(dto) = answer_from_cache(plan, &store, account, &q.text, limit).await? {
        return Ok(dto);
    }
    let gate = dir.locks.get(q.key.clone());
    let _busy = gate.lock().await;
    // Пока ждали замок, тот же запрос мог сходить на сервер: тогда кэш уже свежий.
    let plan = lock(&dir.queries).plan(&q.key, Instant::now(), true);
    if let Some(dto) = answer_from_cache(plan, &store, account, &q.text, limit).await? {
        return Ok(dto);
    }
    let started = Instant::now();
    match dir.fetch_people(sync.inner(), &q.text).await {
        Ok(Found { people, complete, via }) => {
            let found = people.len();
            let Stored { shown, keys, visible } = store_found(&store, account, &q.text, people, complete, limit).await?;
            let coverable = complete && visible && !keys.is_empty();
            lock(&dir.queries).record_done(&q.key, Instant::now(), Answer::new(keys, complete), coverable);
            info!(op = via, query_chars = q.text.chars().count(), found, complete, coverable, shown = shown.len(), ms = started.elapsed().as_millis() as u64, "справочник: поиск на сервере");
            Ok(DirectorySearchDto::new(shown, false, None))
        }
        Err(f) => {
            let why = dir.fail(&sync, "search", &f, describe_transport).await;
            lock(&dir.queries).record_failure(&q.key, Instant::now(), why.clone(), f.is_outage());
            Ok(DirectorySearchDto::new(cached_search(&store, account, &q.text, limit).await?, true, Some(why)))
        }
    }
}

/// Карточка контакта: запись справочника из кэша; если её нет, она выглядит неполной (только
/// из поиска, без отдела) или запрошено `refresh` — `ResolveNames` с полными данными с сервера и
/// замена записи в кэше. Без `refresh` сервер по одному адресу спрашивается не чаще раза в
/// `QUERY_TTL`. `local` — переписка по локальной базе. Отказ сервера — кэш (что есть) и `error`.
#[tauri::command]
pub(crate) async fn contact_card(email: String, refresh: bool, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, dir: State<'_, DirectoryState>) -> CmdResult<ContactCardDto> {
    let key = email.trim().to_lowercase();
    if key.is_empty() {
        return Err("Нет адреса".into());
    }
    let (store, account) = (state.store.clone(), state.account);
    let ckey = card_key(&key);
    let gate = dir.locks.get(ckey.clone());
    let _busy = gate.lock().await;
    let (cached, local) = {
        let (store, key) = (store.clone(), key.clone());
        tauri::async_runtime::spawn_blocking(move || -> aiowa_store::Result<_> { Ok((store.directory_person(account, &key)?, store.contact_local_stats(account, &key)?)) })
            .await
            .map_err(err)?
            .map_err(|e| format!("Ошибка локальной базы: {e}"))?
    };
    let local = ContactLocalDto::from(local);
    let wanted = refresh || cached.as_ref().is_none_or(looks_incomplete);
    // Адрес не SMTP (устаревший EX) справочник по адресу не найдёт; `None` — ответить кэшем.
    let plan = if !looks_like_email(&key) || !wanted {
        None
    } else if refresh {
        Some(Plan::Server)
    } else {
        Some(lock(&dir.queries).plan(&ckey, Instant::now(), false))
    };
    let error = match plan {
        None => None,
        Some(Plan::Cached { answer, .. }) => {
            // Спрошенный адрес мог оказаться дополнительным адресом ящика: запись лежит под основным.
            if cached.is_none()
                && let Some(alt) = card_alias(&key, &answer.keys)
            {
                let alt_key = alt.to_string();
                let (st, alt) = (store.clone(), alt_key.clone());
                let found = tauri::async_runtime::spawn_blocking(move || st.directory_person(account, &alt)).await.map_err(err)?.map_err(|e| format!("Ошибка локальной базы: {e}"))?;
                if let Some(p) = found {
                    let photo_url = cached_photo_url(&store, account, &alt_key);
                    return Ok(ContactCardDto { person: Some(p.into()), from_cache: true, error: None, local, photo_url });
                }
            }
            None
        }
        Some(Plan::Failed(why)) => Some(why),
        Some(Plan::Server) => {
            let started = Instant::now();
            let deadline = started + SERVER_TIMEOUT;
            let fetched = match dir.transport(sync.inner(), deadline, SERVER_TIMEOUT).await {
                Ok(t) => until(deadline, SERVER_TIMEOUT, t.resolve_names_full(&key)).await,
                Err(f) => Err(f),
            };
            match fetched {
                Ok(found) => {
                    let total = found.len();
                    let photo = photo_for(&found, &key);
                    let picked = pick_card(found.into_iter().map(|r| r.person).collect(), &key).map(|mut p| {
                        p.kind = card_kind(p.kind, cached.as_ref().map(|c| c.kind));
                        p
                    });
                    info!(found = total, matched = picked.is_some(), refresh, ms = started.elapsed().as_millis() as u64, "справочник: карточка контакта с сервера");
                    let remember = card_answer(picked.as_ref());
                    if let Some(p) = picked {
                        let (st, row, photo_bytes, pkey) = (store.clone(), p.clone(), photo.clone(), key.clone());
                        let stored = tauri::async_runtime::spawn_blocking(move || {
                            let written = st.replace_directory_person(account, &row)?;
                            if written {
                                // Фото или отметка «фото нет» — вместе с записью.
                                st.set_directory_photo(account, &pkey, photo_bytes.as_deref())?;
                            }
                            Ok::<bool, aiowa_store::StoreError>(written)
                        })
                        .await
                        .map_err(err)?;
                        // Ответ запоминается, только если запись легла в кэш: иначе повторное
                        // открытие в течение `QUERY_TTL` показало бы пустую карточку.
                        let written = stored.unwrap_or_else(|e| {
                            warn!(error = %e, "справочник: карточка не записана в кэш");
                            false
                        });
                        if written && let Some(answer) = remember {
                            lock(&dir.queries).record_done(&ckey, Instant::now(), answer, false);
                        }
                        info!(photo_bytes = photo.as_ref().map(Vec::len).unwrap_or(0), "справочник: фото карточки");
                        let photo_url = photo.as_deref().map(photo_data_url);
                        return Ok(ContactCardDto { person: Some(p.into()), from_cache: false, error: None, local, photo_url });
                    }
                    if let Some(answer) = remember {
                        lock(&dir.queries).record_done(&ckey, Instant::now(), answer, false);
                    }
                    None
                }
                Err(f) => {
                    let why = dir.fail(&sync, "card", &f, describe_transport).await;
                    lock(&dir.queries).record_failure(&ckey, Instant::now(), why.clone(), f.is_outage());
                    Some(why)
                }
            }
        }
    };
    let photo_url = cached_photo_url(&store, account, &key);
    Ok(ContactCardDto { person: cached.map(DirectoryPersonDto::from), from_cache: true, error, local, photo_url })
}

/// Фото из кэша справочника по списку адресов, без сети: адрес (в нижнем регистре) → `data:`-адрес.
/// Для заголовков писем и подсказок: сервер по ним не спрашивается.
#[tauri::command]
pub(crate) async fn contact_photos(emails: Vec<String>, state: State<'_, AppState>) -> CmdResult<HashMap<String, String>> {
    let (store, account) = (state.store.clone(), state.account);
    let emails: Vec<String> = emails.into_iter().take(200).collect();
    tauri::async_runtime::spawn_blocking(move || {
        let found = store.directory_photos(account, &emails).map_err(|e| format!("Ошибка локальной базы: {e}"))?;
        Ok(found.into_iter().map(|(k, b)| (k, photo_data_url(&b))).collect())
    })
    .await
    .map_err(err)?
}

/// Фото одного человека: из кэша, если оно запрашивалось не старше недели (в том числе отметка
/// «фото нет»), иначе `ResolveNames` с полными данными (одна запись за вызов, под замком адреса),
/// запись в кэш и `data:`-адрес; адрес не SMTP или сервер недоступен — `None` без ошибки.
#[tauri::command]
pub(crate) async fn contact_photo(email: String, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, dir: State<'_, DirectoryState>) -> CmdResult<Option<String>> {
    let key = email.trim().to_lowercase();
    if !looks_like_email(&key) {
        return Ok(None);
    }
    let (store, account) = (state.store.clone(), state.account);
    let gate = dir.locks.get(card_key(&key));
    let _busy = gate.lock().await;
    let cached = {
        let (st, k) = (store.clone(), key.clone());
        tauri::async_runtime::spawn_blocking(move || st.directory_photo(account, &k)).await.map_err(err)?.map_err(|e| format!("Ошибка локальной базы: {e}"))?
    };
    if let Some((photo, at)) = cached
        && jiff::Timestamp::now().as_millisecond() - at < PHOTO_TTL_MS
    {
        return Ok(photo.map(|b| photo_data_url(&b)));
    }
    let deadline = Instant::now() + SERVER_TIMEOUT;
    let fetched = match dir.transport(sync.inner(), deadline, SERVER_TIMEOUT).await {
        Ok(t) => until(deadline, SERVER_TIMEOUT, t.resolve_names_full(&key)).await,
        Err(f) => Err(f),
    };
    match fetched {
        Ok(found) => {
            let photo = photo_for(&found, &key);
            let person = pick_card(found.into_iter().map(|r| r.person).collect(), &key);
            let (st, k, bytes) = (store.clone(), key.clone(), photo.clone());
            let stored = tauri::async_runtime::spawn_blocking(move || -> aiowa_store::Result<bool> {
                if let Some(p) = person {
                    st.replace_directory_person(account, &p)?;
                }
                st.set_directory_photo(account, &k, bytes.as_deref())
            })
            .await
            .map_err(err)?;
            if let Err(e) = stored {
                warn!(error = %e, "справочник: фото не записано в кэш");
            }
            info!(photo_bytes = photo.as_ref().map(Vec::len).unwrap_or(0), "справочник: фото с сервера");
            Ok(photo.as_deref().map(photo_data_url))
        }
        Err(f) => {
            let why = dir.fail(&sync, "photo", &f, describe_transport).await;
            warn!(why, "справочник: фото не получено");
            Ok(None)
        }
    }
}

/// Участники списка рассылки (`ExpandDL`, один уровень: вложенные списки приходят записями вида
/// `list`). Записи без адреса SMTP пропускаются, найденные ложатся в кэш справочника. Отказ
/// сервера — `Err` с текстом для окна.
#[tauri::command]
pub(crate) async fn directory_expand(email: String, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, dir: State<'_, DirectoryState>) -> CmdResult<Vec<DirectoryPersonDto>> {
    let key = email.trim().to_lowercase();
    if !looks_like_email(&key) {
        return Err("У списка рассылки нет почтового адреса".into());
    }
    if expand_unsupported(&state.store, state.account) {
        return Err(EXPAND_UNSUPPORTED.into());
    }
    let gate = dir.locks.get(expand_key(&key));
    let _busy = gate.lock().await;
    let started = Instant::now();
    let deadline = started + EXPAND_TIMEOUT;
    let fetched = match dir.transport(sync.inner(), deadline, EXPAND_TIMEOUT).await {
        Ok(t) => until(deadline, EXPAND_TIMEOUT, t.expand_dl(&key)).await,
        Err(f) => Err(f),
    };
    let members = match fetched {
        Ok(m) => m,
        Err(f) => {
            let why = dir.fail(&sync, "expand", &f, describe_expand).await;
            if let Failure::Transport(e) = &f
                && is_not_implemented(e)
                && let Err(e) = state.store.set_state(state.account, EXPAND_UNSUPPORTED_KEY, Some("1"))
            {
                warn!(error = %e, "справочник: признак «ExpandDL не реализован» не записан");
            }
            // Общий отказ (нет связи, вход) виден и поиску: незачем ему ждать таймаут.
            if f.is_outage() {
                lock(&dir.queries).record_failure(&expand_key(&key), Instant::now(), why.clone(), true);
            }
            return Err(why);
        }
    };
    let total = members.len();
    let members = with_unique_email(members);
    info!(members = total, usable = members.len(), ms = started.elapsed().as_millis() as u64, "справочник: участники списка рассылки");
    let (store, account, rows) = (state.store.clone(), state.account, members.clone());
    let stored = tauri::async_runtime::spawn_blocking(move || store.upsert_directory_people(account, &rows)).await.map_err(err)?;
    if let Err(e) = stored {
        warn!(error = %e, "справочник: участники списка не записаны в кэш");
    }
    Ok(members.into_iter().map(DirectoryPersonDto::from).collect())
}

/// Сервер уже ответил, что раскрытия списков нет.
fn expand_unsupported(store: &Store, account: AccountId) -> bool {
    store.get_state(account, EXPAND_UNSUPPORTED_KEY).ok().flatten().as_deref() == Some("1")
}

/// Что умеет справочник на этом сервере (для окна).
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct DirectoryCapsDto {
    can_expand: bool,
}

#[tauri::command]
pub(crate) fn directory_capabilities(state: State<'_, AppState>) -> DirectoryCapsDto {
    DirectoryCapsDto { can_expand: !expand_unsupported(&state.store, state.account) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_not_implemented_is_detected_and_described() {
        let ni = TransportError::Protocol { op: "ExpandDL".into(), status: 500, raw: "Метод или операция не реализована.".into() };
        assert!(is_not_implemented(&ni));
        assert_eq!(describe_expand(&ni), EXPAND_UNSUPPORTED);
        let en = TransportError::Protocol { op: "ExpandDL".into(), status: 500, raw: "The method or operation is not implemented.".into() };
        assert!(is_not_implemented(&en));
        let other = TransportError::Protocol { op: "ExpandDL".into(), status: 500, raw: "Object reference not set".into() };
        assert!(!is_not_implemented(&other));
        assert_eq!(describe_expand(&other), "Сервер не дал развернуть список рассылки (HTTP 500)");
    }

    fn person(name: &str, email: &str) -> DirectoryPerson {
        DirectoryPerson { display_name: Some(name.into()), email: Some(email.into()), kind: PersonKind::Mailbox, ..Default::default() }
    }

    fn emails(people: &[DirectoryPerson]) -> Vec<&str> {
        people.iter().map(|p| p.email.as_deref().unwrap_or_default()).collect()
    }

    #[test]
    fn query_is_normalized() {
        let q = normalize_query("  «Пётр   Семёнов»,  ").unwrap();
        assert_eq!(q.text, "Пётр Семёнов", "кавычки и разделители по краям сняты, пробелы схлопнуты");
        assert_eq!(q.key, "петр семенов", "ключ без регистра и с ё → е");
        assert_eq!(normalize_query("\"Ivan\" <").unwrap().text, "Ivan");
        assert_eq!(normalize_query("ив\u{0}\tан").unwrap().text, "ив ан", "управляющие символы выброшены, табуляция — пробел");
        assert_eq!(normalize_query("ivan.p@x").unwrap().text, "ivan.p@x", "внутри запроса знаки остаются");
        assert_eq!(normalize_query("Ив").unwrap().key, "ив", "два символа — уже запрос");
        assert_eq!(normalize_query("и"), None, "короче двух символов — пусто");
        assert_eq!(normalize_query("  и  "), None);
        assert_eq!(normalize_query(""), None);
        assert_eq!(normalize_query("--"), None, "без букв и цифр искать нечего");
        let long = "а".repeat(300);
        assert_eq!(normalize_query(&long).unwrap().text.chars().count(), MAX_QUERY_CHARS, "длинный запрос обрезан по символам, не по байтам");
        assert_eq!(normalize_query(&format!("{}, x", "б".repeat(MAX_QUERY_CHARS - 1))).unwrap().text, "б".repeat(MAX_QUERY_CHARS - 1), "разделитель на месте обреза снят");
        assert_eq!(normalize_query("ИВАН").unwrap().key, normalize_query("иван").unwrap().key, "один ключ для замка и кэша");
    }

    fn temp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("aiowa-desktop-directory-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(dir.join("mail.db")).unwrap()
    }

    /// Ответ сервера из одной записи; полный ответ здесь же и покрывающий.
    fn done(c: &mut QueryCache, key: &str, at: Instant, complete: bool) {
        c.record_done(key, at, Answer::new(vec![format!("{key}@x.ru")], complete), complete);
    }

    /// Ответ без сервера: адрес первой записи запомненного ответа и покрыт ли запрос более коротким.
    fn cached(plan: Plan) -> Option<(String, bool)> {
        match plan {
            Plan::Cached { answer, covered } => Some((answer.keys.first().cloned().unwrap_or_default(), covered)),
            _ => None,
        }
    }

    #[test]
    fn query_cache_policy() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut c = QueryCache::default();
        assert_eq!(c.plan("пет", t0, true), Plan::Server, "незнакомый запрос — на сервер");
        done(&mut c, "пет", t0, false);
        assert_eq!(cached(c.plan("пет", s(60), true)), Some(("пет@x.ru".into(), false)), "тот же запрос в течение 10 минут — запомненный ответ сервера");
        assert_eq!(c.plan("пет", t0 + QUERY_TTL, true), Plan::Server, "через 10 минут — снова сервер");
        assert_eq!(c.plan("петр", s(1), true), Plan::Server, "неполный ответ не покрывает продолжение");

        // Полный ответ по началу слова покрывает продолжения из одного слова.
        done(&mut c, "ив", t0, true);
        assert_eq!(cached(c.plan("иван", s(1), true)), Some(("ив@x.ru".into(), true)));
        assert_eq!(c.plan("иван", s(1), false), Plan::Server, "без cover (карточка) покрытие не действует");
        assert_eq!(c.plan("иван пет", s(1), true), Plan::Server, "запрос из нескольких слов не покрывается");
        assert_eq!(c.plan("сив", s(1), true), Plan::Server, "покрывает только начало");
        assert_eq!(c.plan("иван", t0 + QUERY_TTL, true), Plan::Server, "устаревший полный ответ не покрывает");
        done(&mut c, "ива", t0, true);
        assert_eq!(cached(c.plan("иван", s(1), true)), Some(("ива@x.ru".into(), true)), "из покрывающих — самый длинный: у него самый узкий ответ");
        done(&mut c, "ив ан", t0, true);
        assert_eq!(c.plan("ив анна", t0, true), Plan::Server, "полный ответ из нескольких слов не покрывает");
        // Полный ответ, часть которого сервер нашёл по полям, которых нет в кэше, не покрывает.
        c.record_done("ан", t0, Answer::new(vec!["a@x.ru".into()], true), false);
        assert_eq!(c.plan("анна", s(1), true), Plan::Server);
        assert_eq!(cached(c.plan("ан", s(1), true)), Some(("a@x.ru".into(), false)), "но сам запрос отвечается по нему");

        // Отказ по запросу: 30 с тот же текст ошибки, другие запросы идут на сервер.
        c.record_failure("сид", t0, "Сервер отказал: X".into(), false);
        assert_eq!(c.plan("сид", s(5), true), Plan::Failed("Сервер отказал: X".into()));
        assert_eq!(c.plan("сидор", s(5), true), Plan::Server);
        assert_eq!(c.plan("сид", t0 + FAILURE_TTL, true), Plan::Server, "после 30 с — снова сервер");

        // Общий отказ: 30 с ни один запрос не идёт на сервер, но свежий ответ по-прежнему отвечает.
        c.record_failure("кто", t0, "Нет связи с сервером".into(), true);
        assert_eq!(c.plan("кто-то", s(1), true), Plan::Failed("Нет связи с сервером".into()));
        assert!(cached(c.plan("пет", s(1), true)).is_some(), "свежий ответ сервера важнее общего отказа");
        assert_eq!(c.plan("кто-то", t0 + FAILURE_TTL, true), Plan::Server);
        c.record_failure("кто", t0, "Нет связи с сервером".into(), true);
        done(&mut c, "ещё", s(2), false);
        assert_eq!(c.plan("кто-то", s(3), true), Plan::Server, "удачный ответ снимает общий отказ");

        // Кэш запросов ограничен.
        let mut big = QueryCache::default();
        for i in 0..(QUERY_CACHE_MAX + 50) {
            done(&mut big, &format!("q{i}"), t0 + Duration::from_millis(i as u64), false);
        }
        assert_eq!(big.len(), QUERY_CACHE_MAX);
        assert_eq!(big.plan("q0", s(1), true), Plan::Server, "вытеснены самые старые");
        assert!(cached(big.plan(&format!("q{}", QUERY_CACHE_MAX + 49), s(1), true)).is_some());
    }

    #[test]
    fn cached_answer_replays_server_not_whole_cache() {
        let store = temp_store("replay");
        let acc = store.account("h", "u").unwrap();
        // В кэше с прошлых поисков — давно удалённый из справочника «Петров Старый».
        store.upsert_directory_people(acc, &[person("Петров Старый", "old@x.ru"), person("Петренко", "petrenko@x.ru")]).unwrap();
        // Полный ответ сервера на «петр» — без него.
        let server = vec![person("Петров Иван", "ivan@x.ru"), person("Петренко", "petrenko@x.ru")];
        store.upsert_directory_people(acc, &server).unwrap();
        let answer = Answer::new(server.iter().filter_map(DirectoryPerson::email_key).collect(), true);
        let replay = |text: &str, answer: &Answer, covered: bool, limit: usize| replay_answer(&store, acc, text, answer, covered, limit).unwrap();
        assert_eq!(emails(&replay("петр", &answer, false, 10)), ["ivan@x.ru", "petrenko@x.ru"], "повтор запроса — ответ сервера в его порядке, без старых записей кэша");
        assert_eq!(emails(&replay("петро", &answer, true, 10)), ["ivan@x.ru"], "продолжение — записи того ответа, подходящие к новому тексту");
        assert_eq!(emails(&replay("петр", &answer, false, 1)), ["ivan@x.ru"], "limit соблюдается");
        // Неполный ответ дополняется кэшем, как и в первый раз (`store_found`).
        let partial = Answer::new(vec!["petrenko@x.ru".into()], false);
        assert_eq!(emails(&replay("петр", &partial, false, 10)), ["petrenko@x.ru", "ivan@x.ru", "old@x.ru"]);
        assert!(replay("петр", &Answer::new(Vec::new(), true), false, 10).is_empty(), "пустой полный ответ — пусто, а не весь кэш");
    }

    #[test]
    fn card_found_by_alias_is_found_again() {
        // ResolveNames по дополнительному адресу ящика вернул одну запись с основным адресом.
        let t0 = Instant::now();
        let picked = pick_card(vec![person("Иван", "Ivan.Primary@x.ru")], "ivan.alias@x.ru");
        let answer = card_answer(picked.as_ref()).expect("запись с SMTP-адресом запоминается");
        let store = temp_store("alias");
        let acc = store.account("h", "u").unwrap();
        assert!(store.replace_directory_person(acc, picked.as_ref().unwrap()).unwrap());
        let mut c = QueryCache::default();
        let ckey = card_key("ivan.alias@x.ru");
        c.record_done(&ckey, t0, answer, false);
        // Повторное открытие без refresh: по спрошенному адресу записи нет, сервер не спрашивается…
        assert_eq!(store.directory_person(acc, "ivan.alias@x.ru").unwrap(), None);
        let Some((first, _)) = cached(c.plan(&ckey, t0 + Duration::from_secs(60), false)) else { panic!("свежий ответ — без сервера") };
        // …и запись находится под основным адресом.
        let keys = [first];
        assert_eq!(card_alias("ivan.alias@x.ru", &keys), Some("ivan.primary@x.ru"));
        assert_eq!(store.directory_person(acc, "ivan.primary@x.ru").unwrap().unwrap().display_name.as_deref(), Some("Иван"));

        assert_eq!(card_answer(Some(&person("EX", "/o=Org/cn=x"))), None, "без SMTP-адреса — не запоминать: в кэш запись не ляжет");
        assert!(card_answer(None).unwrap().keys.is_empty(), "не нашлось — запоминается пустой ответ");
        assert_eq!(card_alias("i@x.ru", &["i@x.ru".to_string()]), None, "запись под спрошенным адресом — не другой адрес");
    }

    #[test]
    fn person_kind_to_window_strings() {
        let cases = [
            (PersonKind::Mailbox, "mailbox"),
            (PersonKind::DistributionList, "list"),
            (PersonKind::Contact, "contact"),
            (PersonKind::Room, "room"),
            (PersonKind::Other, "other"),
        ];
        for (kind, want) in cases {
            assert_eq!(serde_json::to_value(KindDto::from(kind)).unwrap(), serde_json::json!(want), "{kind:?}");
        }
        let dto = DirectoryPersonDto::from(DirectoryPerson { title: Some("Инженер".into()), alias: Some("ipetrov".into()), kind: PersonKind::DistributionList, ..person("Все", " all@x.ru ") });
        let v = serde_json::to_value(&dto).unwrap();
        assert_eq!(v["kind"], "list");
        assert_eq!(v["name"], "Все");
        assert_eq!(v["email"], "all@x.ru", "адрес без пробелов по краям");
        assert_eq!(v["title"], "Инженер");
        assert!(v.get("alias").is_none(), "псевдоним окну не нужен");
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["name", "email", "kind", "title", "department", "company", "office", "business_phone", "mobile_phone", "manager", "direct_reports", "address", "notes"]);
        assert!(v["manager"].is_null() && v["direct_reports"].as_array().is_some_and(Vec::is_empty), "без полных данных — руководителя нет, подчинённых пусто");
    }

    #[test]
    fn server_results_merge_with_cache() {
        let server = vec![person("Б", "B@x.ru"), person("Без адреса", "/o=Org/cn=b"), person("Б дубль", "b@X.ru"), person("А", "a@x.ru")];
        let cached = vec![person("А", "a@x.ru"), person("В", "v@x.ru"), person("Г", "g@x.ru")];
        let full = merge_results(server.clone(), cached.clone(), true, 10);
        assert_eq!(emails(&full), ["B@x.ru", "a@x.ru"], "полный ответ сервера: его порядок, без EX и без повтора адреса, кэш не подмешан");
        let partial = merge_results(server.clone(), cached.clone(), false, 10);
        assert_eq!(emails(&partial), ["B@x.ru", "a@x.ru", "v@x.ru", "g@x.ru"], "неполный ответ дополняется кэшем без повторов");
        assert_eq!(emails(&merge_results(server, cached, false, 3)), ["B@x.ru", "a@x.ru", "v@x.ru"], "limit соблюдается");
        assert!(merge_results(Vec::new(), vec![person("В", "v@x.ru")], true, 5).is_empty());
    }

    #[test]
    fn card_pick_and_kind() {
        let one = vec![person("Иван", "ivan.primary@x.ru")];
        assert_eq!(pick_card(one, "ivan.alias@x.ru").unwrap().email.as_deref(), Some("ivan.primary@x.ru"), "одна запись — она, даже под другим адресом");
        let many = vec![person("Анна", "anna@x.ru"), person("Иван", "Ivan@X.ru")];
        assert_eq!(pick_card(many.clone(), "ivan@x.ru").unwrap().display_name.as_deref(), Some("Иван"), "по адресу без учёта регистра");
        assert_eq!(pick_card(many, "petr@x.ru"), None, "несколько несовпавших — не угадываем");
        assert_eq!(pick_card(Vec::new(), "petr@x.ru"), None);

        assert_eq!(card_kind(PersonKind::Mailbox, Some(PersonKind::Room)), PersonKind::Room, "переговорная остаётся переговорной");
        assert_eq!(card_kind(PersonKind::Other, Some(PersonKind::Contact)), PersonKind::Contact);
        assert_eq!(card_kind(PersonKind::DistributionList, Some(PersonKind::Mailbox)), PersonKind::DistributionList, "определённый новый вид побеждает");
        assert_eq!(card_kind(PersonKind::Mailbox, None), PersonKind::Mailbox);

        assert!(looks_incomplete(&person("Иван", "i@x.ru")), "ящик без отдела — только из поиска");
        assert!(looks_incomplete(&DirectoryPerson { department: Some("ОГЭ".into()), ..person("Иван", "i@x.ru") }), "кэш без оргструктуры — перечитать");
        assert!(!looks_incomplete(&DirectoryPerson { department: Some("ОГЭ".into()), direct_reports: Some(Vec::new()), ..person("Иван", "i@x.ru") }));
        assert!(!looks_incomplete(&DirectoryPerson { kind: PersonKind::DistributionList, ..person("Все", "all@x.ru") }), "у списка отдела не бывает");
    }

    #[test]
    fn failures_split_into_rejections_and_outages() {
        let server = TransportError::Server { code: "ErrorInvalidRequest".into(), message: "текст с запросом".into() };
        let http500 = TransportError::Protocol { op: "FindPeople".into(), status: 500, raw: String::new() };
        assert!(operation_rejected(&server) && operation_rejected(&http500), "отказ по операции — запасной путь");
        assert!(!operation_rejected(&TransportError::Protocol { op: "x".into(), status: 503, raw: String::new() }), "шлюз недоступен — не отказ операции");
        assert!(!operation_rejected(&TransportError::Server { code: "ErrorServerBusy".into(), message: String::new() }));
        assert!(!operation_rejected(&TransportError::SessionExpired));
        assert!(!Failure::Transport(server).is_outage(), "отказ по запросу — только этот запрос");
        assert!(Failure::Transport(TransportError::Network(Box::new(std::io::Error::other("x")))).is_outage());
        assert!(Failure::Timeout(SERVER_TIMEOUT).is_outage() && Failure::Connect("нет пароля".into()).is_outage());
        assert_eq!(Failure::Timeout(SERVER_TIMEOUT).text(describe_transport), "Сервер не ответил за 10 с, повторите попытку");
        let kind = Failure::Transport(TransportError::Server { code: "ErrorX".into(), message: "Иванов".into() }).kind();
        assert_eq!(kind, "ErrorX", "в журнал — код, без текста сервера");
        assert_eq!(error_kind(&http500), "HTTP 500");
        assert_eq!(describe_expand(&http500), "Сервер не дал развернуть список рассылки (HTTP 500)");
        assert_eq!(describe_expand(&TransportError::Server { code: "ErrorNameResolutionNoResults".into(), message: String::new() }), "Список рассылки не найден в справочнике");
        assert_eq!(describe_expand(&TransportError::Network(Box::new(std::io::Error::other("x")))), "Нет связи с сервером");
    }

    #[test]
    fn unique_emails_only() {
        let got = with_unique_email(vec![person("А", "a@x.ru"), person("А2", "A@x.ru"), person("EX", "/o=Org"), DirectoryPerson::default(), person("Б", "b@x.ru")]);
        assert_eq!(emails(&got), ["a@x.ru", "b@x.ru"]);
    }
}
