//! Сборка запросов и разбор ответов OWA JSON. Запросы собираются через `json!`
//! (в воркспейсе включён `preserve_order`, поэтому `__type` остаётся первым ключом,
//! как требует сервер). Ответы разбираются толерантно: неизвестные поля игнорируются.

use aiowa_core::{
    AttachmentMeta, Attendee, BodyType, CalendarEvent, CalendarEventDetails, CalendarItemType, ComposeKind, ConversationId, DistinguishedFolder, FlagStatus, Folder, FolderChange,
    FolderId, FolderRef, FreeBusy, Importance, ItemChange, ItemId, MessageBody, MessageFull, MessageMeta, OutgoingAttachment, OutgoingMessage, Participant, ResponseType, Signature,
    SyncState,
};
use aiowa_transport::WriteResult;
use base64::prelude::*;
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
// Тела операций создания и отправки (см. docs/protocol-owa-json.md § 5f)

/// Получатель письма: `EmailAddress:#Exchange` с `Name`, `EmailAddress`, `RoutingType` (по умолчанию `SMTP`).
pub fn recipient_json(p: &Participant) -> Value {
    let mut v = json!({ "__type": "EmailAddress:#Exchange" });
    if let Some(n) = &p.name {
        v["Name"] = json!(n);
    }
    if let Some(e) = &p.email {
        v["EmailAddress"] = json!(e);
    }
    v["RoutingType"] = json!(p.routing_type.as_deref().unwrap_or("SMTP"));
    v
}

/// Тело письма: `BodyContentType:#Exchange` типа HTML.
pub fn body_content_json(html: &str) -> Value {
    json!({ "__type": "BodyContentType:#Exchange", "BodyType": "HTML", "Value": html })
}

/// Элемент для `CreateItem`: `Message:#Exchange` для нового письма; `ReplyToItem`,
/// `ReplyAllToItem` или `ForwardItem` с `ReferenceItemId` и `NewBodyContent` для ответов и
/// пересылки. У нового письма пустые списки получателей не передаются (так снято на сервере);
/// у ответов и пересылки все три списка передаются всегда, пустой — как `[]`: непереданный
/// список сервер заполняет из оригинала сам, и письмо ушло бы тем, кого пользователь убрал.
/// У нового письма всегда есть `Importance` и `IsRead: true`, у ответов `Importance` только
/// отличная от обычной.
pub fn outgoing_item_json(kind: &ComposeKind, msg: &OutgoingMessage) -> Value {
    let mut item = match kind {
        ComposeKind::New => json!({ "__type": "Message:#Exchange", "Subject": msg.subject, "Body": body_content_json(&msg.body_html) }),
        ComposeKind::Reply { reference } | ComposeKind::ReplyAll { reference } | ComposeKind::Forward { reference } => {
            let ty = match kind {
                ComposeKind::Reply { .. } => "ReplyToItem:#Exchange",
                ComposeKind::ReplyAll { .. } => "ReplyAllToItem:#Exchange",
                _ => "ForwardItem:#Exchange",
            };
            json!({ "__type": ty, "ReferenceItemId": item_id_json(reference), "NewBodyContent": body_content_json(&msg.body_html), "Subject": msg.subject })
        }
    };
    let explicit_empty = !matches!(kind, ComposeKind::New);
    for (key, list) in [("ToRecipients", &msg.to), ("CcRecipients", &msg.cc), ("BccRecipients", &msg.bcc)] {
        if explicit_empty || !list.is_empty() {
            item[key] = Value::Array(list.iter().map(recipient_json).collect());
        }
    }
    if matches!(kind, ComposeKind::New) {
        item["Importance"] = json!(msg.importance.as_str());
        item["IsRead"] = json!(true);
    } else if msg.importance != Importance::Normal {
        item["Importance"] = json!(msg.importance.as_str());
    }
    item
}

/// `CreateItem` черновика: `MessageDisposition: SaveOnly`, папка «Черновики».
pub fn create_item_body(kind: &ComposeKind, msg: &OutgoingMessage) -> Value {
    json!({
        "__type": "CreateItemRequest:#Exchange",
        "MessageDisposition": "SaveOnly",
        "SavedItemFolderId": target_folder_json(&FolderRef::Distinguished(DistinguishedFolder::Drafts)),
        "Items": [ outgoing_item_json(kind, msg) ],
    })
}

/// `CreateAttachment` одного файла к черновику: `FileAttachment:#Exchange`, содержимое в base64.
/// Один файл на запрос: размер тела запроса ограничен размером одного файла (×4/3).
pub fn create_attachment_body(parent: &ItemId, file: &OutgoingAttachment) -> Value {
    json!({
        "__type": "CreateAttachmentRequest:#Exchange",
        "ParentItemId": item_id_json(parent),
        "Attachments": [ {
            "__type": "FileAttachment:#Exchange",
            "Name": file.name,
            "ContentType": file.content_type.as_deref().unwrap_or("application/octet-stream"),
            "IsInline": false,
            "Content": BASE64_STANDARD.encode(&file.bytes),
        } ],
    })
}

/// `SendItem` черновика с сохранением копии в «Отправленных».
pub fn send_item_body(item: &ItemId) -> Value {
    json!({
        "__type": "SendItemRequest:#Exchange",
        "ItemIds": [ item_id_json(item) ],
        "SaveItemToFolder": true,
        "SavedItemFolderId": target_folder_json(&FolderRef::Distinguished(DistinguishedFolder::SentItems)),
    })
}

/// `UpdateItem` черновика целиком: тема, тело, важность и списки получателей. Пустой список
/// снимается `DeleteItemField`, иначе сервер оставил бы старых получателей.
pub fn update_draft_body(item: &ItemId, msg: &OutgoingMessage) -> Value {
    let set = |field: &str, value: Value| json!({ "__type": "SetItemField:#Exchange", "Path": property_uri(field), "Item": value });
    let mut updates = vec![
        set("item:Subject", json!({ "__type": "Message:#Exchange", "Subject": msg.subject })),
        set("item:Body", json!({ "__type": "Message:#Exchange", "Body": body_content_json(&msg.body_html) })),
        set("item:Importance", json!({ "__type": "Message:#Exchange", "Importance": msg.importance.as_str() })),
    ];
    for (field, key, list) in [("message:ToRecipients", "ToRecipients", &msg.to), ("message:CcRecipients", "CcRecipients", &msg.cc), ("message:BccRecipients", "BccRecipients", &msg.bcc)] {
        if list.is_empty() {
            updates.push(json!({ "__type": "DeleteItemField:#Exchange", "Path": property_uri(field) }));
        } else {
            let mut m = json!({ "__type": "Message:#Exchange" });
            m[key] = Value::Array(list.iter().map(recipient_json).collect());
            updates.push(set(field, m));
        }
    }
    json!({
        "__type": "UpdateItemRequest:#Exchange",
        "ConflictResolution": "AlwaysOverwrite",
        "MessageDisposition": "SaveOnly",
        "SuppressReadReceipts": true,
        "ItemChanges": [ { "__type": "ItemChange:#Exchange", "ItemId": item_id_json(item), "Updates": updates } ],
    })
}

