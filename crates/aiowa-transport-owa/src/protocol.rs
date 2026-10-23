//! Сборка запросов и разбор ответов OWA JSON. Запросы собираются через `json!`
//! (в воркспейсе включён `preserve_order`, поэтому `__type` остаётся первым ключом,
//! как требует сервер). Ответы разбираются толерантно: неизвестные поля игнорируются.

use aiowa_core::{
    AttachmentMeta, BodyType, ConversationId, FlagStatus, Folder, FolderChange, FolderId, FolderRef, Importance, ItemChange, ItemId, MessageBody,
    MessageFull, MessageMeta, Participant, SyncState,
};
use aiowa_transport::WriteResult;
use jiff::Timestamp;
use serde_json::{json, Value};

/// Версии схемы, которые браузерная OWA использует для разных операций.
pub const V_2013: &str = "Exchange2013";
pub const V_2016: &str = "V2016_02_03";
pub const V_2017: &str = "V2017_08_18";

/// Поля, которые запрашиваются у элементов при синхронизации с метаданными.
// Имена со стандартными префиксами EWS: без префикса сервер принимает не все
// (`LastModifiedTime` отвергает как неизвестное значение PropertyUriEnum).
pub const META_FIELDS: &[&str] = &[
    "item:Subject",
    "item:DateTimeReceived",
    "item:DateTimeSent",
    "item:LastModifiedTime",
    "message:From",
    "message:Sender",
    "item:DisplayTo",
    "message:IsRead",
    "item:IsDraft",
    "item:HasAttachments",
    "item:Importance",
    "item:Size",
    "item:ItemClass",
    "item:ConversationId",
    "message:InternetMessageId",
    "item:Preview",
    "item:Flag",
    "item:ParentFolderId",
];

pub fn request_header(version: &str, tz: &str) -> Value {
    json!({
        "__type": "JsonRequestHeaders:#Exchange",
        "RequestServerVersion": version,
        "TimeZoneContext": {
            "__type": "TimeZoneContext:#Exchange",
            "TimeZoneDefinition": { "__type": "TimeZoneDefinitionType:#Exchange", "Id": tz }
        }
    })
}

pub fn envelope(op: &str, version: &str, tz: &str, body: Value) -> Value {
    json!({
        "__type": format!("{op}JsonRequest:#Exchange"),
        "Header": request_header(version, tz),
        "Body": body,
    })
}

pub fn property_uri(field: &str) -> Value {
    json!({ "__type": "PropertyUri:#Exchange", "FieldURI": field })
}

pub fn folder_ref_json(f: &FolderRef) -> Value {
    match f {
        FolderRef::Distinguished(d) => json!({ "__type": "DistinguishedFolderId:#Exchange", "Id": d.as_str() }),
        FolderRef::Id(id) => {
            let mut v = json!({ "__type": "FolderId:#Exchange", "Id": id.id });
            if let Some(ck) = &id.change_key {
                v["ChangeKey"] = json!(ck);
            }
            v
        }
    }
}

pub fn target_folder_json(f: &FolderRef) -> Value {
    json!({ "__type": "TargetFolderId:#Exchange", "BaseFolderId": folder_ref_json(f) })
}

pub fn item_id_json(id: &ItemId) -> Value {
    let mut v = json!({ "__type": "ItemId:#Exchange", "Id": id.id });
    if let Some(ck) = &id.change_key {
        v["ChangeKey"] = json!(ck);
    }
    v
}

// ---------------------------------------------------------------------------
// Тела запросов

pub fn sync_folder_hierarchy_body(state: Option<&SyncState>, with_distinguished: bool) -> Value {
    // IdOnly + DisplayName сервер молча игнорирует (имя не приходит), поэтому AllProperties:
    // папок десятки, лишние поля ничего не стоят.
    let mut fields: Vec<&str> = Vec::new();
    if with_distinguished {
        fields.push("DistinguishedFolderId");
    }
    let mut body = json!({
        "__type": "SyncFolderHierarchyRequest:#Exchange",
        "FolderShape": {
            "__type": "FolderResponseShape:#Exchange",
            "BaseShape": "AllProperties",
            "AdditionalProperties": fields.iter().map(|f| property_uri(f)).collect::<Vec<_>>()
        },
        "SyncFolderId": target_folder_json(&FolderRef::Distinguished(aiowa_core::DistinguishedFolder::MsgFolderRoot)),
    });
    if let Some(s) = state {
        body["SyncState"] = json!(s.0);
    }
    body
}

