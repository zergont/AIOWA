//! Канал уведомлений OWA: конверт `SubscribeToNotification`, разбор ответа подписки и
//! разбор потока `ev.owa2` (блоки `<script>…</script>`, приходящие частями).
//!
//! Формы сняты с браузерной OWA (`fixtures/raw/owa-capture-shapes-2026-10-16.json`) и
//! проверены на сервере, см. `docs/protocol-owa-json.md` § 4 и § 5h. Содержимое писем из
//! событий в журнал не попадает: только типы, ключи и счётчики.

use crate::protocol::{self, str_of};
use aiowa_core::{FolderId, ItemId, Participant};
use aiowa_transport::{NotificationEvent, NotificationKind, SubscriptionRequest, SubscriptionResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::debug;

/// Форма параметров подписки. Первая — как шлёт браузерная OWA (`FolderId` строкой);
/// остальные — запасные на случай отказа сервера по форме.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeForm {
    /// `Parameters{__type: SubscriptionParameters, NotificationType, FolderId: "<строка>", subscriptionIdSuffix: ""}`.
    FolderString,
    /// То же, но `FolderId` объектом `{__type: FolderId, Id}`.
    FolderObject,
    /// Как `FolderString`, но без `__type` у `SubscriptionData` и `Parameters` и без суффикса.
    Untyped,
}

impl SubscribeForm {
    pub const ALL: [SubscribeForm; 3] = [SubscribeForm::FolderString, SubscribeForm::FolderObject, SubscribeForm::Untyped];

    pub fn code(self) -> u8 {
        match self {
            SubscribeForm::FolderString => 1,
            SubscribeForm::FolderObject => 2,
            SubscribeForm::Untyped => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.code() == code)
    }

    pub fn describe(self) -> &'static str {
        match self {
            SubscribeForm::FolderString => "SubscriptionParameters с FolderId строкой (как браузерная OWA)",
            SubscribeForm::FolderObject => "SubscriptionParameters с FolderId объектом",
            SubscribeForm::Untyped => "без __type у SubscriptionData и Parameters",
        }
    }
}

/// Тело `SubscribeToNotification`: OWA-специфичный конверт — параметры лежат на верхнем уровне
/// рядом с объектом `request`, у которого свой `Header` (`RequestServerVersion: Exchange2013`).
pub fn subscribe_body(subs: &[SubscriptionRequest], tz: &str, form: SubscribeForm) -> Value {
    let data: Vec<Value> = subs
        .iter()
        .map(|s| {
            let folder = match &s.kind {
                NotificationKind::FolderRows { folder } | NotificationKind::Calendar { folder } => Some(folder),
                _ => None,
            };
            let mut params = match form {
                SubscribeForm::Untyped => json!({ "NotificationType": s.kind.name() }),
                _ => json!({ "__type": "SubscriptionParameters:#Exchange", "NotificationType": s.kind.name() }),
            };
            if let Some(f) = folder {
                params["FolderId"] = match form {
                    SubscribeForm::FolderObject => json!({ "__type": "FolderId:#Exchange", "Id": f.id }),
                    _ => json!(f.id),
                };
            }
            if form != SubscribeForm::Untyped {
                params["subscriptionIdSuffix"] = json!("");
            }
            match form {
                SubscribeForm::Untyped => json!({ "SubscriptionId": s.id, "Parameters": params }),
                _ => json!({ "__type": "SubscriptionData:#Exchange", "SubscriptionId": s.id, "Parameters": params }),
            }
        })
        .collect();
    json!({
        "request": {
            "__type": "NotificationSubscribeJsonRequest:#Exchange",
            "Header": protocol::request_header(protocol::V_2013, tz),
        },
        "subscriptionData": data,
    })
}