/// `GetUserConfiguration` настроек OWA (`OWA.UserOptions`): среди прочего `SignatureHtml` и
/// `AutoAddSignature`. `base` — папка объекта конфигурации: корень ящика `root` (так его читает
/// EWS Managed API) или корень почтовых папок `msgfolderroot`.
pub fn get_user_configuration_body(base: &str) -> Value {
    json!({
        "__type": "GetUserConfigurationRequest:#Exchange",
        "UserConfigurationName": {
            "__type": "UserConfigurationNameType:#Exchange",
            "Name": "OWA.UserOptions",
            "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": base }
        },
        "UserConfigurationProperties": "All",
    })
}

// ---------------------------------------------------------------------------
// Календарь (см. docs/protocol-owa-json.md § 5g)

/// Поля события для списка (`FindItem` с `CalendarView`); у `GetItem` к ним добавляются
/// `CALENDAR_DETAIL_FIELDS`.
pub const CALENDAR_FIELDS: &[&str] = &[
    "item:Subject",
    "calendar:Start",
    "calendar:End",
    "calendar:IsAllDayEvent",
    "calendar:Location",
    "calendar:Organizer",
    "calendar:LegacyFreeBusyStatus",
    "calendar:CalendarItemType",
    "calendar:IsMeeting",
    "calendar:IsCancelled",
    "calendar:MyResponseType",
    "item:Sensitivity",
    "item:HasAttachments",
    "item:LastModifiedTime",
    "calendar:AppointmentState",
];

/// Бит `AppointmentState`: встреча отменена (`IsCancelled` в `FindItem` сервер не отдаёт).
const APPOINTMENT_STATE_CANCELLED: i64 = 0x4;

/// Поля события для панели: участники с ответами, повторение, тело, напоминание.
pub const CALENDAR_DETAIL_FIELDS: &[&str] = &["calendar:RequiredAttendees", "calendar:OptionalAttendees", "calendar:Recurrence", "item:Body", "item:ReminderIsSet", "item:ReminderMinutesBeforeStart"];

/// Момент для окна `CalendarView`: `2026-10-20T00:00:00Z` (`with_zone`) или без `Z`
/// (на случай, если сервер ждёт «наивное» время в поясе контекста, как у `GetCalendarView`).
pub fn calendar_view_date(t: Timestamp, with_zone: bool) -> String {
    let s = t.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    if with_zone { format!("{s}Z") } else { s }
}

/// `FindItem` по папке `calendar` с `CalendarPageView`: сервер разворачивает повторяющиеся
/// встречи в экземпляры, окно `[start, end)`. `max_entries` — `MaxEntriesReturned`, если нужен.
pub fn find_calendar_view_body(start: Timestamp, end: Timestamp, with_zone: bool, max_entries: Option<u32>) -> Value {
    let mut paging = json!({ "__type": "CalendarPageView:#Exchange", "StartDate": calendar_view_date(start, with_zone), "EndDate": calendar_view_date(end, with_zone) });
    if let Some(n) = max_entries {
        paging["MaxEntriesReturned"] = json!(n);
    }
    json!({
        "__type": "FindItemRequest:#Exchange",
        "ItemShape": {
            "__type": "ItemResponseShape:#Exchange",
            "BaseShape": "IdOnly",
            "AdditionalProperties": CALENDAR_FIELDS.iter().map(|f| property_uri(f)).collect::<Vec<_>>()
        },
        "ParentFolderIds": [ folder_ref_json(&FolderRef::Distinguished(DistinguishedFolder::Calendar)) ],
        "Traversal": "Shallow",
        "Paging": paging,
    })
}

/// Запасной путь: операция OWA `GetCalendarView` (её шлёт браузерная OWA при листании
/// календаря): `CalendarId` папки, `RangeStart`/`RangeEnd` без зоны в поясе контекста.
pub fn get_calendar_view_body(start: Timestamp, end: Timestamp) -> Value {
    json!({
        "__type": "GetCalendarViewRequest:#Exchange",
        "CalendarId": target_folder_json(&FolderRef::Distinguished(DistinguishedFolder::Calendar)),
        "RangeStart": calendar_view_date(start, false),
        "RangeEnd": calendar_view_date(end, false),
    })
}

/// Форма `ItemShape` для `GetItem` события.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarItemShape {
    /// `IdOnly` + явный список свойств (`CALENDAR_FIELDS` + `CALENDAR_DETAIL_FIELDS`).
    Explicit,
    /// `AllProperties`: всё, что сервер знает о событии.
    AllProperties,
}

/// `GetItem` одного события: тело HTML, участники, повторение, напоминание.
pub fn get_calendar_item_body(id: &ItemId, shape: CalendarItemShape, max_body: u64) -> Value {
    let mut item_shape = json!({
        "__type": "ItemResponseShape:#Exchange",
        "BaseShape": match shape { CalendarItemShape::Explicit => "IdOnly", CalendarItemShape::AllProperties => "AllProperties" },
        "BodyType": "HTML",
        "FilterHtmlContent": true,
        "BlockExternalImagesIfSenderUntrusted": true,
        "MaximumBodySize": max_body,
        "MaximumRecipientsToReturn": 500
    });
    if shape == CalendarItemShape::Explicit {
        item_shape["AdditionalProperties"] = Value::Array(CALENDAR_FIELDS.iter().chain(CALENDAR_DETAIL_FIELDS).map(|f| property_uri(f)).collect());
    }
    json!({
        "__type": "GetItemRequest:#Exchange",
        "ItemShape": item_shape,
        "ItemIds": [ item_id_json(id) ],
        "ShapeName": null,
    })
}

/// `GetItem` повторяющейся встречи по идентификатору её экземпляра (`RecurringMasterItemId`
/// с `OccurrenceId`): только образец повторения, который у экземпляров отсутствует.
pub fn get_recurring_master_body(occurrence: &ItemId) -> Value {
    let mut id = json!({ "__type": "RecurringMasterItemId:#Exchange", "OccurrenceId": occurrence.id });
    if let Some(ck) = &occurrence.change_key {
        id["ChangeKey"] = json!(ck);
    }
    json!({
        "__type": "GetItemRequest:#Exchange",
        "ItemShape": {
            "__type": "ItemResponseShape:#Exchange",
            "BaseShape": "IdOnly",
            "AdditionalProperties": [ property_uri("calendar:Recurrence") ]
        },
        "ItemIds": [ id ],
        "ShapeName": null,
    })
}

/// Момент из строки сервера: ISO с зоной (`Z`, `+00:00`), а без зоны — как время в UTC
/// (контекст запросов — UTC); дата без времени — полночь UTC.
pub fn parse_calendar_timestamp(s: Option<&str>) -> Option<Timestamp> {
    let s = s?.trim();
    if let Ok(t) = s.parse::<Timestamp>() {
        return Some(t);
    }
    if let Ok(dt) = s.parse::<jiff::civil::DateTime>() {
        return dt.to_zoned(jiff::tz::TimeZone::UTC).ok().map(|z| z.timestamp());
    }
    s.parse::<jiff::civil::Date>().ok().and_then(|d| d.to_zoned(jiff::tz::TimeZone::UTC).ok()).map(|z| z.timestamp())
}