pub fn sync_folder_items_body(folder: &FolderRef, state: Option<&SyncState>, max_changes: u32, with_meta: bool) -> Value {
    let mut shape = json!({ "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly" });
    if with_meta {
        shape["AdditionalProperties"] = Value::Array(META_FIELDS.iter().map(|f| property_uri(f)).collect());
    }
    let mut body = json!({
        "__type": "SyncFolderItemsRequest:#Exchange",
        "ItemShape": shape,
        "SyncFolderId": target_folder_json(folder),
        "MaxChangesReturned": max_changes,
        "SyncScope": "NormalItems",
    });
    if let Some(s) = state {
        body["SyncState"] = json!(s.0);
    }
    body
}

pub fn get_item_body(ids: &[ItemId], shape_name: &str, max_body: u64) -> Value {
    json!({
        "__type": "GetItemRequest:#Exchange",
        "ItemShape": {
            "__type": "ItemResponseShape:#Exchange",
            "BaseShape": "IdOnly",
            "FilterHtmlContent": true,
            "BlockExternalImagesIfSenderUntrusted": true,
            "MaximumBodySize": max_body,
            "MaximumRecipientsToReturn": 200
        },
        "ItemIds": ids.iter().map(item_id_json).collect::<Vec<_>>(),
        "ShapeName": shape_name,
    })
}

/// Операция `GetAttachment` (EWS-схема): содержимое файлового вложения приходит в `Content` как base64.
pub fn get_attachment_body(attachment_id: &str) -> Value {
    json!({
        "__type": "GetAttachmentRequest:#Exchange",
        "AttachmentIds": [ { "__type": "AttachmentId:#Exchange", "Id": attachment_id } ],
        "AttachmentShape": {
            "__type": "AttachmentResponseShape:#Exchange",
            "IncludeMimeContent": false,
            "BodyType": "Best"
        }
    })
}

pub fn get_folder_body(folders: &[FolderRef]) -> Value {
    json!({
        "__type": "GetFolderRequest:#Exchange",
        "FolderIds": folders.iter().map(folder_ref_json).collect::<Vec<_>>(),
        "FolderShape": {
            "__type": "FolderResponseShape:#Exchange",
            "BaseShape": "IdOnly",
            "AdditionalProperties": [ property_uri("UnreadCount"), property_uri("TotalCount") ]
        },
        "ShapeName": null,
    })
}

// ---------------------------------------------------------------------------
// Тела операций записи

/// `UpdateItem` с одним `SetItemField` на каждое письмо. `field` — `FieldURI` (`message:IsRead`,
/// `item:Flag`), `item` — объект `Message:#Exchange` с новым значением поля.
/// `ConflictResolution: AlwaysOverwrite`: локальный `ChangeKey` может отстать от сервера,
/// а операция всё равно должна пройти. `MessageDisposition: SaveOnly` обязателен для писем.
pub fn update_item_body(ids: &[ItemId], field: &str, item: Value) -> Value {
    json!({
        "__type": "UpdateItemRequest:#Exchange",
        "ConflictResolution": "AlwaysOverwrite",
        "MessageDisposition": "SaveOnly",
        "SuppressReadReceipts": true,
        "ItemChanges": ids.iter().map(|id| json!({
            "__type": "ItemChange:#Exchange",
            "ItemId": item_id_json(id),
            "Updates": [ {
                "__type": "SetItemField:#Exchange",
                "Path": property_uri(field),
                "Item": item.clone()
            } ]
        })).collect::<Vec<_>>(),
    })
}

/// `UpdateItem` поля `message:IsRead`.
pub fn set_read_body(ids: &[ItemId], read: bool) -> Value {
    update_item_body(ids, "message:IsRead", json!({ "__type": "Message:#Exchange", "IsRead": read }))
}

/// `UpdateItem` поля `item:Flag`: `Flagged` без дат (как «пометить» в OWA) или `NotFlagged`.
pub fn set_flag_body(ids: &[ItemId], flagged: bool) -> Value {
    let status = if flagged { FlagStatus::Flagged } else { FlagStatus::NotFlagged };
    update_item_body(ids, "item:Flag", json!({ "__type": "Message:#Exchange", "Flag": { "__type": "FlagType:#Exchange", "FlagStatus": status.as_str() } }))
}