/// Ответ подписки: массив `[{SubscriptionId, SuccessfullyCreated, SubscriptionExists, ErrorInfo}]`.
/// Результат по позициям `subs`; подписка без ответа — отказ «нет ответа».
pub fn parse_subscription_results(v: &Value, subs: &[SubscriptionRequest]) -> Vec<SubscriptionResult> {
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => v.get("subscriptionData").or_else(|| v.get("Body")).and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_else(|| vec![v]),
        _ => vec![],
    };
    subs.iter()
        .map(|s| {
            let found = items.iter().find(|i| str_of(i, "SubscriptionId") == Some(s.id.as_str()));
            match found {
                Some(i) => {
                    let created = protocol::bool_of(i, "SuccessfullyCreated") || protocol::bool_of(i, "SubscriptionExists");
                    let error = match i.get("ErrorInfo") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(e)) if e.trim().is_empty() => None,
                        Some(Value::String(e)) => Some(e.clone()),
                        Some(other) => Some(truncate(&other.to_string(), 300)),
                    };
                    SubscriptionResult { id: s.id.clone(), created, error: if created { None } else { error.or_else(|| Some("сервер не создал подписку".into())) } }
                }
                None => SubscriptionResult { id: s.id.clone(), created: false, error: Some("нет ответа по подписке".into()) },
            }
        })
        .collect()
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// Предел буфера без единого открывающего тега: дальше начало отбрасывается.
const MAX_BUFFER: usize = 1 << 20;

/// Накопитель байтов потока: выдаёт содержимое завершённых блоков `<script>…</script>`.
/// Блок может прийти в нескольких чанках, граница чанка может резать тег и UTF-8.
#[derive(Default)]
pub struct ScriptBlockParser {
    buf: Vec<u8>,
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() || hay.len() - from < needle.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle)).map(|p| p + from)
}

impl ScriptBlockParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Дописывает чанк и возвращает содержимое всех блоков, которые закрылись.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let Some(start) = find(&self.buf, b"<script", 0) else {
                // Тега нет: оставляем хвост на случай, что он пришёл не целиком.
                let keep = self.buf.len().min(7);
                if self.buf.len() > keep {
                    self.buf.drain(..self.buf.len() - keep);
                }
                break;
            };
            let Some(open_end) = find(&self.buf, b">", start) else { break };
            let Some(close) = find(&self.buf, b"</script>", open_end + 1) else {
                if self.buf.len() > MAX_BUFFER {
                    self.buf.drain(..start);
                }
                break;
            };
            out.push(String::from_utf8_lossy(&self.buf[open_end + 1..close]).into_owned());
            self.buf.drain(..close + "</script>".len());
        }
        out
    }

    /// Байт в буфере, ещё не сложившихся в блок.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

/// Значение `data:'…'` из JS-литерала вида `{id:'pg',data:'alive1',mark:'0'}`.
fn js_field<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let pos = s.find(&format!("{key}:"))?;
    let rest = s[pos + key.len() + 1..].trim_start();
    let quote = rest.chars().next()?;
    if quote == '\'' || quote == '"' {
        let inner = &rest[1..];
        let end = inner.find(quote)?;
        Some(&inner[..end])
    } else {
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        Some(rest[..end].trim())
    }
}

/// Служебный блок `{id:'pg', data:'…'}` в событие. Снято с сервера: `alive1` (с `mark:'0'`)
/// сразу после открытия, `noerr` через 40 с, затем `update` каждые 40 с; конец цикла через
/// 320 с — `restart` и `alive0`, после чего сервер закрывает поток (подписки сохраняются,
/// нужен только `FinishNotificationRequest` и новый цикл). Всё это признаки живого потока.
fn control_event(data: &str) -> NotificationEvent {
    let lower = data.to_ascii_lowercase();
    if lower.starts_with("alive") || lower == "noerr" || lower == "update" || lower == "restart" {
        NotificationEvent::KeepAlive
    } else if lower.contains("rld") || lower.contains("reload") {
        NotificationEvent::Reload
    } else {
        NotificationEvent::Unknown { raw_kind: format!("pg:{}", truncate(data, 40)) }
    }
}