/// Число из поля: JSON-число или строка с числом (EWS-схема описывает их строками).
pub fn num_of(v: &Value, key: &str) -> Option<i64> {
    match v.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Место встречи: строка `Location` или объект (`EnhancedLocation`/`Location {DisplayName}`).
fn location_of(v: &Value) -> Option<String> {
    let raw = match v.get("Location") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(obj) if obj.is_object() => str_of(obj, "DisplayName"),
        _ => None,
    };
    raw.or_else(|| v.get("EnhancedLocation").and_then(|e| str_of(e, "DisplayName"))).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Событие из элемента `CalendarItem:#Exchange` (`FindItem`, `GetCalendarView`, `GetItem`).
/// Без `ItemId`, `Start` или `End` события нет. OWA JSON отдаёт занятость как `FreeBusyType`
/// и свой ответ как `ResponseType` (имена EWS `LegacyFreeBusyStatus`/`MyResponseType` тоже
/// принимаются); отмена — `IsCancelled` либо бит `AppointmentState`.
pub fn parse_calendar_event(v: &Value) -> Option<CalendarEvent> {
    let id = parse_item_id(v.get("ItemId")?)?;
    let start = parse_calendar_timestamp(str_of(v, "Start"))?;
    let end = parse_calendar_timestamp(str_of(v, "End")).unwrap_or(start);
    let appointment_state = num_of(v, "AppointmentState").unwrap_or(0);
    Some(CalendarEvent {
        id,
        subject: str_of(v, "Subject").map(String::from),
        start,
        end,
        is_all_day: bool_of(v, "IsAllDayEvent"),
        location: location_of(v),
        organizer: v.get("Organizer").and_then(parse_participant),
        free_busy: str_of(v, "FreeBusyType").or_else(|| str_of(v, "LegacyFreeBusyStatus")).map(FreeBusy::parse).unwrap_or_default(),
        calendar_type: str_of(v, "CalendarItemType").map(CalendarItemType::parse).unwrap_or_default(),
        is_meeting: bool_of(v, "IsMeeting"),
        is_cancelled: bool_of(v, "IsCancelled") || appointment_state & APPOINTMENT_STATE_CANCELLED != 0,
        my_response: str_of(v, "ResponseType").or_else(|| str_of(v, "MyResponseType")).map(ResponseType::parse).unwrap_or_default(),
        sensitivity: str_of(v, "Sensitivity").map(String::from),
        has_attachments: bool_of(v, "HasAttachments"),
        last_modified: parse_timestamp(str_of(v, "LastModifiedTime")),
    })
}

/// Элементы ответа `FindItem` (`RootFolder.Items[]`) или `GetCalendarView` (`Items[]`) и признак
/// полной выдачи (`RootFolder.IncludesLastItemInRange`; если его нет — считается полной).
pub fn parse_calendar_view(msg: &Value) -> (Vec<CalendarEvent>, bool) {
    let root = msg.get("RootFolder").unwrap_or(msg);
    let items = root.get("Items").and_then(Value::as_array).map(|a| a.iter().filter_map(parse_calendar_event).collect()).unwrap_or_default();
    let includes_last = root.get("IncludesLastItemInRange").and_then(Value::as_bool).unwrap_or(true);
    (items, includes_last)
}

/// Участник встречи: `{ Mailbox: {...}, ResponseType, LastResponseTime }`.
fn parse_attendee(v: &Value, required: bool) -> Option<Attendee> {
    let participant = parse_participant(v)?;
    Some(Attendee { participant, response: str_of(v, "ResponseType").map(ResponseType::parse).unwrap_or_default(), required })
}

fn parse_attendees(v: Option<&Value>, required: bool) -> Vec<Attendee> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(|x| parse_attendee(x, required)).collect()).unwrap_or_default()
}

/// Событие целиком из элемента ответа `GetItem`.
pub fn parse_calendar_details(v: &Value) -> Option<CalendarEventDetails> {
    let event = parse_calendar_event(v)?;
    let body = v.get("Body").or_else(|| v.get("NormalizedBody")).or_else(|| v.get("UniqueBody")).and_then(parse_body).map(|(body_type, value, truncated)| MessageBody {
        body_type,
        unique_body: None,
        full_body: Some(value),
        is_truncated: truncated,
    });
    let mut attendees = parse_attendees(v.get("RequiredAttendees"), true);
    attendees.extend(parse_attendees(v.get("OptionalAttendees"), false));
    let reminder_set = v.get("ReminderIsSet").and_then(Value::as_bool).unwrap_or(true);
    let reminder_minutes = if reminder_set { num_of(v, "ReminderMinutesBeforeStart") } else { None };
    Some(CalendarEventDetails { event, body, attendees, reminder_minutes, recurrence_text: v.get("Recurrence").and_then(recurrence_text) })
}

/// Список дней недели из `DaysOfWeek`: строка через пробел (как в EWS) или массив строк.
fn days_of_week(v: &Value) -> Vec<String> {
    match v.get("DaysOfWeek") {
        Some(Value::String(s)) => s.split_whitespace().map(String::from).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
        _ => Vec::new(),
    }
}

fn day_name_ru(day: &str) -> &str {
    match day {
        "Monday" => "пн",
        "Tuesday" => "вт",
        "Wednesday" => "ср",
        "Thursday" => "чт",
        "Friday" => "пт",
        "Saturday" => "сб",
        "Sunday" => "вс",
        "Day" => "любой день",
        "Weekday" => "будний день",
        "WeekendDay" => "выходной",
        other => other,
    }
}

fn index_name_ru(index: &str) -> &str {
    match index {
        "First" => "первый",
        "Second" => "второй",
        "Third" => "третий",
        "Fourth" => "четвёртый",
        "Last" => "последний",
        other => other,
    }
}

fn month_name_ru(month: &str) -> &str {
    match month {
        "January" => "января",
        "February" => "февраля",
        "March" => "марта",
        "April" => "апреля",
        "May" => "мая",
        "June" => "июня",
        "July" => "июля",
        "August" => "августа",
        "September" => "сентября",
        "October" => "октября",
        "November" => "ноября",
        "December" => "декабря",
        other => other,
    }
}

/// «Каждые N <единиц>» с русским склонением: `forms` — (1, 2–4, 5+), например («день», «дня», «дней»).
fn every_n(n: i64, forms: (&str, &str, &str), each: &str) -> String {
    if n <= 1 {
        return each.to_string();
    }
    let last = n % 10;
    let tens = n % 100;
    let word = if (11..=14).contains(&tens) {
        forms.2
    } else if last == 1 {
        forms.0
    } else if (2..=4).contains(&last) {
        forms.1
    } else {
        forms.2
    };
    format!("Каждые {n} {word}")
}

/// Дата `2026-01-05` из `RecurrenceRange` в виде `05.01.2026`.
fn range_date(v: &Value, key: &str) -> Option<String> {
    let s = str_of(v, key)?;
    let d: jiff::civil::Date = s.get(..10)?.parse().ok()?;
    Some(d.strftime("%d.%m.%Y").to_string())
}