/// `MoveItem` в папку; `ReturnNewItemIds` — чтобы в ответе пришли новые идентификаторы.
pub fn move_item_body(ids: &[ItemId], to_folder: &FolderRef) -> Value {
    json!({
        "__type": "MoveItemRequest:#Exchange",
        "ToFolderId": target_folder_json(to_folder),
        "ItemIds": ids.iter().map(item_id_json).collect::<Vec<_>>(),
        "ReturnNewItemIds": true,
    })
}

/// `DeleteItem`: `MoveToDeletedItems` или `HardDelete`.
pub fn delete_item_body(ids: &[ItemId], delete_type: &str) -> Value {
    json!({
        "__type": "DeleteItemRequest:#Exchange",
        "ItemIds": ids.iter().map(item_id_json).collect::<Vec<_>>(),
        "DeleteType": delete_type,
        "SuppressReadReceipts": true,
        "ReturnMovedItemIds": true,
    })
}

// ---------------------------------------------------------------------------
// Разбор ответов

pub fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub fn bool_of(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub fn i64_of(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

pub fn parse_timestamp(s: Option<&str>) -> Option<Timestamp> {
    s.and_then(|s| s.parse::<Timestamp>().ok())
}

pub fn parse_item_id(v: &Value) -> Option<ItemId> {
    let id = str_of(v, "Id")?;
    Some(ItemId { id: id.to_string(), change_key: str_of(v, "ChangeKey").map(String::from) })
}

pub fn parse_folder_id(v: &Value) -> Option<FolderId> {
    let id = str_of(v, "Id")?;
    Some(FolderId { id: id.to_string(), change_key: str_of(v, "ChangeKey").map(String::from) })
}

/// `From`/`Sender` приходят как `{ "Mailbox": {...} }`, получатели как `{EmailAddress, Name, RoutingType}`.
pub fn parse_participant(v: &Value) -> Option<Participant> {
    let m = v.get("Mailbox").unwrap_or(v);
    if !m.is_object() {
        return None;
    }
    let p = Participant {
        name: str_of(m, "Name").map(String::from),
        email: str_of(m, "EmailAddress").map(String::from),
        routing_type: str_of(m, "RoutingType").map(String::from),
    };
    if p.name.is_none() && p.email.is_none() {
        return None;
    }
    Some(p)
}

pub fn parse_participants(v: Option<&Value>) -> Vec<Participant> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(parse_participant).collect()).unwrap_or_default()
}

pub fn parse_message_meta(v: &Value) -> Option<MessageMeta> {
    let item_id = parse_item_id(v.get("ItemId")?)?;
    Some(MessageMeta {
        item_id,
        parent_folder_id: v.get("ParentFolderId").and_then(parse_folder_id),
        conversation_id: v.get("ConversationId").and_then(|c| str_of(c, "Id")).map(|s| ConversationId(s.to_string())),
        internet_message_id: str_of(v, "InternetMessageId").map(String::from),
        item_class: str_of(v, "ItemClass").map(String::from),
        subject: str_of(v, "Subject").map(String::from),
        from: v.get("From").and_then(parse_participant),
        sender: v.get("Sender").and_then(parse_participant),
        display_to: str_of(v, "DisplayTo").map(String::from),
        received_at: parse_timestamp(str_of(v, "DateTimeReceived")),
        sent_at: parse_timestamp(str_of(v, "DateTimeSent")),
        last_modified_at: parse_timestamp(str_of(v, "LastModifiedTime")),
        is_read: bool_of(v, "IsRead"),
        is_draft: bool_of(v, "IsDraft"),
        has_attachments: bool_of(v, "HasAttachments"),
        importance: str_of(v, "Importance").map(Importance::parse).unwrap_or_default(),
        flag: v.get("Flag").and_then(|f| str_of(f, "FlagStatus")).map(FlagStatus::parse).unwrap_or_default(),
        size: i64_of(v, "Size"),
        preview: str_of(v, "Preview").map(String::from),
    })
}

fn parse_body(v: &Value) -> Option<(BodyType, String, bool)> {
    let value = str_of(v, "Value")?;
    let body_type = str_of(v, "BodyType").map(BodyType::parse).unwrap_or(BodyType::Html);
    Some((body_type, value.to_string(), bool_of(v, "IsTruncated")))
}