/// Разбор одного блока `<script>`: служебный keep-alive, JS-литерал управления или JSON-массив
/// событий. `subs` — известные подписки сессии (вид по идентификатору).
pub fn parse_block(inner: &str, subs: &HashMap<String, NotificationKind>) -> Vec<NotificationEvent> {
    let text = inner.trim();
    if text.is_empty() {
        return vec![];
    }
    let first = text.find(['{', '[']);
    let last = text.rfind(['}', ']']);
    let Some((start, end)) = first.zip(last).filter(|(s, e)| e >= s) else {
        debug!(len = text.chars().count(), "канал: блок без JSON");
        return vec![NotificationEvent::Unknown { raw_kind: "text".into() }];
    };
    let payload = &text[start..=end];
    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in &items {
                out.push(classify_event(item, subs));
            }
            if out.is_empty() {
                debug!("канал: пустой массив событий");
            }
            out
        }
        Ok(v @ Value::Object(_)) => {
            if str_of(&v, "id") == Some("pg") {
                let data = str_of(&v, "data").unwrap_or("");
                // Служебный блок содержимого писем не несёт: его текст целиком идёт в журнал отладки.
                debug!(data, mark = str_of(&v, "mark").unwrap_or(""), raw = truncate(text, 200), "канал: служебный блок");
                return vec![control_event(data)];
            }
            vec![classify_event(&v, subs)]
        }
        Ok(_) => vec![NotificationEvent::Unknown { raw_kind: "scalar".into() }],
        Err(_) => {
            // JS-литерал с одинарными кавычками: {id:'pg',data:'alive1',mark:'0'}
            if js_field(payload, "id").map(|v| v == "pg").unwrap_or(false) {
                let data = js_field(payload, "data").unwrap_or("");
                debug!(data, mark = js_field(payload, "mark").unwrap_or(""), raw = truncate(text, 200), "канал: служебный блок");
                return vec![control_event(data)];
            }
            debug!(len = payload.chars().count(), "канал: блок не разобран");
            vec![NotificationEvent::Unknown { raw_kind: "unparsed".into() }]
        }
    }
}

/// Строка из строки или числа (`EventType` сервер может прислать и так, и так).
fn string_or_number(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Идентификатор папки из строки или объекта `{Id, ChangeKey}`.
fn folder_of(v: &Value, keys: &[&str]) -> Option<FolderId> {
    keys.iter().find_map(|k| match v.get(*k) {
        Some(Value::String(s)) if !s.is_empty() => Some(FolderId { id: s.clone(), change_key: None }),
        Some(o @ Value::Object(_)) => protocol::parse_folder_id(o),
        _ => None,
    })
}

fn item_of(v: &Value, keys: &[&str]) -> Option<ItemId> {
    keys.iter().find_map(|k| match v.get(*k) {
        Some(Value::String(s)) if !s.is_empty() => Some(ItemId { id: s.clone(), change_key: None }),
        Some(o @ Value::Object(_)) => protocol::parse_item_id(o),
        _ => None,
    })
}

fn participant_of(v: &Value, keys: &[&str]) -> Option<Participant> {
    keys.iter().find_map(|k| match v.get(*k) {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            let s = s.trim().to_string();
            if s.contains('@') && !s.contains(' ') { Some(Participant { name: None, email: Some(s), routing_type: None }) } else { Some(Participant { name: Some(s), email: None, routing_type: None }) }
        }
        Some(o @ Value::Object(_)) => protocol::parse_participant(o),
        _ => None,
    })
}

fn string_of(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| str_of(v, k).filter(|s| !s.is_empty()).map(String::from))
}

/// Первое числовое значение среди ключей, содержащих `Count` (сами ключи у разных сборок разные).
fn count_of(v: &Value) -> Option<i64> {
    let o = v.as_object()?;
    for key in ["UnseenCount", "UnseenItemCount", "Count", "UnreadCount"] {
        if let Some(n) = o.get(key).and_then(number_of) {
            return Some(n);
        }
    }
    o.iter().find(|(k, _)| k.to_ascii_lowercase().contains("count")).and_then(|(_, v)| number_of(v))
}

fn number_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Вид подписки по идентификатору события, иначе по `__type`, `NotificationType` или `EventType`.
fn kind_of(v: &Value, id: &str, subs: &HashMap<String, NotificationKind>) -> Option<NotificationKind> {
    if let Some(k) = subs.get(id) {
        return Some(k.clone());
    }
    let hint = [str_of(v, "__type"), str_of(v, "NotificationType"), str_of(v, "EventType")].into_iter().flatten().map(str::to_ascii_lowercase).collect::<Vec<_>>().join(" ");
    if hint.contains("newmail") {
        Some(NotificationKind::NewMail)
    } else if hint.contains("hierarchy") {
        Some(NotificationKind::Hierarchy)
    } else if hint.contains("unseen") {
        Some(NotificationKind::Unseen)
    } else if hint.contains("calendar") {
        folder_of(v, &["FolderId", "ParentFolderId"]).map(|folder| NotificationKind::Calendar { folder })
    } else if hint.contains("row") {
        folder_of(v, &["FolderId", "ParentFolderId"]).map(|folder| NotificationKind::FolderRows { folder })
    } else {
        None
    }
}