/// Краткое описание повторения из структуры `Recurrence` (`RecurrencePattern` + `RecurrenceRange`):
/// «Каждую неделю: пн, ср, с 05.01.2026 по 30.06.2026». Неизвестный образец — `None`.
pub fn recurrence_text(rec: &Value) -> Option<String> {
    let pattern = rec.get("RecurrencePattern")?;
    let ty = str_of(pattern, "__type").unwrap_or("").split(':').next().unwrap_or("");
    let interval = num_of(pattern, "Interval").unwrap_or(1);
    let days = days_of_week(pattern).iter().map(|d| day_name_ru(d).to_string()).collect::<Vec<_>>().join(", ");
    let mut text = match ty {
        "DailyRecurrence" => every_n(interval, ("день", "дня", "дней"), "Ежедневно"),
        "WeeklyRecurrence" => {
            let base = every_n(interval, ("неделю", "недели", "недель"), "Каждую неделю");
            if days.is_empty() { base } else { format!("{base}: {days}") }
        }
        "AbsoluteMonthlyRecurrence" => {
            let base = every_n(interval, ("месяц", "месяца", "месяцев"), "Ежемесячно");
            match num_of(pattern, "DayOfMonth") {
                Some(d) => format!("{base} {d}-го числа"),
                None => base,
            }
        }
        "RelativeMonthlyRecurrence" => {
            let base = every_n(interval, ("месяц", "месяца", "месяцев"), "Ежемесячно");
            let index = str_of(pattern, "DayOfWeekIndex").map(index_name_ru).unwrap_or("");
            if days.is_empty() { base } else { format!("{base}: {index} {days}").replace(":  ", ": ") }
        }
        "AbsoluteYearlyRecurrence" => {
            let month = str_of(pattern, "Month").map(month_name_ru).unwrap_or("");
            match num_of(pattern, "DayOfMonth") {
                Some(d) => format!("Ежегодно {d} {month}").trim_end().to_string(),
                None => "Ежегодно".to_string(),
            }
        }
        "RelativeYearlyRecurrence" => {
            let month = str_of(pattern, "Month").map(month_name_ru).unwrap_or("");
            let index = str_of(pattern, "DayOfWeekIndex").map(index_name_ru).unwrap_or("");
            format!("Ежегодно: {index} {days} {month}").split_whitespace().collect::<Vec<_>>().join(" ")
        }
        _ => return None,
    };
    if let Some(range) = rec.get("RecurrenceRange") {
        let rty = str_of(range, "__type").unwrap_or("").split(':').next().unwrap_or("");
        if let Some(from) = range_date(range, "StartDate") {
            text.push_str(&format!(", с {from}"));
        }
        match rty {
            "EndDateRecurrence" => {
                if let Some(to) = range_date(range, "EndDate") {
                    text.push_str(&format!(" по {to}"));
                }
            }
            "NumberedRecurrence" => {
                if let Some(n) = num_of(range, "NumberOfOccurrences") {
                    text.push_str(&format!(", всего {n} раз"));
                }
            }
            _ => {}
        }
    }
    Some(text)
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

/// Идентификатор созданного элемента из сообщения ответа `CreateItem`: `Items[0].ItemId`.
pub fn parse_created_item_id(msg: &Value) -> Option<ItemId> {
    msg.pointer("/Items/0/ItemId").and_then(parse_item_id)
}

/// Идентификатор письма-родителя после `CreateAttachment`: `Attachments[0].AttachmentId`
/// содержит `RootItemId` и `RootItemChangeKey` (новый ключ черновика).
pub fn parse_attachment_root(msg: &Value) -> Option<ItemId> {
    let att = msg.pointer("/Attachments/0/AttachmentId")?;
    let id = str_of(att, "RootItemId")?;
    Some(ItemId { id: id.to_string(), change_key: str_of(att, "RootItemChangeKey").map(String::from) })
}

/// Идентификатор созданного вложения из ответа `CreateAttachment`: `Attachments[0].AttachmentId.Id`.
pub fn parse_attachment_id(msg: &Value) -> Option<String> {
    msg.pointer("/Attachments/0/AttachmentId/Id").and_then(Value::as_str).map(String::from)
}

/// Скалярное значение объекта словаря (`UserConfigurationDictionaryObject`): `Value` как строка,
/// число, булево или массив (в схеме EWS `Value` повторяемый, берётся первый элемент), иначе `Values[0]`.
fn dictionary_object_value(obj: &Value) -> Option<String> {
    obj.get("Value")
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Array(a) => a.first().and_then(|x| match x {
                Value::String(s) => Some(s.clone()),
                Value::Bool(b) => Some(b.to_string()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }),
            Value::Bool(b) => Some(b.to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .or_else(|| obj.pointer("/Values/0").and_then(Value::as_str).map(String::from))
}

/// Словарь `UserConfiguration.Dictionary[]` ответа `GetUserConfiguration` как пары ключ → значение
/// (ключ и значение — `DictionaryKey`/`DictionaryValue`, см. `dictionary_object_value`).
pub fn user_configuration_dictionary(msg: &Value) -> Vec<(String, String)> {
    let entries = msg.pointer("/UserConfiguration/Dictionary").and_then(Value::as_array);
    let mut out = Vec::new();
    for e in entries.into_iter().flatten() {
        let Some(key) = e.get("DictionaryKey").and_then(dictionary_object_value) else { continue };
        if let Some(v) = e.get("DictionaryValue").and_then(dictionary_object_value) {
            out.push((key, v));
        }
    }
    out
}

/// Форма JSON без содержимого: строки заменяются на `str(длина)`, массивы показывают длину и
/// форму первого элемента, объекты — ключи. Для журнала отладки и документации протокола.
pub fn json_shape(v: &Value, depth: usize) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(_) => "bool".into(),
        Value::Number(_) => "num".into(),
        Value::String(s) => format!("str({})", s.chars().count()),
        Value::Array(a) => match a.first() {
            None => "[]".into(),
            Some(first) if depth > 0 => format!("[{}: {}]", a.len(), json_shape(first, depth - 1)),
            Some(_) => format!("[{}]", a.len()),
        },
        Value::Object(o) => {
            if depth == 0 {
                return "{…}".into();
            }
            let fields: Vec<String> = o.iter().map(|(k, v)| format!("{k}: {}", json_shape(v, depth - 1))).collect();
            format!("{{{}}}", fields.join(", "))
        }
    }
}

/// Подпись из словаря настроек OWA: `SignatureHtml` и `AutoAddSignature`. Пустая или
/// отсутствующая подпись — `None`.
pub fn parse_user_signature(msg: &Value) -> Option<Signature> {
    let dict = user_configuration_dictionary(msg);
    let get = |k: &str| dict.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str());
    let html = get("SignatureHtml").map(str::trim).filter(|h| !h.is_empty())?;
    let auto_add = get("AutoAddSignature").map(|v| v.eq_ignore_ascii_case("true") || v == "1").unwrap_or(false);
    Some(Signature { html: html.to_string(), auto_add })
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

    fn outgoing() -> OutgoingMessage {
        OutgoingMessage {
            to: vec![Participant { name: Some("Иван".into()), email: Some("i@x.ru".into()), routing_type: None }],
            cc: vec![],
            bcc: vec![Participant { name: None, email: Some("b@x.ru".into()), routing_type: Some("SMTP".into()) }],
            subject: "Тема".into(),
            body_html: "<p>Привет</p>".into(),
            importance: Importance::Normal,
        }
    }

    #[test]
    fn create_item_new_body_exact() {
        let s = serde_json::to_string(&create_item_body(&ComposeKind::New, &outgoing())).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"CreateItemRequest:#Exchange","MessageDisposition":"SaveOnly","SavedItemFolderId":{"__type":"TargetFolderId:#Exchange","BaseFolderId":{"__type":"DistinguishedFolderId:#Exchange","Id":"drafts"}},"Items":[{"__type":"Message:#Exchange","Subject":"Тема","Body":{"__type":"BodyContentType:#Exchange","BodyType":"HTML","Value":"<p>Привет</p>"},"ToRecipients":[{"__type":"EmailAddress:#Exchange","Name":"Иван","EmailAddress":"i@x.ru","RoutingType":"SMTP"}],"BccRecipients":[{"__type":"EmailAddress:#Exchange","EmailAddress":"b@x.ru","RoutingType":"SMTP"}],"Importance":"Normal","IsRead":true}]}"#
        );
        let env = envelope("CreateItem", V_2017, "UTC", create_item_body(&ComposeKind::New, &outgoing()));
        assert!(serde_json::to_string(&env).unwrap().starts_with(r#"{"__type":"CreateItemJsonRequest:#Exchange","Header":{"__type":"JsonRequestHeaders:#Exchange""#));
    }

    #[test]
    fn create_item_reply_forms_exact() {
        let reference = ItemId { id: "R".into(), change_key: Some("rk".into()) };
        let mut msg = outgoing();
        msg.bcc.clear();
        msg.cc = vec![Participant { name: Some("Пётр".into()), email: Some("p@x.ru".into()), routing_type: None }];
        let item = outgoing_item_json(&ComposeKind::ReplyAll { reference: reference.clone() }, &msg);
        assert_eq!(
            serde_json::to_string(&item).unwrap(),
            r#"{"__type":"ReplyAllToItem:#Exchange","ReferenceItemId":{"__type":"ItemId:#Exchange","Id":"R","ChangeKey":"rk"},"NewBodyContent":{"__type":"BodyContentType:#Exchange","BodyType":"HTML","Value":"<p>Привет</p>"},"Subject":"Тема","ToRecipients":[{"__type":"EmailAddress:#Exchange","Name":"Иван","EmailAddress":"i@x.ru","RoutingType":"SMTP"}],"CcRecipients":[{"__type":"EmailAddress:#Exchange","Name":"Пётр","EmailAddress":"p@x.ru","RoutingType":"SMTP"}],"BccRecipients":[]}"#,
            "у ответа пустой список передаётся явно: иначе сервер заполнит его из оригинала"
        );
        let s = serde_json::to_string(&outgoing_item_json(&ComposeKind::Reply { reference: reference.clone() }, &msg)).unwrap();
        assert!(s.starts_with(r#"{"__type":"ReplyToItem:#Exchange","ReferenceItemId":{"#), "{s}");
        assert!(!s.contains("IsRead"), "у ответа нет IsRead: {s}");
        // Убранные пользователем «Кому» и «Копия» у ответа уходят пустыми списками, а не пропуском поля.
        let mut only_bcc = msg.clone();
        only_bcc.to.clear();
        only_bcc.cc.clear();
        only_bcc.bcc = vec![Participant { name: None, email: Some("b@x.ru".into()), routing_type: None }];
        let s = serde_json::to_string(&outgoing_item_json(&ComposeKind::ReplyAll { reference: reference.clone() }, &only_bcc)).unwrap();
        assert!(s.contains(r#""ToRecipients":[],"CcRecipients":[],"BccRecipients":[{"#), "{s}");
        // У нового письма пустые списки по-прежнему не передаются (форма, снятая на сервере).
        let s = serde_json::to_string(&outgoing_item_json(&ComposeKind::New, &only_bcc)).unwrap();
        assert!(!s.contains("ToRecipients") && !s.contains("CcRecipients") && s.contains("BccRecipients"), "{s}");
        msg.importance = Importance::High;
        let s = serde_json::to_string(&outgoing_item_json(&ComposeKind::Forward { reference }, &msg)).unwrap();
        assert!(s.starts_with(r#"{"__type":"ForwardItem:#Exchange","ReferenceItemId":{"#), "{s}");
        assert!(s.ends_with(r#","Importance":"High"}"#), "важность только отличная от обычной: {s}");
        let s = serde_json::to_string(&create_item_body(&ComposeKind::Reply { reference: ItemId { id: "R".into(), change_key: None } }, &msg)).unwrap();
        assert!(s.contains(r#""Items":[{"__type":"ReplyToItem:#Exchange","ReferenceItemId":{"__type":"ItemId:#Exchange","Id":"R"}"#), "{s}");
    }

    #[test]
    fn create_attachment_body_exact() {
        let parent = ItemId { id: "D".into(), change_key: Some("dk".into()) };
        let file = OutgoingAttachment { name: "счёт.txt".into(), content_type: Some("text/plain".into()), bytes: b"hello".to_vec() };
        assert_eq!(
            serde_json::to_string(&create_attachment_body(&parent, &file)).unwrap(),
            r#"{"__type":"CreateAttachmentRequest:#Exchange","ParentItemId":{"__type":"ItemId:#Exchange","Id":"D","ChangeKey":"dk"},"Attachments":[{"__type":"FileAttachment:#Exchange","Name":"счёт.txt","ContentType":"text/plain","IsInline":false,"Content":"aGVsbG8="}]}"#
        );
        let file = OutgoingAttachment { name: "x.bin".into(), content_type: None, bytes: vec![] };
        assert!(serde_json::to_string(&create_attachment_body(&parent, &file)).unwrap().contains(r#""ContentType":"application/octet-stream","IsInline":false,"Content":"""#));
    }

    #[test]
    fn send_item_body_exact() {
        let s = serde_json::to_string(&send_item_body(&ItemId { id: "D".into(), change_key: Some("dk".into()) })).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"SendItemRequest:#Exchange","ItemIds":[{"__type":"ItemId:#Exchange","Id":"D","ChangeKey":"dk"}],"SaveItemToFolder":true,"SavedItemFolderId":{"__type":"TargetFolderId:#Exchange","BaseFolderId":{"__type":"DistinguishedFolderId:#Exchange","Id":"sentitems"}}}"#
        );
    }

    #[test]
    fn update_draft_body_exact() {
        let s = serde_json::to_string(&update_draft_body(&ItemId { id: "D".into(), change_key: Some("dk".into()) }, &outgoing())).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"UpdateItemRequest:#Exchange","ConflictResolution":"AlwaysOverwrite","MessageDisposition":"SaveOnly","SuppressReadReceipts":true,"ItemChanges":[{"__type":"ItemChange:#Exchange","ItemId":{"__type":"ItemId:#Exchange","Id":"D","ChangeKey":"dk"},"Updates":[{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:Subject"},"Item":{"__type":"Message:#Exchange","Subject":"Тема"}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:Body"},"Item":{"__type":"Message:#Exchange","Body":{"__type":"BodyContentType:#Exchange","BodyType":"HTML","Value":"<p>Привет</p>"}}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:Importance"},"Item":{"__type":"Message:#Exchange","Importance":"Normal"}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"message:ToRecipients"},"Item":{"__type":"Message:#Exchange","ToRecipients":[{"__type":"EmailAddress:#Exchange","Name":"Иван","EmailAddress":"i@x.ru","RoutingType":"SMTP"}]}},{"__type":"DeleteItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"message:CcRecipients"}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"message:BccRecipients"},"Item":{"__type":"Message:#Exchange","BccRecipients":[{"__type":"EmailAddress:#Exchange","EmailAddress":"b@x.ru","RoutingType":"SMTP"}]}}]}]}"#
        );
    }

    #[test]
    fn get_user_configuration_body_exact() {
        assert_eq!(
            serde_json::to_string(&get_user_configuration_body("root")).unwrap(),
            r#"{"__type":"GetUserConfigurationRequest:#Exchange","UserConfigurationName":{"__type":"UserConfigurationNameType:#Exchange","Name":"OWA.UserOptions","BaseFolderId":{"__type":"DistinguishedFolderId:#Exchange","Id":"root"}},"UserConfigurationProperties":"All"}"#
        );
        assert!(serde_json::to_string(&get_user_configuration_body("msgfolderroot")).unwrap().contains(r#""Id":"msgfolderroot"}"#));
    }

    #[test]
    fn parses_create_item_and_attachment_responses() {
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"__type":"ItemInfoResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError","Items":[{"__type":"Message:#Exchange","ItemId":{"__type":"ItemId:#Exchange","Id":"D1","ChangeKey":"k1"}}]}]}}}"#).unwrap();
        let msgs = response_messages(&v);
        assert_eq!(parse_created_item_id(msgs[0]), Some(ItemId { id: "D1".into(), change_key: Some("k1".into()) }));
        assert_eq!(parse_attachment_root(msgs[0]), None);
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"__type":"AttachmentInfoResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError","Attachments":[{"__type":"FileAttachment:#Exchange","AttachmentId":{"__type":"AttachmentId:#Exchange","Id":"A1","RootItemId":"D1","RootItemChangeKey":"k2"}}]}]}}}"#).unwrap();
        let msgs = response_messages(&v);
        assert_eq!(parse_attachment_root(msgs[0]), Some(ItemId { id: "D1".into(), change_key: Some("k2".into()) }));
        assert_eq!(parse_attachment_id(msgs[0]).as_deref(), Some("A1"));
        assert_eq!(parse_created_item_id(msgs[0]), None);
        // SendItem: успех без Items, отказ — ResponseClass Error.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"__type":"SendItemResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError"}]}}}"#).unwrap();
        assert!(message_error(response_messages(&v)[0]).is_none());
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"ResponseClass":"Error","ResponseCode":"ErrorItemNotFound","MessageText":"нет"}]}}}"#).unwrap();
        assert_eq!(message_error(response_messages(&v)[0]), Some(("ErrorItemNotFound".into(), "нет".into())));
    }

    #[test]
    fn parses_user_signature_dictionary() {
        let msg: Value = serde_json::from_str(r#"{"ResponseClass":"Success","ResponseCode":"NoError","UserConfiguration":{"__type":"UserConfiguration:#Exchange","UserConfigurationName":{"Name":"OWA.UserOptions"},"Dictionary":[
            {"__type":"UserConfigurationDictionaryEntry:#Exchange","DictionaryKey":{"__type":"UserConfigurationDictionaryObject:#Exchange","Type":"String","Value":"SignatureHtml"},"DictionaryValue":{"__type":"UserConfigurationDictionaryObject:#Exchange","Type":"String","Value":"<div>С уважением</div>"}},
            {"DictionaryKey":{"Type":"String","Value":"AutoAddSignature"},"DictionaryValue":{"Type":"Boolean","Value":"True"}},
            {"DictionaryKey":{"Type":"String","Value":"Other"},"DictionaryValue":{"Type":"StringArray","Values":["a","b"]}}
        ]}}"#).unwrap();
        let dict = user_configuration_dictionary(&msg);
        assert_eq!(dict.len(), 3);
        assert_eq!(dict[2], ("Other".to_string(), "a".to_string()));
        assert_eq!(parse_user_signature(&msg), Some(Signature { html: "<div>С уважением</div>".into(), auto_add: true }));
        // Ключ и значение массивами (в схеме EWS `Value` повторяемый), булево значение, строковое "false".
        let msg: Value = serde_json::from_str(r#"{"UserConfiguration":{"Dictionary":[{"DictionaryKey":{"Value":["SignatureHtml"]},"DictionaryValue":{"Value":["<p>x</p>"]}},{"DictionaryKey":{"Value":"AutoAddSignature"},"DictionaryValue":{"Value":"false"}}]}}"#).unwrap();
        assert_eq!(parse_user_signature(&msg), Some(Signature { html: "<p>x</p>".into(), auto_add: false }));
        let msg: Value = serde_json::from_str(r#"{"UserConfiguration":{"Dictionary":[{"DictionaryKey":{"Value":["SignatureHtml"]},"DictionaryValue":{"Value":["<p>x</p>"]}},{"DictionaryKey":{"Value":["AutoAddSignature"]},"DictionaryValue":{"Value":[true]}}]}}"#).unwrap();
        assert_eq!(parse_user_signature(&msg), Some(Signature { html: "<p>x</p>".into(), auto_add: true }));
        assert_eq!(json_shape(&msg, 5), "{UserConfiguration: {Dictionary: [2: {DictionaryKey: {Value: [1]}, DictionaryValue: {Value: [1]}}]}}");
        assert_eq!(json_shape(&msg, 4), "{UserConfiguration: {Dictionary: [2: {DictionaryKey: {…}, DictionaryValue: {…}}]}}");
        // Нет подписи или она пустая.
        let msg: Value = serde_json::from_str(r#"{"UserConfiguration":{"Dictionary":[{"DictionaryKey":{"Value":"AutoAddSignature"},"DictionaryValue":{"Value":"True"}},{"DictionaryKey":{"Value":"SignatureHtml"},"DictionaryValue":{"Value":"  "}}]}}"#).unwrap();
        assert_eq!(parse_user_signature(&msg), None);
        assert_eq!(parse_user_signature(&json!({"UserConfiguration":{}})), None);
    }

    #[test]
    fn find_calendar_view_body_exact() {
        let start: Timestamp = "2026-10-20T00:00:00Z".parse().unwrap();
        let end: Timestamp = "2026-10-27T00:00:00Z".parse().unwrap();
        let s = serde_json::to_string(&find_calendar_view_body(start, end, true, None)).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"FindItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"IdOnly","AdditionalProperties":[{"__type":"PropertyUri:#Exchange","FieldURI":"item:Subject"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Start"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:End"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:IsAllDayEvent"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Location"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Organizer"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:LegacyFreeBusyStatus"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:CalendarItemType"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:IsMeeting"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:IsCancelled"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:MyResponseType"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:Sensitivity"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:HasAttachments"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:LastModifiedTime"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:AppointmentState"}]},"ParentFolderIds":[{"__type":"DistinguishedFolderId:#Exchange","Id":"calendar"}],"Traversal":"Shallow","Paging":{"__type":"CalendarPageView:#Exchange","StartDate":"2026-10-20T00:00:00Z","EndDate":"2026-10-27T00:00:00Z"}}"#
        );
        let s = serde_json::to_string(&find_calendar_view_body(start, end, false, Some(500))).unwrap();
        assert!(s.ends_with(r#""Paging":{"__type":"CalendarPageView:#Exchange","StartDate":"2026-10-20T00:00:00","EndDate":"2026-10-27T00:00:00","MaxEntriesReturned":500}}"#), "{s}");
        let env = envelope("FindItem", V_2017, "UTC", find_calendar_view_body(start, end, true, None));
        assert!(serde_json::to_string(&env).unwrap().starts_with(r#"{"__type":"FindItemJsonRequest:#Exchange","Header":{"__type":"JsonRequestHeaders:#Exchange""#));
        assert_eq!(
            serde_json::to_string(&get_calendar_view_body(start, end)).unwrap(),
            r#"{"__type":"GetCalendarViewRequest:#Exchange","CalendarId":{"__type":"TargetFolderId:#Exchange","BaseFolderId":{"__type":"DistinguishedFolderId:#Exchange","Id":"calendar"}},"RangeStart":"2026-10-20T00:00:00","RangeEnd":"2026-10-27T00:00:00"}"#
        );
    }

    #[test]
    fn get_calendar_item_body_exact() {
        let id = ItemId { id: "E".into(), change_key: Some("ek".into()) };
        let s = serde_json::to_string(&get_calendar_item_body(&id, CalendarItemShape::Explicit, 4096)).unwrap();
        assert!(s.starts_with(r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"IdOnly","BodyType":"HTML","FilterHtmlContent":true,"BlockExternalImagesIfSenderUntrusted":true,"MaximumBodySize":4096,"MaximumRecipientsToReturn":500,"AdditionalProperties":[{"__type":"PropertyUri:#Exchange","FieldURI":"item:Subject"},"#), "{s}");
        assert!(s.contains(r#"{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:RequiredAttendees"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:OptionalAttendees"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Recurrence"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:Body"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:ReminderIsSet"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:ReminderMinutesBeforeStart"}]},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"E","ChangeKey":"ek"}],"ShapeName":null}"#), "{s}");
        let s = serde_json::to_string(&get_calendar_item_body(&id, CalendarItemShape::AllProperties, 4096)).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"AllProperties","BodyType":"HTML","FilterHtmlContent":true,"BlockExternalImagesIfSenderUntrusted":true,"MaximumBodySize":4096,"MaximumRecipientsToReturn":500},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"E","ChangeKey":"ek"}],"ShapeName":null}"#
        );
        assert_eq!(
            serde_json::to_string(&get_recurring_master_body(&id)).unwrap(),
            r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"IdOnly","AdditionalProperties":[{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Recurrence"}]},"ItemIds":[{"__type":"RecurringMasterItemId:#Exchange","OccurrenceId":"E","ChangeKey":"ek"}],"ShapeName":null}"#
        );
        assert!(serde_json::to_string(&get_recurring_master_body(&ItemId { id: "E".into(), change_key: None })).unwrap().contains(r#""ItemIds":[{"__type":"RecurringMasterItemId:#Exchange","OccurrenceId":"E"}]"#));
    }

    #[test]
    fn parses_calendar_view_and_dates() {
        let msg: Value = serde_json::from_str(r#"{"__type":"FindItemResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError",
          "RootFolder":{"__type":"FindItemParentType:#Exchange","IncludesLastItemInRange":false,"TotalItemsInView":3,"Items":[
            {"__type":"CalendarItem:#Exchange","ItemId":{"__type":"ItemId:#Exchange","Id":"A","ChangeKey":"a1"},"Subject":"Планёрка","Start":"2026-10-20T07:00:00Z","End":"2026-10-20T07:30:00Z","IsAllDayEvent":false,"Location":"  Переговорная 3 ","Organizer":{"__type":"SingleRecipientType:#Exchange","Mailbox":{"Name":"Иван","EmailAddress":"i@x.ru","RoutingType":"SMTP"}},"LegacyFreeBusyStatus":"Busy","CalendarItemType":"Occurrence","IsMeeting":true,"IsCancelled":false,"MyResponseType":"Accept","Sensitivity":"Normal","HasAttachments":false,"LastModifiedTime":"2026-10-01T10:00:00+00:00"},
            {"__type":"CalendarItem:#Exchange","ItemId":{"Id":"B"},"Subject":"Отпуск","Start":"2026-10-21T00:00:00","End":"2026-10-22T00:00:00","IsAllDayEvent":true,"Location":"","LegacyFreeBusyStatus":"OOF","CalendarItemType":"Single","MyResponseType":"Organizer"},
            {"__type":"CalendarItem:#Exchange","ItemId":{"Id":"C"},"Start":"2026-10-22","IsCancelled":true,"LegacyFreeBusyStatus":"Nonsense"},
            {"__type":"CalendarItem:#Exchange","ItemId":{"Id":"D"},"Subject":"без начала"},
            {"__type":"CalendarItem:#Exchange","ItemId":{"Id":"E"},"Start":"2026-10-23T10:00:00Z","End":"2026-10-23T11:00:00Z","FreeBusyType":"Tentative","ResponseType":"Decline","AppointmentState":5,"Location":{"__type":"EnhancedLocation:#Exchange","DisplayName":"Кабинет 7"}}
          ]}}"#).unwrap();
        let (items, last) = parse_calendar_view(&msg);
        assert!(!last);
        assert_eq!(items.len(), 4, "событие без Start пропущено");
        // Имена OWA JSON (снято с сервера): FreeBusyType, ResponseType, отмена битом AppointmentState, место объектом.
        let e = &items[3];
        assert_eq!((e.free_busy, e.my_response, e.is_cancelled, e.location.as_deref()), (FreeBusy::Tentative, ResponseType::Decline, true, Some("Кабинет 7")));
        let a = &items[0];
        assert_eq!((a.id.id.as_str(), a.id.change_key.as_deref()), ("A", Some("a1")));
        assert_eq!(a.subject.as_deref(), Some("Планёрка"));
        assert_eq!(a.start, "2026-10-20T07:00:00Z".parse::<Timestamp>().unwrap());
        assert_eq!(a.end.duration_since(a.start).as_mins(), 30);
        assert_eq!(a.location.as_deref(), Some("Переговорная 3"));
        assert_eq!(a.organizer.as_ref().unwrap().email.as_deref(), Some("i@x.ru"));
        assert_eq!((a.free_busy, a.calendar_type, a.my_response), (FreeBusy::Busy, CalendarItemType::Occurrence, ResponseType::Accept));
        assert!(a.is_meeting && !a.is_cancelled && !a.is_all_day && a.last_modified.is_some());
        let b = &items[1];
        assert!(b.is_all_day && b.location.is_none() && b.organizer.is_none());
        assert_eq!(b.start, "2026-10-21T00:00:00Z".parse::<Timestamp>().unwrap(), "время без зоны читается как UTC");
        assert_eq!(b.end.duration_since(b.start).as_hours(), 24);
        assert_eq!((b.free_busy, b.calendar_type, b.my_response), (FreeBusy::Oof, CalendarItemType::Single, ResponseType::Organizer));
        let c = &items[2];
        assert_eq!(c.start, "2026-10-22T00:00:00Z".parse::<Timestamp>().unwrap(), "дата без времени — полночь UTC");
        assert_eq!(c.end, c.start, "без End конец равен началу");
        assert!(c.is_cancelled);
        assert_eq!((c.free_busy, c.my_response), (FreeBusy::NoData, ResponseType::Unknown));
        // GetCalendarView: элементы прямо в Body без RootFolder, признака полноты нет.
        let body: Value = serde_json::from_str(r#"{"ResponseClass":"Success","ResponseCode":"NoError","Items":[{"ItemId":{"Id":"X"},"Start":"2026-10-20T07:00:00Z","End":"2026-10-20T08:00:00Z"}]}"#).unwrap();
        let (items, last) = parse_calendar_view(&body);
        assert_eq!((items.len(), last), (1, true));
        assert_eq!(calendar_view_date("2026-10-20T00:00:00Z".parse().unwrap(), true), "2026-10-20T00:00:00Z");
        assert_eq!(calendar_view_date("2026-10-20T00:00:00+03:00".parse().unwrap(), false), "2026-10-19T21:00:00");
    }

    #[test]
    fn parses_calendar_details_and_recurrence() {
        let v: Value = serde_json::from_str(r#"{"__type":"CalendarItem:#Exchange","ItemId":{"Id":"A","ChangeKey":"a2"},"Subject":"Планёрка","Start":"2026-10-20T07:00:00Z","End":"2026-10-20T07:30:00Z",
          "Body":{"__type":"BodyContentType:#Exchange","BodyType":"HTML","Value":"<p>Повестка</p>","IsTruncated":false},
          "RequiredAttendees":[{"__type":"AttendeeType:#Exchange","Mailbox":{"Name":"Иван","EmailAddress":"i@x.ru","RoutingType":"SMTP"},"ResponseType":"Accept","LastResponseTime":"2026-10-01T10:00:00Z"},{"Mailbox":{"Name":"Пётр","EmailAddress":"p@x.ru"},"ResponseType":"NoResponseReceived"}],
          "OptionalAttendees":[{"Mailbox":{"EmailAddress":"o@x.ru"},"ResponseType":"Decline"},{"Mailbox":{}}],
          "ReminderIsSet":true,"ReminderMinutesBeforeStart":"15",
          "Recurrence":{"__type":"RecurrenceType:#Exchange","RecurrencePattern":{"__type":"WeeklyRecurrence:#Exchange","Interval":1,"DaysOfWeek":"Monday Wednesday","FirstDayOfWeek":"Monday"},"RecurrenceRange":{"__type":"EndDateRecurrence:#Exchange","StartDate":"2026-01-05+00:00","EndDate":"2026-06-30+00:00"}}}"#).unwrap();
        let d = parse_calendar_details(&v).unwrap();
        assert_eq!(d.event.id.change_key.as_deref(), Some("a2"));
        let body = d.body.unwrap();
        assert_eq!((body.body_type, body.full_body.as_deref(), body.unique_body.is_none(), body.is_truncated), (BodyType::Html, Some("<p>Повестка</p>"), true, false));
        assert_eq!(d.attendees.len(), 3, "участник без имени и адреса пропущен");
        assert_eq!((d.attendees[0].participant.email.as_deref(), d.attendees[0].response, d.attendees[0].required), (Some("i@x.ru"), ResponseType::Accept, true));
        assert_eq!((d.attendees[1].response, d.attendees[1].required), (ResponseType::NoResponseReceived, true));
        assert_eq!((d.attendees[2].participant.email.as_deref(), d.attendees[2].response, d.attendees[2].required), (Some("o@x.ru"), ResponseType::Decline, false));
        assert_eq!(d.reminder_minutes, Some(15), "число строкой, как в схеме EWS");
        assert_eq!(d.recurrence_text.as_deref(), Some("Каждую неделю: пн, ср, с 05.01.2026 по 30.06.2026"));

        // Напоминание выключено, тела нет, участников нет, повторения нет.
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"B"},"Start":"2026-10-20T07:00:00Z","End":"2026-10-20T07:30:00Z","ReminderIsSet":false,"ReminderMinutesBeforeStart":15}"#).unwrap();
        let d = parse_calendar_details(&v).unwrap();
        assert!(d.body.is_none() && d.attendees.is_empty() && d.reminder_minutes.is_none() && d.recurrence_text.is_none());
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"B"},"Start":"2026-10-20T07:00:00Z","End":"2026-10-20T07:30:00Z","ReminderMinutesBeforeStart":30,"UniqueBody":{"BodyType":"Text","Value":"текст"}}"#).unwrap();
        let d = parse_calendar_details(&v).unwrap();
        assert_eq!(d.reminder_minutes, Some(30), "без ReminderIsSet минуты считаются включёнными");
        assert_eq!(d.body.unwrap().body_type, BodyType::Text);

        // Образцы повторения.
        let text = |pattern: &str, range: &str| recurrence_text(&serde_json::from_str::<Value>(&format!(r#"{{"RecurrencePattern":{pattern},"RecurrenceRange":{range}}}"#)).unwrap());
        let no_end = r#"{"__type":"NoEndRecurrence:#Exchange","StartDate":"2026-01-05"}"#;
        assert_eq!(text(r#"{"__type":"DailyRecurrence:#Exchange","Interval":1}"#, no_end).as_deref(), Some("Ежедневно, с 05.01.2026"));
        assert_eq!(text(r#"{"__type":"DailyRecurrence:#Exchange","Interval":"3"}"#, no_end).as_deref(), Some("Каждые 3 дня, с 05.01.2026"));
        assert_eq!(text(r#"{"__type":"WeeklyRecurrence:#Exchange","Interval":2,"DaysOfWeek":["Friday"]}"#, r#"{"__type":"NumberedRecurrence:#Exchange","StartDate":"2026-01-09","NumberOfOccurrences":10}"#).as_deref(), Some("Каждые 2 недели: пт, с 09.01.2026, всего 10 раз"));
        assert_eq!(text(r#"{"__type":"WeeklyRecurrence:#Exchange","Interval":5,"DaysOfWeek":"Monday"}"#, "{}").as_deref(), Some("Каждые 5 недель: пн"));
        assert_eq!(text(r#"{"__type":"WeeklyRecurrence:#Exchange","Interval":21,"DaysOfWeek":"Monday"}"#, "{}").as_deref(), Some("Каждые 21 неделю: пн"));
        assert_eq!(text(r#"{"__type":"WeeklyRecurrence:#Exchange","Interval":12,"DaysOfWeek":"Monday"}"#, "{}").as_deref(), Some("Каждые 12 недель: пн"));
        assert_eq!(text(r#"{"__type":"AbsoluteMonthlyRecurrence:#Exchange","Interval":1,"DayOfMonth":15}"#, "{}").as_deref(), Some("Ежемесячно 15-го числа"));
        assert_eq!(text(r#"{"__type":"AbsoluteMonthlyRecurrence:#Exchange","Interval":2,"DayOfMonth":15}"#, "{}").as_deref(), Some("Каждые 2 месяца 15-го числа"));
        assert_eq!(text(r#"{"__type":"RelativeMonthlyRecurrence:#Exchange","Interval":1,"DaysOfWeek":"Monday","DayOfWeekIndex":"First"}"#, "{}").as_deref(), Some("Ежемесячно: первый пн"));
        assert_eq!(text(r#"{"__type":"AbsoluteYearlyRecurrence:#Exchange","DayOfMonth":8,"Month":"March"}"#, "{}").as_deref(), Some("Ежегодно 8 марта"));
        assert_eq!(text(r#"{"__type":"RelativeYearlyRecurrence:#Exchange","DaysOfWeek":"Friday","DayOfWeekIndex":"Last","Month":"May"}"#, "{}").as_deref(), Some("Ежегодно: последний пт мая"));
        assert_eq!(text(r#"{"__type":"WeeklyRegeneration:#Exchange","Interval":1}"#, "{}"), None, "образцы задач не описываются");
        assert_eq!(recurrence_text(&json!({})), None);
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