pub fn parse_message_full(v: &Value) -> Option<MessageFull> {
    let meta = parse_message_meta(v)?;
    let unique = v.get("UniqueBody").and_then(parse_body);
    let full = v.get("NormalizedBody").or_else(|| v.get("Body")).and_then(parse_body);
    let body = match (&unique, &full) {
        (None, None) => None,
        _ => Some(MessageBody {
            body_type: unique.as_ref().or(full.as_ref()).map(|b| b.0).unwrap_or(BodyType::Html),
            unique_body: unique.as_ref().map(|b| b.1.clone()),
            full_body: full.as_ref().map(|b| b.1.clone()),
            is_truncated: unique.as_ref().map(|b| b.2).unwrap_or(false) || full.as_ref().map(|b| b.2).unwrap_or(false),
        }),
    };
    let attachments = v
        .get("Attachments")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|att| {
                    let id = att.get("AttachmentId").and_then(|i| str_of(i, "Id"))?;
                    Some(AttachmentMeta {
                        id: id.to_string(),
                        name: str_of(att, "Name").map(String::from),
                        content_type: str_of(att, "ContentType").map(String::from),
                        size: i64_of(att, "Size"),
                        is_inline: bool_of(att, "IsInline"),
                        content_id: str_of(att, "ContentId").map(String::from),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(MessageFull {
        meta,
        to: parse_participants(v.get("ToRecipients")),
        cc: parse_participants(v.get("CcRecipients")),
        bcc: parse_participants(v.get("BccRecipients")),
        body,
        attachments,
    })
}

pub fn parse_folder(v: &Value) -> Option<Folder> {
    let id = parse_folder_id(v.get("FolderId")?)?;
    Some(Folder {
        id,
        parent_id: v.get("ParentFolderId").and_then(parse_folder_id),
        display_name: str_of(v, "DisplayName").unwrap_or("").to_string(),
        distinguished: str_of(v, "DistinguishedFolderId").filter(|s| !s.is_empty() && *s != "none").map(String::from),
        folder_class: str_of(v, "FolderClass").map(String::from),
        total_count: i64_of(v, "TotalCount").unwrap_or(0),
        unread_count: i64_of(v, "UnreadCount").unwrap_or(0),
        child_folder_count: i64_of(v, "ChildFolderCount").unwrap_or(0),
    })
}

/// Массив изменений из `Changes.Changes[]` (OWA кладёт массив внутрь объекта).
pub fn changes_array(msg: &Value) -> Vec<&Value> {
    match msg.get("Changes") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(Value::Object(o)) => o.get("Changes").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub fn parse_item_changes(msg: &Value, requested: u32) -> (SyncState, Vec<ItemChange>, bool) {
    let state = SyncState(str_of(msg, "SyncState").unwrap_or("").to_string());
    let raw = changes_array(msg);
    let mut out = Vec::with_capacity(raw.len());
    for ch in &raw {
        let kind = str_of(ch, "ChangeType").unwrap_or("");
        match kind {
            "Create" | "Update" => {
                if let Some(meta) = ch.get("Item").and_then(parse_message_meta) {
                    out.push(if kind == "Create" { ItemChange::Create(meta) } else { ItemChange::Update(meta) });
                }
            }
            "Delete" => {
                if let Some(id) = ch.get("ItemId").and_then(parse_item_id).or_else(|| ch.get("Item").and_then(|i| i.get("ItemId")).and_then(parse_item_id)) {
                    out.push(ItemChange::Delete(id));
                }
            }
            "ReadFlagChange" => {
                if let Some(id) = ch.get("ItemId").and_then(parse_item_id) {
                    out.push(ItemChange::ReadFlag { item_id: id, is_read: bool_of(ch, "IsRead") });
                }
            }
            _ => {}
        }
    }
    let includes_last = msg.get("IncludesLastItemInRange").and_then(Value::as_bool).unwrap_or(raw.len() < requested as usize);
    (state, out, includes_last)
}

pub fn parse_folder_changes(msg: &Value) -> (SyncState, Vec<FolderChange>, bool) {
    let state = SyncState(str_of(msg, "SyncState").unwrap_or("").to_string());
    let raw = changes_array(msg);
    let mut out = Vec::with_capacity(raw.len());
    for ch in &raw {
        let kind = str_of(ch, "ChangeType").unwrap_or("");
        match kind {
            "Create" | "Update" => {
                if let Some(f) = ch.get("Folder").and_then(parse_folder) {
                    out.push(if kind == "Create" { FolderChange::Create(f) } else { FolderChange::Update(f) });
                }
            }
            "Delete" => {
                if let Some(id) = ch.get("FolderId").and_then(parse_folder_id) {
                    out.push(FolderChange::Delete(id));
                }
            }
            _ => {}
        }
    }
    let includes_last = msg.get("IncludesLastFolderInRange").and_then(Value::as_bool).unwrap_or(true);
    (state, out, includes_last)
}

/// Первое сообщение ответа: `Body.ResponseMessages.Items[i]`.
pub fn response_messages(v: &Value) -> Vec<&Value> {
    v.pointer("/Body/ResponseMessages/Items").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default()
}

/// Идентификатор письма после операции записи из сообщения ответа: `Items[0].ItemId`
/// (`UpdateItem`, `MoveItem` с `ReturnNewItemIds`), либо `MovedItemId`/`MovedItemIds[0]`
/// (`DeleteItem` с `ReturnMovedItemIds`, форма не снята с сервера). Нет ни того ни другого — `None`.
pub fn written_item_id(msg: &Value) -> Option<ItemId> {
    msg.pointer("/Items/0/ItemId")
        .and_then(parse_item_id)
        .or_else(|| msg.get("MovedItemId").and_then(parse_item_id))
        .or_else(|| msg.pointer("/MovedItemIds/0").and_then(parse_item_id))
}

/// Разбор ответа операции записи по элементам. Сервер отвечает одним `ResponseMessage` на каждый
/// `ItemId` в порядке запроса. Если число сообщений не сошлось, успешные сопоставляются по
/// идентификатору; единственное сообщение-ошибка на пачку относится ко всем элементам;
/// элементы без ответа получают отказ `NoResponse`.
pub fn parse_write_results(msgs: &[&Value], ids: &[ItemId]) -> Vec<WriteResult> {
    let mut parsed: Vec<Option<WriteResult>> = vec![None; ids.len()];
    let by_position = msgs.len() == ids.len();
    if !by_position && msgs.len() == 1
        && let Some((code, message)) = message_error(msgs[0])
    {
        return ids.iter().map(|id| WriteResult::Failed { id: id.clone(), code: code.clone(), message: message.clone() }).collect();
    }
    for (pos, msg) in msgs.iter().enumerate() {
        let new_id = written_item_id(msg);
        let idx = if by_position {
            Some(pos)
        } else {
            new_id.as_ref().and_then(|n| ids.iter().position(|c| c.id == n.id))
        };
        let Some(idx) = idx else { continue };
        parsed[idx] = Some(match message_error(msg) {
            Some((code, message)) => WriteResult::Failed { id: ids[idx].clone(), code, message },
            None => WriteResult::Ok { id: new_id.unwrap_or_else(|| ids[idx].clone()) },
        });
    }
    parsed
        .into_iter()
        .enumerate()
        .map(|(i, p)| p.unwrap_or_else(|| WriteResult::Failed { id: ids[i].clone(), code: "NoResponse".into(), message: "сервер не вернул ответ по элементу".into() }))
        .collect()
}

/// Код и текст ошибки сообщения ответа, если `ResponseClass != Success`.
pub fn message_error(msg: &Value) -> Option<(String, String)> {
    let class = str_of(msg, "ResponseClass").unwrap_or("Success");
    if class == "Success" {
        return None;
    }
    Some((str_of(msg, "ResponseCode").unwrap_or("Unknown").to_string(), str_of(msg, "MessageText").unwrap_or("").to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_is_first_key() {
        let env = envelope("GetFolder", V_2013, "Russian Standard Time", get_folder_body(&[FolderRef::Distinguished(aiowa_core::DistinguishedFolder::Inbox)]));
        let s = serde_json::to_string(&env).unwrap();
        assert!(s.starts_with("{\"__type\":\"GetFolderJsonRequest:#Exchange\""), "{s}");
        let body = s.find("\"Body\":{").unwrap();
        assert!(s[body..].starts_with("\"Body\":{\"__type\":\"GetFolderRequest:#Exchange\""), "{s}");
    }

    #[test]
    fn parses_sync_changes() {
        let msg: Value = serde_json::from_str(r#"{"ResponseClass":"Success","ResponseCode":"NoError",
          "Changes":{"Changes":[
            {"__type":"SyncFolderItemsCreateType:#Exchange","ChangeType":"Create","Item":{"__type":"Message:#Exchange","ItemId":{"Id":"A","ChangeKey":"c1"},"Subject":"Тема","IsRead":true,"DateTimeReceived":"2026-10-16T18:38:12+03:00","From":{"Mailbox":{"Name":"Иван","EmailAddress":"i@x.ru","RoutingType":"SMTP"}}}},
            {"__type":"SyncFolderItemsDeleteType:#Exchange","ChangeType":"Delete","ItemId":{"Id":"B","ChangeKey":"c2"}},
            {"__type":"SyncFolderItemsReadFlagType:#Exchange","ChangeType":"ReadFlagChange","ItemId":{"Id":"C"},"IsRead":false}
          ]},"SyncState":"S1"}"#).unwrap();
        let (state, changes, last) = parse_item_changes(&msg, 512);
        assert_eq!(state.0, "S1");
        assert_eq!(changes.len(), 3);
        assert!(last);
        match &changes[0] {
            ItemChange::Create(m) => {
                assert_eq!(m.subject.as_deref(), Some("Тема"));
                assert!(m.is_read);
                assert_eq!(m.from.as_ref().unwrap().email.as_deref(), Some("i@x.ru"));
                assert!(m.received_at.is_some());
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&changes[1], ItemChange::Delete(id) if id.id == "B"));
        assert!(matches!(&changes[2], ItemChange::ReadFlag { is_read: false, .. }));
    }

    #[test]
    fn parses_full_message() {
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"A","ChangeKey":"k"},"Subject":"S",
          "UniqueBody":{"BodyType":"HTML","Value":"<p>x</p>","IsTruncated":false},
          "ToRecipients":[{"Name":"П","EmailAddress":"p@x.ru","RoutingType":"SMTP"}],
          "Attachments":[{"AttachmentId":{"Id":"att1"},"Name":"a.pdf","ContentType":"application/pdf","Size":10,"IsInline":false}]}"#).unwrap();
        let m = parse_message_full(&v).unwrap();
        assert_eq!(m.to.len(), 1);
        assert_eq!(m.attachments[0].name.as_deref(), Some("a.pdf"));
        assert_eq!(m.body.unwrap().unique_body.as_deref(), Some("<p>x</p>"));
    }

    fn ids() -> Vec<ItemId> {
        vec![ItemId { id: "A".into(), change_key: Some("ck1".into()) }, ItemId { id: "B".into(), change_key: None }]
    }

    #[test]
    fn update_item_is_read_body_exact() {
        let body = set_read_body(&ids()[..1], true);
        let s = serde_json::to_string(&body).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"UpdateItemRequest:#Exchange","ConflictResolution":"AlwaysOverwrite","MessageDisposition":"SaveOnly","SuppressReadReceipts":true,"ItemChanges":[{"__type":"ItemChange:#Exchange","ItemId":{"__type":"ItemId:#Exchange","Id":"A","ChangeKey":"ck1"},"Updates":[{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"message:IsRead"},"Item":{"__type":"Message:#Exchange","IsRead":true}}]}]}"#
        );
        let env = envelope("UpdateItem", V_2017, "UTC", body);
        let s = serde_json::to_string(&env).unwrap();
        assert!(s.starts_with(r#"{"__type":"UpdateItemJsonRequest:#Exchange","Header":{"__type":"JsonRequestHeaders:#Exchange","RequestServerVersion":"V2017_08_18""#), "{s}");
    }

    #[test]
    fn update_item_flag_body_exact() {
        let s = serde_json::to_string(&set_flag_body(&ids()[1..], true)).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"UpdateItemRequest:#Exchange","ConflictResolution":"AlwaysOverwrite","MessageDisposition":"SaveOnly","SuppressReadReceipts":true,"ItemChanges":[{"__type":"ItemChange:#Exchange","ItemId":{"__type":"ItemId:#Exchange","Id":"B"},"Updates":[{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:Flag"},"Item":{"__type":"Message:#Exchange","Flag":{"__type":"FlagType:#Exchange","FlagStatus":"Flagged"}}}]}]}"#
        );
        let s = serde_json::to_string(&set_flag_body(&ids()[1..], false)).unwrap();
        assert!(s.ends_with(r#""Item":{"__type":"Message:#Exchange","Flag":{"__type":"FlagType:#Exchange","FlagStatus":"NotFlagged"}}}]}]}"#), "{s}");
    }

    #[test]
    fn move_item_body_exact() {
        let to = FolderRef::Id(FolderId { id: "F1".into(), change_key: None });
        let s = serde_json::to_string(&move_item_body(&ids(), &to)).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"MoveItemRequest:#Exchange","ToFolderId":{"__type":"TargetFolderId:#Exchange","BaseFolderId":{"__type":"FolderId:#Exchange","Id":"F1"}},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"A","ChangeKey":"ck1"},{"__type":"ItemId:#Exchange","Id":"B"}],"ReturnNewItemIds":true}"#
        );
        let s = serde_json::to_string(&move_item_body(&ids()[..1], &FolderRef::Distinguished(aiowa_core::DistinguishedFolder::DeletedItems))).unwrap();
        assert!(s.contains(r#""BaseFolderId":{"__type":"DistinguishedFolderId:#Exchange","Id":"deleteditems"}"#), "{s}");
    }

    #[test]
    fn delete_item_body_exact() {
        let s = serde_json::to_string(&delete_item_body(&ids()[..1], "MoveToDeletedItems")).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"DeleteItemRequest:#Exchange","ItemIds":[{"__type":"ItemId:#Exchange","Id":"A","ChangeKey":"ck1"}],"DeleteType":"MoveToDeletedItems","SuppressReadReceipts":true,"ReturnMovedItemIds":true}"#
        );
        assert!(serde_json::to_string(&delete_item_body(&ids(), "HardDelete")).unwrap().contains(r#""DeleteType":"HardDelete""#));
    }

    #[test]
    fn write_results_by_position_with_new_ids() {
        let v: Value = serde_json::from_str(
            r#"{"Body":{"ResponseMessages":{"Items":[
              {"__type":"UpdateItemResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError","Items":[{"__type":"Message:#Exchange","ItemId":{"__type":"ItemId:#Exchange","Id":"A","ChangeKey":"ck2"}}]},
              {"__type":"UpdateItemResponseMessage:#Exchange","ResponseClass":"Error","ResponseCode":"ErrorItemNotFound","MessageText":"Нет такого","Items":[]}
            ]}}}"#,
        )
        .unwrap();
        let out = parse_write_results(&response_messages(&v), &ids());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], WriteResult::Ok { id: ItemId { id: "A".into(), change_key: Some("ck2".into()) } });
        assert_eq!(out[1], WriteResult::Failed { id: ids()[1].clone(), code: "ErrorItemNotFound".into(), message: "Нет такого".into() });
    }

    #[test]
    fn write_results_without_items_keep_old_id() {
        // DeleteItem: успешное сообщение без Items — идентификатор остаётся исходным.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"__type":"DeleteItemResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError"}]}}}"#).unwrap();
        let out = parse_write_results(&response_messages(&v), &ids()[..1]);
        assert_eq!(out, vec![WriteResult::Ok { id: ids()[0].clone() }]);
        // Форма с MovedItemId (если сервер её присылает) даёт новый идентификатор.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"ResponseClass":"Success","ResponseCode":"NoError","MovedItemId":{"Id":"A2","ChangeKey":"k"}}]}}}"#).unwrap();
        let out = parse_write_results(&response_messages(&v), &ids()[..1]);
        assert_eq!(out, vec![WriteResult::Ok { id: ItemId { id: "A2".into(), change_key: Some("k".into()) } }]);
    }

    #[test]
    fn write_results_count_mismatch() {
        // Одно сообщение-ошибка на пачку из двух: отказ обоим с тем же кодом.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"ResponseClass":"Error","ResponseCode":"ErrorAccessDenied","MessageText":"нельзя"}]}}}"#).unwrap();
        let out = parse_write_results(&response_messages(&v), &ids());
        assert!(out.iter().all(|r| matches!(r, WriteResult::Failed { code, .. } if code == "ErrorAccessDenied")), "{out:?}");
        // Один успешный ответ на пачку из двух: сопоставление по идентификатору, второй без ответа.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"ResponseClass":"Success","ResponseCode":"NoError","Items":[{"ItemId":{"Id":"B","ChangeKey":"k2"}}]}]}}}"#).unwrap();
        let out = parse_write_results(&response_messages(&v), &ids());
        assert!(matches!(&out[0], WriteResult::Failed { code, .. } if code == "NoResponse"), "{out:?}");
        assert_eq!(out[1], WriteResult::Ok { id: ItemId { id: "B".into(), change_key: Some("k2".into()) } });
    }
}