/// Одно событие массива: вид по подписке, поля — терпимо к форме. В журнал — тип, `EventType`
/// и ключи объекта (без значений).
pub fn classify_event(v: &Value, subs: &HashMap<String, NotificationKind>) -> NotificationEvent {
    let Some(o) = v.as_object() else {
        return NotificationEvent::Unknown { raw_kind: "scalar".into() };
    };
    let id = string_or_number(o.get("id")).or_else(|| string_or_number(o.get("SubscriptionId"))).unwrap_or_default();
    let event_type = string_or_number(o.get("EventType")).unwrap_or_default();
    let keys: Vec<&str> = o.keys().map(String::as_str).collect();
    let type_name = str_of(v, "__type").unwrap_or("");
    let kind = kind_of(v, &id, subs);
    debug!(type_name, event_type, known_subscription = subs.contains_key(&id), kind = ?kind.as_ref().map(NotificationKind::name), ?keys, "канал: событие");
    match kind {
        Some(NotificationKind::FolderRows { folder }) => NotificationEvent::FolderRows { subscription: id, folder: Some(folder), event_type },
        Some(NotificationKind::NewMail) => NotificationEvent::NewMail {
            subscription: id,
            item_id: item_of(v, &["ItemId", "Id"]),
            folder: folder_of(v, &["FolderId", "ParentFolderId"]).or_else(|| v.get("Item").and_then(|i| folder_of(i, &["ParentFolderId"]))),
            from: participant_of(v, &["Sender", "From", "SenderName", "SenderSmtpAddress"]),
            subject: string_of(v, &["Subject"]),
            preview: string_of(v, &["PreviewText", "Preview"]),
            is_read: v.get("IsRead").and_then(Value::as_bool),
        },
        // Только `FolderId`: `ParentFolderId` у иерархического события — родитель, а не сама папка.
        Some(NotificationKind::Hierarchy) => NotificationEvent::Hierarchy { subscription: id, folder: folder_of(v, &["FolderId"]), event_type },
        Some(NotificationKind::Unseen) => NotificationEvent::Unseen { subscription: id, count: count_of(v) },
        Some(NotificationKind::Calendar { folder }) => NotificationEvent::Calendar { subscription: id, folder: Some(folder) },
        None => {
            let raw_kind = if !type_name.is_empty() {
                type_name.to_string()
            } else if !event_type.is_empty() {
                format!("EventType={event_type}")
            } else {
                format!("keys={}", keys.join(","))
            };
            NotificationEvent::Unknown { raw_kind }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(id: &str, kind: NotificationKind) -> SubscriptionRequest {
        SubscriptionRequest { id: id.into(), kind }
    }

    fn folder(id: &str) -> FolderId {
        FolderId { id: id.into(), change_key: Some("ck".into()) }
    }

    /// Точный JSON конверта подписки: форма браузерной OWA, `__type` первым ключом, `FolderId`
    /// строкой без `ChangeKey`, без `__type` на верхнем уровне.
    #[test]
    fn subscribe_body_exact_json() {
        let subs = [sub("11111111-1111-1111-1111-111111111111", NotificationKind::FolderRows { folder: folder("FOLDER1") }), sub("22222222-2222-2222-2222-222222222222", NotificationKind::NewMail)];
        let body = subscribe_body(&subs, "UTC", SubscribeForm::FolderString);
        let s = serde_json::to_string(&body).unwrap();
        let expected = concat!(
            r#"{"request":{"__type":"NotificationSubscribeJsonRequest:#Exchange","Header":{"__type":"JsonRequestHeaders:#Exchange","RequestServerVersion":"Exchange2013","TimeZoneContext":{"__type":"TimeZoneContext:#Exchange","TimeZoneDefinition":{"__type":"TimeZoneDefinitionType:#Exchange","Id":"UTC"}}}},"#,
            r#""subscriptionData":[{"__type":"SubscriptionData:#Exchange","SubscriptionId":"11111111-1111-1111-1111-111111111111","Parameters":{"__type":"SubscriptionParameters:#Exchange","NotificationType":"RowNotification","FolderId":"FOLDER1","subscriptionIdSuffix":""}},"#,
            r#"{"__type":"SubscriptionData:#Exchange","SubscriptionId":"22222222-2222-2222-2222-222222222222","Parameters":{"__type":"SubscriptionParameters:#Exchange","NotificationType":"NewMailNotification","subscriptionIdSuffix":""}}]}"#
        );
        assert_eq!(s, expected);
    }

    #[test]
    fn subscribe_body_fallback_forms() {
        let subs = [sub("a", NotificationKind::Calendar { folder: folder("CAL") }), sub("b", NotificationKind::Hierarchy), sub("c", NotificationKind::Unseen)];
        let obj = subscribe_body(&subs, "UTC", SubscribeForm::FolderObject);
        assert_eq!(obj.pointer("/subscriptionData/0/Parameters/FolderId"), Some(&json!({ "__type": "FolderId:#Exchange", "Id": "CAL" })));
        assert_eq!(obj.pointer("/subscriptionData/0/Parameters/NotificationType").and_then(Value::as_str), Some("CalendarItemNotification"));
        assert_eq!(obj.pointer("/subscriptionData/1/Parameters/NotificationType").and_then(Value::as_str), Some("HierarchyNotification"));
        assert!(obj.pointer("/subscriptionData/1/Parameters/FolderId").is_none());
        assert_eq!(obj.pointer("/subscriptionData/2/Parameters/NotificationType").and_then(Value::as_str), Some("UnseenItemNotification"));
        let untyped = subscribe_body(&subs, "UTC", SubscribeForm::Untyped);
        assert!(untyped.pointer("/subscriptionData/0/__type").is_none());
        assert!(untyped.pointer("/subscriptionData/0/Parameters/__type").is_none());
        assert!(untyped.pointer("/subscriptionData/0/Parameters/subscriptionIdSuffix").is_none());
        assert_eq!(untyped.pointer("/subscriptionData/0/Parameters/FolderId").and_then(Value::as_str), Some("CAL"));
        assert_eq!(SubscribeForm::from_code(2), Some(SubscribeForm::FolderObject));
        assert_eq!(SubscribeForm::from_code(0), None);
    }

    #[test]
    fn parses_subscription_results_by_id_and_position_gaps() {
        let subs = [sub("a", NotificationKind::NewMail), sub("b", NotificationKind::Hierarchy), sub("c", NotificationKind::Unseen), sub("d", NotificationKind::FolderRows { folder: folder("F") })];
        let v: Value = serde_json::from_str(
            r#"[{"SubscriptionId":"b","SuccessfullyCreated":false,"SubscriptionExists":true,"ErrorInfo":null},
                {"SubscriptionId":"a","SuccessfullyCreated":true,"SubscriptionExists":false,"ErrorInfo":""},
                {"SubscriptionId":"c","SuccessfullyCreated":false,"SubscriptionExists":false,"ErrorInfo":"Тип не поддерживается"}]"#,
        )
        .unwrap();
        let r = parse_subscription_results(&v, &subs);
        assert_eq!(r.len(), 4);
        assert_eq!(r[0], SubscriptionResult { id: "a".into(), created: true, error: None });
        assert_eq!(r[1], SubscriptionResult { id: "b".into(), created: true, error: None }, "существующая подписка считается созданной");
        assert_eq!(r[2], SubscriptionResult { id: "c".into(), created: false, error: Some("Тип не поддерживается".into()) });
        assert!(!r[3].created);
        assert_eq!(r[3].error.as_deref(), Some("нет ответа по подписке"));
    }

    #[test]
    fn script_blocks_split_across_chunks() {
        let mut p = ScriptBlockParser::new();
        assert!(p.push(b"<html><body>\n<scri").is_empty());
        assert!(p.push(b"pt>cp({id:'pg',data:'noerr'});</scr").is_empty());
        let got = p.push(b"ipt>\n<script>cp({id:'pg',data:'alive1',mark:'0'});</script><script>x([{\"EventType\":\"RowAdded\",\"id\":\"s1\",\"Subject\":\"\xD0\xA2");
        assert_eq!(got, vec!["cp({id:'pg',data:'noerr'});".to_string(), "cp({id:'pg',data:'alive1',mark:'0'});".to_string()]);
        // Граница чанка внутри многобайтового символа: блок закрывается следующим чанком целиком.
        let got = p.push("\u{435}\u{43c}\u{430}\"}]);</script>".as_bytes());
        assert_eq!(got, vec!["x([{\"EventType\":\"RowAdded\",\"id\":\"s1\",\"Subject\":\"Тема\"}]);".to_string()]);
        assert_eq!(p.pending(), 0);
        // Тег в другом регистре и мусор между блоками.
        let got = p.push(b"\r\n   <SCRIPT>cp({id:'pg',data:'alive1',mark:'1'});</SCRIPT>");
        assert_eq!(got.len(), 1);
        assert_eq!(p.pending(), 0);
    }

    #[test]
    fn control_blocks() {
        let subs = HashMap::new();
        assert_eq!(parse_block("cp({id:'pg',data:'alive1',mark:'0'});", &subs), vec![NotificationEvent::KeepAlive]);
        assert_eq!(parse_block("cp({id:'pg',data:'noerr'});", &subs), vec![NotificationEvent::KeepAlive]);
        // Форма, снятая с сервера 23.10.2026: литерал без обёртки-вызова.
        assert_eq!(parse_block("{id:'pg',data:'alive1',mark:'0'}", &subs), vec![NotificationEvent::KeepAlive]);
        assert_eq!(parse_block("{id:'pg',data:'noerr'}", &subs), vec![NotificationEvent::KeepAlive]);
        assert_eq!(parse_block("{id:'pg',data:'update'}", &subs), vec![NotificationEvent::KeepAlive]);
        assert_eq!(parse_block("{id:'pg',data:'restart'}", &subs), vec![NotificationEvent::KeepAlive], "конец цикла — не просьба переподписаться");
        assert_eq!(parse_block("{id:'pg',data:'alive0'}", &subs), vec![NotificationEvent::KeepAlive]);
        assert_eq!(parse_block(r#"cp({"id":"pg","data":"alive1","mark":"0"});"#, &subs), vec![NotificationEvent::KeepAlive], "та же форма в JSON");
        assert_eq!(parse_block("cp({id:'pg',data:'rld'});", &subs), vec![NotificationEvent::Reload]);
        assert_eq!(parse_block("cp({id:'pg',data:'err',mark:'3'});", &subs), vec![NotificationEvent::Unknown { raw_kind: "pg:err".into() }]);
        assert_eq!(parse_block("parent.ping();", &subs), vec![NotificationEvent::Unknown { raw_kind: "text".into() }]);
        assert_eq!(parse_block("   ", &subs), vec![]);
        assert_eq!(parse_block("cp({id:'xx',data:zz});", &subs), vec![NotificationEvent::Unknown { raw_kind: "unparsed".into() }]);
    }

    #[test]
    fn event_arrays_by_known_subscription_and_by_type_hint() {
        let mut subs = HashMap::new();
        subs.insert("row-1".to_string(), NotificationKind::FolderRows { folder: folder("INBOX") });
        subs.insert("mail-1".to_string(), NotificationKind::NewMail);
        subs.insert("unseen-1".to_string(), NotificationKind::Unseen);
        subs.insert("cal-1".to_string(), NotificationKind::Calendar { folder: folder("CAL") });
        subs.insert("hier-1".to_string(), NotificationKind::Hierarchy);
        let block = r#"cp([{"__type":"RowNotificationPayload:#Exchange","EventType":"RowAdded","id":"row-1","Item":{"ItemId":{"Id":"I1"}}},
            {"EventType":1,"id":"row-1"},
            {"__type":"NewMailNotificationPayload:#Exchange","EventType":"NewMail","id":"mail-1","ItemId":"ITEM","ConversationId":"CONV","Sender":"Иван Петров","Subject":"Тема","PreviewText":"Начало текста","IsRead":false,"FolderId":"INBOX"},
            {"EventType":"NewMail","id":"mail-1","Sender":"ivan@example.com","Subject":""},
            {"EventType":"UnseenItemNotification","id":"unseen-1","UnseenData":{"x":1},"UnseenCount":"7"},
            {"EventType":"CalendarItemNotification","id":"cal-1"},
            {"EventType":"HierarchyNotification","id":"hier-1","FolderId":"F2"},
            {"__type":"NewMailNotificationPayload:#Exchange","EventType":"NewMail","id":"other","ItemId":{"Id":"I2","ChangeKey":"c"},"From":{"Mailbox":{"Name":"N","EmailAddress":"n@x.ru"}}},
            {"__type":"InstantMessageNotificationPayload:#Exchange","EventType":"0","id":"im"},
            {"EventType":"Reload","id":"row-1"}]);"#;
        let got = parse_block(block, &subs);
        assert_eq!(got.len(), 10);
        assert_eq!(got[0], NotificationEvent::FolderRows { subscription: "row-1".into(), folder: Some(folder("INBOX")), event_type: "RowAdded".into() });
        assert_eq!(got[1], NotificationEvent::FolderRows { subscription: "row-1".into(), folder: Some(folder("INBOX")), event_type: "1".into() }, "числовой EventType");
        match &got[2] {
            NotificationEvent::NewMail { subscription, item_id, folder, from, subject, preview, is_read } => {
                assert_eq!(subscription, "mail-1");
                assert_eq!(item_id.as_ref().map(|i| i.id.as_str()), Some("ITEM"));
                assert_eq!(folder.as_ref().map(|f| f.id.as_str()), Some("INBOX"));
                assert_eq!(from.as_ref().and_then(|p| p.name.as_deref()), Some("Иван Петров"));
                assert_eq!(subject.as_deref(), Some("Тема"));
                assert_eq!(preview.as_deref(), Some("Начало текста"));
                assert_eq!(*is_read, Some(false));
            }
            other => panic!("{other:?}"),
        }
        match &got[3] {
            NotificationEvent::NewMail { from, subject, is_read, .. } => {
                assert_eq!(from.as_ref().and_then(|p| p.email.as_deref()), Some("ivan@example.com"), "строка с @ — адрес");
                assert_eq!(*subject, None, "пустая тема — None");
                assert_eq!(*is_read, None);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(got[4], NotificationEvent::Unseen { subscription: "unseen-1".into(), count: Some(7) });
        assert_eq!(got[5], NotificationEvent::Calendar { subscription: "cal-1".into(), folder: Some(folder("CAL")) });
        assert_eq!(got[6], NotificationEvent::Hierarchy { subscription: "hier-1".into(), folder: Some(FolderId { id: "F2".into(), change_key: None }), event_type: "HierarchyNotification".into() });
        match &got[7] {
            NotificationEvent::NewMail { subscription, item_id, from, .. } => {
                assert_eq!(subscription, "other", "неизвестная подписка — вид по __type");
                assert_eq!(item_id.as_ref().and_then(|i| i.change_key.as_deref()), Some("c"));
                assert_eq!(from.as_ref().and_then(|p| p.email.as_deref()), Some("n@x.ru"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(got[8], NotificationEvent::Unknown { raw_kind: "InstantMessageNotificationPayload:#Exchange".into() });
        assert_eq!(got[9], NotificationEvent::FolderRows { subscription: "row-1".into(), folder: Some(folder("INBOX")), event_type: "Reload".into() }, "Reload по подписке строк — событие папки");
    }

    #[test]
    fn unknown_event_without_hints_names_keys_only() {
        let subs = HashMap::new();
        let got = parse_block(r#"[{"id":"zzz","Foo":"секрет","Bar":1}]"#, &subs);
        assert_eq!(got, vec![NotificationEvent::Unknown { raw_kind: "keys=id,Foo,Bar".into() }]);
        let got = parse_block(r#"[{"id":"zzz","EventType":"Weird"}]"#, &subs);
        assert_eq!(got, vec![NotificationEvent::Unknown { raw_kind: "EventType=Weird".into() }]);
    }
}
