//! Сборка запросов и разбор ответов OWA JSON. Запросы собираются через `json!`
//! (в воркспейсе включён `preserve_order`, поэтому `__type` остаётся первым ключом,
//! как требует сервер). Ответы разбираются толерантно: неизвестные поля игнорируются.

use aiowa_core::{
    response_from_item_class, AttachmentMeta, Attendee, BodyType, CalendarEvent, CalendarEventDetails, CalendarItemChanges, CalendarItemType, ComposeKind, ConversationId, DistinguishedFolder,
    DirectoryPerson, DraftContent, FlagStatus, Folder, FolderChange, FolderId, FolderRef, FreeBusy, Importance, ItemChange, ItemId, MeetingMessageKind, MeetingRequestInfo, MeetingResponse, MessageBody, MessageFull,
    MessageMeta, NewCalendarItem, OutgoingAttachment, OutgoingMessage, Participant, PersonKind, ResponseType, Signature, SyncState, LastVerb,
    ContactAddress, ContactChange, ContactEmail, ContactItem, ContactPhone};
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
    "item:IconIndex",
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

/// Расширенные свойства «последнее действие с письмом»: `PR_LAST_VERB_EXECUTED` (0x1081, код
/// 102 ответ, 103 ответ всем, 104 пересылка) и `PR_LAST_VERB_EXECUTION_TIME` (0x1082). Идут вместе
/// с `META_FIELDS` в `SyncFolderItems`, `FindItem` и `GetItem` формы списка (docs/obkatka.md п. 20).
pub fn verb_extended_props() -> Vec<Value> {
    vec![
        json!({ "__type": "ExtendedPropertyUri:#Exchange", "PropertyTag": "0x1081", "PropertyType": "Integer" }),
        json!({ "__type": "ExtendedPropertyUri:#Exchange", "PropertyTag": "0x1082", "PropertyType": "SystemTime" }),
    ]
}

/// Свойства списка: `META_FIELDS` и расширенные свойства последнего действия.
fn meta_properties() -> Vec<Value> {
    META_FIELDS.iter().map(|f| property_uri(f)).chain(verb_extended_props()).collect()
}

pub fn sync_folder_items_body(folder: &FolderRef, state: Option<&SyncState>, max_changes: u32, with_meta: bool) -> Value {
    let mut shape = json!({ "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly" });
    if with_meta {
        shape["AdditionalProperties"] = Value::Array(meta_properties());
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

/// `SyncFolderItems` по папке контактов (§ 5q): форма `AllProperties` — у контактов нет именованной
/// формы списка. Снято с сервера 30.10.2026: в этой операции полная форма отдаёт только базовые
/// поля элемента (`ItemId`, `ItemClass`, `LastModifiedTime`, `Subject`…) и из контактных —
/// `DisplayName`, `FileAs`, `ContactSource`; адреса, телефоны, компания, заметки и участники группы
/// приходят отдельным `GetItem` с той же формой (`get_contact_items_body`).
pub fn sync_contact_items_body(folder: &FolderRef, state: Option<&SyncState>, max_changes: u32) -> Value {
    let mut body = json!({
        "__type": "SyncFolderItemsRequest:#Exchange",
        "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "AllProperties", "BodyType": "Text" },
        "SyncFolderId": target_folder_json(folder),
        "MaxChangesReturned": max_changes,
        "SyncScope": "NormalItems",
    });
    if let Some(s) = state {
        body["SyncState"] = json!(s.0);
    }
    body
}

/// Свойство, по которому сортируется выборка последних писем папки.
pub const RECENT_SORT_FIELD: &str = "item:DateTimeReceived";

/// `FindItem` последних писем папки (§ 5m): `IdOnly` с теми же свойствами, что у
/// `SyncFolderItems` (`META_FIELDS`), страница `IndexedPageView` от начала со смещением `offset`,
/// сортировка по `DateTimeReceived` по убыванию — свежие первыми.
pub fn find_recent_items_body(folder: &FolderRef, max: u32, offset: u32) -> Value {
    json!({
        "__type": "FindItemRequest:#Exchange",
        "ItemShape": {
            "__type": "ItemResponseShape:#Exchange",
            "BaseShape": "IdOnly",
            "AdditionalProperties": meta_properties()
        },
        "ParentFolderIds": [ folder_ref_json(folder) ],
        "Traversal": "Shallow",
        "Paging": { "__type": "IndexedPageView:#Exchange", "BasePoint": "Beginning", "Offset": offset, "MaxEntriesReturned": max },
        "SortOrder": [ { "__type": "SortResults:#Exchange", "Order": "Descending", "Path": property_uri(RECENT_SORT_FIELD) } ],
    })
}

/// Свойство `Reply-To` письма, которого нет в именованных формах (`ItemPart` и т. п.): запрашивается
/// через `AdditionalProperties` поверх `ShapeName` (§ 5j).
pub const REPLY_TO_FIELD: &str = "message:ReplyTo";

/// `GetItem` писем в именованной форме `shape_name` (`ItemPart`, `MailListItem`,
/// `ItemNormalizedBody`). `with_reply_to` добавляет к форме свойство `message:ReplyTo`
/// (`AdditionalProperties` действуют вместе с `ShapeName`, см. § 5j).
pub fn get_item_body(ids: &[ItemId], shape_name: &str, max_body: u64, with_reply_to: bool) -> Value {
    let mut shape = json!({
        "__type": "ItemResponseShape:#Exchange",
        "BaseShape": "IdOnly",
        "FilterHtmlContent": true,
        "BlockExternalImagesIfSenderUntrusted": true,
        "MaximumBodySize": max_body,
        "MaximumRecipientsToReturn": 200
    });
    if with_reply_to {
        shape["AdditionalProperties"] = json!([property_uri(REPLY_TO_FIELD)]);
    }
    // Форма списка (дозапрос метаданных без тел) — ещё и последнее действие с письмом.
    if shape_name == "MailListItem" {
        shape["AdditionalProperties"] = Value::Array(verb_extended_props());
    }
    json!({
        "__type": "GetItemRequest:#Exchange",
        "ItemShape": shape,
        "ItemIds": ids.iter().map(item_id_json).collect::<Vec<_>>(),
        "ShapeName": shape_name,
    })
}

/// `GetItem` только с идентификаторами (`BaseShape: IdOnly`, без именованной формы): сервер
/// отдаёт `ItemId` с полным `ChangeKey`. Нужен перед `ReferenceItemId` ответа и пересылки и для
/// починки усечённых ключей в базе. Ключи в запрос не кладутся: сервер их не сверяет, а
/// усечённые — лишний повод для отказа.
pub fn get_item_ids_body(ids: &[ItemId]) -> Value {
    json!({
        "__type": "GetItemRequest:#Exchange",
        "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly" },
        "ItemIds": ids.iter().map(|id| json!({ "__type": "ItemId:#Exchange", "Id": id.id })).collect::<Vec<_>>(),
        "ShapeName": null,
    })
}

/// `GetItem` контактов по идентификаторам (§ 5q): форма `AllProperties` с телом текстом — так
/// приходят адреса, телефоны, компания, заметки и участники группы, которых `SyncFolderItems` с
/// той же формой не отдаёт. Ключ изменений не нужен: читается текущая версия.
pub fn get_contact_items_body(ids: &[ItemId]) -> Value {
    json!({
        "__type": "GetItemRequest:#Exchange",
        "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "AllProperties", "BodyType": "Text" },
        "ItemIds": ids.iter().map(|id| json!({ "__type": "ItemId:#Exchange", "Id": id.id })).collect::<Vec<_>>(),
        "ShapeName": null,
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
// Тела операций с папками (см. docs/protocol-owa-json.md § 5j)

/// Класс создаваемых папок: почтовая.
pub const MAIL_FOLDER_CLASS: &str = "IPF.Note";

/// `CreateFolder` одной почтовой папки `display_name` внутри `parent`
/// (`ParentFolderId{BaseFolderId}`, как `TargetFolderId` у `MoveItem`).
pub fn create_folder_body(parent: &FolderRef, display_name: &str) -> Value {
    json!({
        "__type": "CreateFolderRequest:#Exchange",
        "ParentFolderId": target_folder_json(parent),
        "Folders": [ { "__type": "Folder:#Exchange", "DisplayName": display_name, "FolderClass": MAIL_FOLDER_CLASS } ],
    })
}

/// `UpdateFolder` с одним `SetFolderField` поля `folder:DisplayName`.
pub fn rename_folder_body(id: &FolderId, display_name: &str) -> Value {
    json!({
        "__type": "UpdateFolderRequest:#Exchange",
        "FolderChanges": [ {
            "__type": "FolderChange:#Exchange",
            "FolderId": folder_ref_json(&FolderRef::Id(id.clone())),
            "Updates": [ {
                "__type": "SetFolderField:#Exchange",
                "Path": property_uri("folder:DisplayName"),
                "Folder": { "__type": "Folder:#Exchange", "DisplayName": display_name }
            } ]
        } ],
    })
}

/// `MoveFolder` папки в `parent` (`ToFolderId{BaseFolderId}`).
pub fn move_folder_body(id: &FolderId, parent: &FolderRef) -> Value {
    json!({
        "__type": "MoveFolderRequest:#Exchange",
        "ToFolderId": target_folder_json(parent),
        "FolderIds": [ folder_ref_json(&FolderRef::Id(id.clone())) ],
    })
}

/// `DeleteFolder`: `delete_type` — `MoveToDeletedItems` (папка станет подпапкой «Удалённых») или
/// `HardDelete` (навсегда вместе с содержимым).
pub fn delete_folder_body(id: &FolderId, delete_type: &str) -> Value {
    json!({
        "__type": "DeleteFolderRequest:#Exchange",
        "FolderIds": [ folder_ref_json(&FolderRef::Id(id.clone())) ],
        "DeleteType": delete_type,
    })
}

/// Идентификатор папки из сообщения ответа `CreateFolder`/`UpdateFolder`/`MoveFolder`:
/// `Folders[0].FolderId`. Нет — `None` (у `DeleteFolder` папок в ответе нет).
pub fn parse_written_folder_id(msg: &Value) -> Option<FolderId> {
    msg.pointer("/Folders/0/FolderId").and_then(parse_folder_id)
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

/// `UpdateItem` черновика целиком: тема, тело, списки получателей и важность. Пустой список
/// снимается `DeleteItemField`, иначе сервер оставил бы старых получателей. Важность пишется,
/// только если она не обычная (как у ответов в `outgoing_item_json`): у композера выбора важности
/// нет, и `Normal` значит «не трогать» — иначе важность, заданная черновику в OWA, сбросилась бы.
pub fn update_draft_body(item: &ItemId, msg: &OutgoingMessage) -> Value {
    let set = |field: &str, value: Value| json!({ "__type": "SetItemField:#Exchange", "Path": property_uri(field), "Item": value });
    let mut updates = vec![
        set("item:Subject", json!({ "__type": "Message:#Exchange", "Subject": msg.subject })),
        set("item:Body", json!({ "__type": "Message:#Exchange", "Body": body_content_json(&msg.body_html) })),
    ];
    if msg.importance != Importance::Normal {
        updates.push(set("item:Importance", json!({ "__type": "Message:#Exchange", "Importance": msg.importance.as_str() })));
    }
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

// ---------------------------------------------------------------------------
// Черновики: удаление вложений и чтение целиком (см. docs/protocol-owa-json.md § 5k)

/// `DeleteAttachment` по идентификаторам вложений: несколько за один запрос, ответ — по одному
/// сообщению на каждое вложение в порядке запроса, у каждого `RootItemId` с новым ключом письма.
pub fn delete_attachment_body(ids: &[String]) -> Value {
    json!({
        "__type": "DeleteAttachmentRequest:#Exchange",
        "AttachmentIds": ids.iter().map(|id| json!({ "__type": "AttachmentId:#Exchange", "Id": id })).collect::<Vec<_>>(),
    })
}

/// Идентификатор письма-родителя из сообщения ответа `DeleteAttachment`: `RootItemId` — объект с
/// `RootItemId` и `RootItemChangeKey` (атрибуты элемента `m:RootItemId` схемы EWS); терпимо и к
/// форме `{ Id, ChangeKey }`, и к строке с ключом рядом (`RootItemChangeKey` на уровне сообщения).
/// Нет — `None`.
pub fn parse_delete_attachment_root(msg: &Value) -> Option<ItemId> {
    match msg.get("RootItemId")? {
        Value::String(s) if !s.is_empty() => Some(ItemId { id: s.clone(), change_key: str_of(msg, "RootItemChangeKey").map(String::from) }),
        root @ Value::Object(_) => {
            let id = str_of(root, "RootItemId").or_else(|| str_of(root, "Id")).filter(|s| !s.is_empty())?;
            Some(ItemId { id: id.to_string(), change_key: str_of(root, "RootItemChangeKey").or_else(|| str_of(root, "ChangeKey")).map(String::from) })
        }
        _ => None,
    }
}

/// Форма `ItemShape` для `GetItem` черновика.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftShape {
    /// `AllProperties` с `BodyType: HTML`: полное тело в члене `Body`, получатели, вложения.
    AllProperties,
    /// Именованная форма `ItemNormalizedBody` (§ 5b'): полное тело в `NormalizedBody`.
    NormalizedBody,
}

/// `GetItem` одного черновика с полным телом HTML, получателями и вложениями. `AllProperties` —
/// основная форма (у писем даёт `Body`, `ToRecipients`/`CcRecipients`/`BccRecipients`,
/// `Attachments`, § 5b'), `ItemNormalizedBody` — запасная.
pub fn get_draft_body(id: &ItemId, shape: DraftShape, max_body: u64) -> Value {
    match shape {
        DraftShape::NormalizedBody => get_item_body(std::slice::from_ref(id), "ItemNormalizedBody", max_body, false),
        DraftShape::AllProperties => json!({
            "__type": "GetItemRequest:#Exchange",
            "ItemShape": {
                "__type": "ItemResponseShape:#Exchange",
                "BaseShape": "AllProperties",
                "BodyType": "HTML",
                "FilterHtmlContent": true,
                "BlockExternalImagesIfSenderUntrusted": true,
                "MaximumBodySize": max_body,
                "MaximumRecipientsToReturn": 500
            },
            "ItemIds": [ item_id_json(id) ],
            "ShapeName": null,
        }),
    }
}

/// Какой член тела есть у элемента ответа `GetItem`: `Body`, `NormalizedBody`, `UniqueBody` или
/// ни одного (тогда черновик без тела — или форма его не отдала).
pub fn body_key_of(item: &Value) -> Option<&'static str> {
    ["Body", "NormalizedBody", "UniqueBody"].into_iter().find(|k| item.get(k).is_some_and(|b| b.get("Value").is_some()))
}

/// Обычный текст в HTML: экранирование `& < >` и `<div>` на строку (пустая строка — `<div><br></div>`,
/// как в OWA). Нужен, когда сервер отдал тело черновика типом `Text`.
pub fn text_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    for line in text.lines() {
        let escaped: String = line.chars().map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            c => c.to_string(),
        }).collect();
        if escaped.trim().is_empty() {
            out.push_str("<div><br></div>");
        } else {
            out.push_str("<div>");
            out.push_str(&escaped);
            out.push_str("</div>");
        }
    }
    out
}

/// Черновик из элемента ответа `GetItem` (`parse_message_full` плюс сведение тела к HTML): тело —
/// `Body`, иначе `NormalizedBody`, иначе `UniqueBody`; тело типа `Text` переводится в HTML; без
/// тела — пустая строка. Без `ItemId` черновика нет.
pub fn parse_draft_content(item: &Value) -> Option<DraftContent> {
    let full = parse_message_full(item)?;
    let body_html = match &full.body {
        Some(b) => {
            let raw = b.full_body.as_deref().or(b.unique_body.as_deref()).unwrap_or("");
            match b.body_type {
                BodyType::Html => raw.to_string(),
                BodyType::Text => text_to_html(raw),
            }
        }
        None => String::new(),
    };
    Some(DraftContent {
        id: full.meta.item_id,
        subject: full.meta.subject.unwrap_or_default(),
        body_html,
        to: full.to,
        cc: full.cc,
        bcc: full.bcc,
        importance: full.meta.importance,
        attachments: full.attachments,
        is_truncated: full.body.as_ref().is_some_and(|b| b.is_truncated),
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
/// `CALENDAR_DETAIL_FIELDS`. Место запрашивается и как `calendar:Location`, и как
/// `calendar:EnhancedLocation`: член `Location` в OWA JSON — объект `EnhancedLocation`, и
/// заполняет его только второе свойство; по одному `calendar:Location` место не приходит (§ 5i).
pub const CALENDAR_FIELDS: &[&str] = &[
    "item:Subject",
    "calendar:Start",
    "calendar:End",
    "calendar:IsAllDayEvent",
    "calendar:Location",
    "calendar:EnhancedLocation",
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
    "item:ReminderIsSet",
    "item:ReminderMinutesBeforeStart",
];

/// Бит `AppointmentState`: встреча отменена (`IsCancelled` в `FindItem` сервер не отдаёт).
const APPOINTMENT_STATE_CANCELLED: i64 = 0x4;

/// Поля события для панели: участники с ответами, повторение, тело (напоминание уже в `CALENDAR_FIELDS`).
pub const CALENDAR_DETAIL_FIELDS: &[&str] = &["calendar:RequiredAttendees", "calendar:OptionalAttendees", "calendar:Recurrence", "item:Body"];

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

/// Расширенное свойство места встречи `PidLidLocation` (набор `Appointment`, 0x8208, строка):
/// читается и пишется напрямую в хранилище, минуя `calendar:Location`/`EnhancedLocation`.
pub fn location_extended_uri() -> Value {
    json!({ "__type": "ExtendedPropertyUri:#Exchange", "DistinguishedPropertySetId": "Appointment", "PropertyId": 33288, "PropertyType": "String" })
}

/// `GetItem` события для проверки записи: `AllProperties` плюс `PidLidLocation` расширенным
/// свойством — место видно и когда сервер не отдаёт `Location` в JSON.
pub fn get_calendar_probe_body(id: &ItemId) -> Value {
    let mut body = get_calendar_item_body(id, CalendarItemShape::AllProperties, 4096);
    body["ItemShape"]["AdditionalProperties"] = json!([location_extended_uri()]);
    body
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

/// Напоминание из полей элемента: минуты, если `ReminderIsSet` не выключен явно (без
/// `ReminderIsSet` минуты считаются включёнными; число приходит и числом, и строкой).
fn reminder_of(v: &Value) -> Option<i64> {
    match v.get("ReminderIsSet").and_then(Value::as_bool) {
        Some(false) => None,
        _ => num_of(v, "ReminderMinutesBeforeStart"),
    }
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
        reminder_minutes: reminder_of(v),
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
    let reminder_minutes = event.reminder_minutes;
    Some(CalendarEventDetails { event, body, attendees, reminder_minutes, recurrence_text: v.get("Recurrence").and_then(recurrence_text) })
}

// ---------------------------------------------------------------------------
// Календарь: запись и письма о встречах (см. docs/protocol-owa-json.md § 5i)

/// Имя члена занятости в объекте `CalendarItem` запроса. В ответах сервер отдаёт
/// `calendar:LegacyFreeBusyStatus` как `FreeBusyType` (§ 5g); сериализация DataContract
/// симметрична, поэтому и в запросе значение идёт под этим именем. `FieldURI` при этом
/// остаётся `calendar:LegacyFreeBusyStatus`.
pub const FREE_BUSY_KEY: &str = "FreeBusyType";

/// Момент в полях события запроса: ISO в UTC с `Z`, без долей секунды (как сервер отдаёт сам).
pub fn calendar_datetime(t: Timestamp) -> String {
    calendar_view_date(t, true)
}

/// Граница события в запросе: обычно момент UTC с `Z`; у события «весь день» в контексте пояса
/// пользователя (`local_all_day`) — местная полночь календарной даты без зоны
/// (`2026-10-26T00:00:00`), как шлёт браузерная OWA: сервер читает её в поясе `TimeZoneContext`
/// и хранит событие в этом поясе (§ 5i). Дата берётся из момента по `all_day_civil_date`.
pub fn calendar_bound(t: Timestamp, local_all_day: bool) -> String {
    if local_all_day { format!("{}T00:00:00", aiowa_core::all_day_civil_date(t)) } else { calendar_datetime(t) }
}

/// Участник встречи для запроса: `AttendeeType:#Exchange` с `Mailbox` в форме получателя письма.
pub fn attendee_json(p: &Participant) -> Value {
    json!({ "__type": "AttendeeType:#Exchange", "Mailbox": recipient_json(p) })
}

fn attendees_json(list: &[Participant]) -> Value {
    Value::Array(list.iter().map(attendee_json).collect())
}

/// Место после очистки: пустое или из одних пробелов — нет места.
fn clean_location(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Место в объекте `CalendarItem` запроса: член `Location` — объект `EnhancedLocation:#Exchange`
/// с `DisplayName`. Строка в этом члене не десериализуется (HTTP 500), объект с `Annotation` и
/// `PostalAddress` (`PersonaPostalAddress:#Exchange`, `LocationSource: None`) — тоже HTTP 500
/// (§ 5i). Член соответствует свойству `calendar:EnhancedLocation`, а не `calendar:Location`.
/// Пустое `DisplayName` снимает место.
pub fn location_json(display_name: &str) -> Value {
    json!({ "__type": "EnhancedLocation:#Exchange", "DisplayName": display_name })
}

/// Свойство места для `SetItemField`: ему соответствует член `Location` объекта.
pub const LOCATION_FIELD: &str = "calendar:EnhancedLocation";

/// `SetItemField` события: свойство `field` и значение под членом `key` объекта
/// `CalendarItem:#Exchange` (в объекте должен быть ровно один член, иначе
/// `ErrorIncorrectUpdatePropertyCount`).
pub fn set_calendar_field(field: &str, key: &str, value: Value) -> Value {
    let mut item = json!({ "__type": "CalendarItem:#Exchange" });
    item[key] = value;
    json!({ "__type": "SetItemField:#Exchange", "Path": property_uri(field), "Item": item })
}

/// `DeleteItemField` по свойству `field`.
pub fn delete_calendar_field(field: &str) -> Value {
    json!({ "__type": "DeleteItemField:#Exchange", "Path": property_uri(field) })
}

/// Элемент `CalendarItem:#Exchange` для `CreateItem`: тема, тело (если есть), начало и конец
/// (`local_all_day` — местные полуночи дат без зоны, см. `calendar_bound`), признак «весь день»,
/// место (если есть), участники (только непустые списки), напоминание (`ReminderIsSet` всегда,
/// минуты — при включённом), занятость, конфиденциальность.
pub fn calendar_item_json(item: &NewCalendarItem, local_all_day: bool) -> Value {
    let mut v = json!({ "__type": "CalendarItem:#Exchange", "Subject": item.subject });
    if let Some(b) = &item.body_html {
        v["Body"] = body_content_json(b);
    }
    v["Start"] = json!(calendar_bound(item.start, local_all_day));
    v["End"] = json!(calendar_bound(item.end, local_all_day));
    v["IsAllDayEvent"] = json!(item.is_all_day);
    if let Some(l) = clean_location(item.location.as_deref()) {
        v["Location"] = location_json(l);
    }
    if !item.required.is_empty() {
        v["RequiredAttendees"] = attendees_json(&item.required);
    }
    if !item.optional.is_empty() {
        v["OptionalAttendees"] = attendees_json(&item.optional);
    }
    match item.reminder_minutes {
        Some(m) => {
            v["ReminderIsSet"] = json!(true);
            v["ReminderMinutesBeforeStart"] = json!(m);
        }
        None => v["ReminderIsSet"] = json!(false),
    }
    v[FREE_BUSY_KEY] = json!(item.free_busy.as_str());
    v["Sensitivity"] = json!(if item.sensitivity_private { "Private" } else { "Normal" });
    v
}

/// `CreateItem` события в папке `calendar`: с участниками приглашения рассылает сервер
/// (`SendToAllAndSaveCopy`), без них — `SendToNone`. `local_all_day` — запрос идёт в контексте
/// пояса пользователя и событие на весь день: границы местными полуночами дат.
pub fn create_calendar_item_body(item: &NewCalendarItem, local_all_day: bool) -> Value {
    json!({
        "__type": "CreateItemRequest:#Exchange",
        "SendMeetingInvitations": if item.has_attendees() { "SendToAllAndSaveCopy" } else { "SendToNone" },
        "SavedItemFolderId": target_folder_json(&FolderRef::Distinguished(DistinguishedFolder::Calendar)),
        "Items": [ calendar_item_json(item, local_all_day) ],
    })
}

/// `UpdateItem` события с готовым списком `updates` (`set_calendar_field`/`delete_calendar_field`).
/// `ConflictResolution: AlwaysOverwrite`, как у писем. Рассылка обновлений для событий обязательна
/// и в OWA JSON называется **`SendCalendarInvitationsOrCancellations`** (имя схемы EWS
/// `SendMeetingInvitationsOrCancellations` сервер молча игнорирует и отвечает
/// `ErrorSendMeetingInvitationsOrCancellationsRequired`, § 5i).
pub fn update_calendar_item_request(id: &ItemId, updates: Vec<Value>, send_updates: bool) -> Value {
    json!({
        "__type": "UpdateItemRequest:#Exchange",
        "ConflictResolution": "AlwaysOverwrite",
        "SendCalendarInvitationsOrCancellations": if send_updates { "SendToAllAndSaveCopy" } else { "SendToNone" },
        "ItemChanges": [ { "__type": "ItemChange:#Exchange", "ItemId": item_id_json(id), "Updates": updates } ],
    })
}

/// `UpdateItem` события по заполненным полям `changes`: `SetItemField` со значением в объекте
/// `CalendarItem:#Exchange`, опустевшие списки участников — `DeleteItemField`, снятие места —
/// `EnhancedLocation` с пустым `DisplayName`, снятие напоминания — `ReminderIsSet: false`.
/// `local_all_day` — запрос идёт в контексте пояса пользователя и `changes.is_all_day ==
/// Some(true)`: границы местными полуночами дат (`calendar_bound`).
pub fn update_calendar_item_body(id: &ItemId, changes: &CalendarItemChanges, send_updates: bool, local_all_day: bool) -> Value {
    let set = set_calendar_field;
    let del = delete_calendar_field;
    let mut updates: Vec<Value> = Vec::new();
    if let Some(s) = &changes.subject {
        updates.push(set("item:Subject", "Subject", json!(s)));
    }
    if let Some(t) = changes.start {
        updates.push(set("calendar:Start", "Start", json!(calendar_bound(t, local_all_day))));
    }
    if let Some(t) = changes.end {
        updates.push(set("calendar:End", "End", json!(calendar_bound(t, local_all_day))));
    }
    if let Some(b) = changes.is_all_day {
        updates.push(set("calendar:IsAllDayEvent", "IsAllDayEvent", json!(b)));
    }
    if let Some(loc) = &changes.location {
        updates.push(set(LOCATION_FIELD, "Location", location_json(clean_location(loc.as_deref()).unwrap_or(""))));
    }
    if let Some(b) = &changes.body_html {
        updates.push(set("item:Body", "Body", body_content_json(b)));
    }
    for (field, key, list) in [("calendar:RequiredAttendees", "RequiredAttendees", &changes.required), ("calendar:OptionalAttendees", "OptionalAttendees", &changes.optional)] {
        if let Some(list) = list {
            if list.is_empty() {
                updates.push(del(field));
            } else {
                updates.push(set(field, key, attendees_json(list)));
            }
        }
    }
    if let Some(reminder) = changes.reminder_minutes {
        match reminder {
            Some(m) => {
                updates.push(set("item:ReminderIsSet", "ReminderIsSet", json!(true)));
                updates.push(set("item:ReminderMinutesBeforeStart", "ReminderMinutesBeforeStart", json!(m)));
            }
            None => updates.push(set("item:ReminderIsSet", "ReminderIsSet", json!(false))),
        }
    }
    if let Some(fb) = changes.free_busy {
        updates.push(set("calendar:LegacyFreeBusyStatus", FREE_BUSY_KEY, json!(fb.as_str())));
    }
    update_calendar_item_request(id, updates, send_updates)
}

/// `DeleteItem` события: `delete_type` — `MoveToDeletedItems` или `HardDelete`;
/// `SendMeetingCancellations` обязателен для событий (иначе `ErrorSendMeetingCancellationsRequired`).
pub fn delete_calendar_item_body(id: &ItemId, delete_type: &str, send_cancellations: bool) -> Value {
    json!({
        "__type": "DeleteItemRequest:#Exchange",
        "ItemIds": [ item_id_json(id) ],
        "DeleteType": delete_type,
        "SendMeetingCancellations": if send_cancellations { "SendToAllAndSaveCopy" } else { "SendToNone" },
    })
}

/// `CreateItem` с ответом на приглашение: `AcceptItem`, `TentativelyAcceptItem` или `DeclineItem`
/// по `ReferenceItemId` (письмо-приглашение или событие календаря), с телом ответа, если есть.
/// `MessageDisposition`: `SendAndSaveCopy` — ответ уходит организатору, `SaveOnly` — без ответа.
pub fn respond_to_meeting_body(reference: &ItemId, response: MeetingResponse, send_response: bool, body_html: Option<&str>) -> Value {
    let ty = match response {
        MeetingResponse::Accept => "AcceptItem:#Exchange",
        MeetingResponse::Tentative => "TentativelyAcceptItem:#Exchange",
        MeetingResponse::Decline => "DeclineItem:#Exchange",
    };
    let mut item = json!({ "__type": ty, "ReferenceItemId": item_id_json(reference) });
    if let Some(b) = body_html.map(str::trim).filter(|b| !b.is_empty()) {
        item["Body"] = body_content_json(b);
    }
    json!({
        "__type": "CreateItemRequest:#Exchange",
        "MessageDisposition": if send_response { "SendAndSaveCopy" } else { "SaveOnly" },
        "Items": [ item ],
    })
}

/// Поля письма о встрече для `GetItem` в явной форме: класс и тема письма, поля встречи
/// (`calendar:…` действуют и на письмах-приглашениях) и поля `meeting:…` письма.
pub const MEETING_MESSAGE_FIELDS: &[&str] = &[
    "item:ItemClass",
    "item:Subject",
    "item:DateTimeReceived",
    "message:From",
    "calendar:Start",
    "calendar:End",
    "calendar:IsAllDayEvent",
    "calendar:Location",
    "calendar:EnhancedLocation",
    "calendar:Organizer",
    "calendar:MyResponseType",
    "calendar:CalendarItemType",
    "calendar:Recurrence",
    "meeting:IsOutOfDate",
    "meeting:AssociatedCalendarItemId",
    "meeting:ResponseType",
    "meeting:HasBeenProcessed",
    "meeting:IsDelegated",
];

/// `GetItem` письма о встрече: `IdOnly` с явным списком `MEETING_MESSAGE_FIELDS` или
/// `AllProperties` (запасная форма; тело ограничено `max_body`).
pub fn get_meeting_request_body(id: &ItemId, shape: CalendarItemShape, max_body: u64) -> Value {
    let mut item_shape = json!({
        "__type": "ItemResponseShape:#Exchange",
        "BaseShape": match shape { CalendarItemShape::Explicit => "IdOnly", CalendarItemShape::AllProperties => "AllProperties" },
        "BodyType": "HTML",
        "FilterHtmlContent": true,
        "BlockExternalImagesIfSenderUntrusted": true,
        "MaximumBodySize": max_body,
    });
    if shape == CalendarItemShape::Explicit {
        item_shape["AdditionalProperties"] = Value::Array(MEETING_MESSAGE_FIELDS.iter().map(|f| property_uri(f)).collect());
    }
    json!({
        "__type": "GetItemRequest:#Exchange",
        "ItemShape": item_shape,
        "ItemIds": [ item_id_json(id) ],
        "ShapeName": null,
    })
}

/// Письмо о встрече из элемента ответа `GetItem`. Вид — по `ItemClass`; организатор — `Organizer`,
/// иначе `From` (кроме письма-ответа: там `From` — отвечающий участник, и организатор остаётся
/// пустым); свой ответ — `MyResponseType` или `ResponseType` (имя OWA JSON у приглашений);
/// у письма-ответа `ResponseType` — ответ отправителя, при его отсутствии он выводится из класса
/// (`…Resp.Pos`/`Neg`/`Tent`).
pub fn parse_meeting_request_info(v: &Value) -> MeetingRequestInfo {
    let class = str_of(v, "ItemClass").unwrap_or("");
    let kind = MeetingMessageKind::from_item_class(class);
    let response_type = str_of(v, "ResponseType").map(ResponseType::parse);
    let (my_response, response_type_of_sender) = if kind == MeetingMessageKind::Response {
        (ResponseType::Unknown, response_type.filter(|r| *r != ResponseType::Unknown).or_else(|| response_from_item_class(class)))
    } else {
        (str_of(v, "MyResponseType").map(ResponseType::parse).or(response_type).unwrap_or_default(), None)
    };
    MeetingRequestInfo {
        kind,
        start: parse_calendar_timestamp(str_of(v, "Start")),
        end: parse_calendar_timestamp(str_of(v, "End")),
        is_all_day: bool_of(v, "IsAllDayEvent"),
        location: location_of(v),
        organizer: v.get("Organizer").and_then(parse_participant).or_else(|| if kind == MeetingMessageKind::Response { None } else { v.get("From").and_then(parse_participant) }),
        my_response,
        is_out_of_date: bool_of(v, "IsOutOfDate"),
        associated_calendar_item: v.get("AssociatedCalendarItemId").and_then(parse_item_id),
        recurrence_text: v.get("Recurrence").and_then(recurrence_text),
        response_type_of_sender,
    }
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
// Справочник организации (см. docs/protocol-owa-json.md § 5l)

/// `FindPeople` по известной папке `directory` (глобальный список адресов): первая страница
/// `IndexedPageView` до `max` записей, форма `Default`, без разрешения разовых адресов и без
/// индекса подсказок (как автодополнение браузерной OWA при поиске по справочнику).
pub fn find_people_body(query: &str, max: usize) -> Value {
    json!({
        "__type": "FindPeopleRequest:#Exchange",
        "IndexedPageItemView": { "__type": "IndexedPageView:#Exchange", "BasePoint": "Beginning", "Offset": 0, "MaxEntriesReturned": max },
        "QueryString": query,
        "ParentFolderId": { "__type": "TargetFolderId:#Exchange", "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "directory" } },
        "PersonaShape": { "__type": "PersonaResponseShape:#Exchange", "BaseShape": "Default" },
        "ShouldResolveOneOffEmailAddress": false,
        "SearchPeopleSuggestionIndex": false
    })
}

/// Дополнительные свойства записи `FindPeople` для подсказок: в форме `Default` нет должности,
/// офиса и телефонов. Сервер (Exchange 2019) принимает их и отдаёт под ключами `Title` (строка),
/// `OfficeLocationsArray`, `BusinessPhoneNumbersArray`, `MobilePhonesArray` (атрибутированные
/// значения); `persona:Department` отвергает (`ErrorInvalidArgument`), поэтому его здесь нет (§ 5l).
pub const PERSONA_EXTRA_FIELDS: &[&str] = &["persona:Title", "persona:OfficeLocations", "persona:BusinessPhoneNumbers", "persona:MobilePhones"];

/// `find_people_body` с дополнительными свойствами записи: `AdditionalProperties` формы
/// `PersonaShape` (`PropertyUri` с именами `persona:*`, например `persona:Title`).
pub fn find_people_body_with(query: &str, max: usize, additional: &[&str]) -> Value {
    let mut v = find_people_body(query, max);
    if !additional.is_empty() {
        v["PersonaShape"]["AdditionalProperties"] = Value::Array(additional.iter().map(|f| property_uri(f)).collect());
    }
    v
}

/// `ResolveNames` по справочнику (`SearchScope: ActiveDirectory`). `full_contact` — с данными
/// контакта (`ReturnFullContactData: true` и `ContactDataShape: AllProperties`: должность,
/// отдел, телефоны), иначе только адрес (`Mailbox`).
pub fn resolve_names_body(query: &str, full_contact: bool) -> Value {
    resolve_names_body_shaped(query, full_contact.then_some("AllProperties"))
}

/// `ResolveNames` с выбранной формой данных контакта: `Some(форма)` — `ReturnFullContactData: true`
/// и `ContactDataShape` (`IdOnly`, `Default`, `AllProperties`), `None` — только адрес.
pub fn resolve_names_body_shaped(query: &str, contact_shape: Option<&str>) -> Value {
    let mut v = json!({
        "__type": "ResolveNamesRequest:#Exchange",
        "UnresolvedEntry": query,
        "ReturnFullContactData": contact_shape.is_some(),
        "SearchScope": "ActiveDirectory"
    });
    if let Some(shape) = contact_shape {
        v["ContactDataShape"] = json!(shape);
    }
    v
}

/// `ExpandDL` списка рассылки по SMTP-адресу.
pub fn expand_dl_body(email: &str) -> Value {
    json!({
        "__type": "ExpandDLRequest:#Exchange",
        "Mailbox": { "__type": "EmailAddress:#Exchange", "EmailAddress": email, "RoutingType": "SMTP" }
    })
}

/// Строка значения поля справочника в любой из форм OWA: строка; атрибутированное значение
/// `Persona` `{ "Value": … }`; массив таких значений (первое непустое); телефон `{ "Number": … }`;
/// адрес `{ "EmailAddress": … }`. Пустое или пробельное — `None`.
fn loose_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Array(a) => a.iter().find_map(loose_text),
        Value::Object(o) => ["Value", "Number", "EmailAddress", "DisplayName", "Name"].iter().find_map(|k| o.get(*k).and_then(loose_text)),
        _ => None,
    }
}

/// Первое непустое значение из ключей `keys` (в порядке перечисления).
fn first_text(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| v.get(*k).and_then(loose_text))
}

/// Адрес, похожий на SMTP: без префикса `SMTP:`, с `@` между непустыми частями, без пробелов и
/// без другого префикса вида (`SIP:`, `X500:` — адреса не почты).
fn smtp_like(s: &str) -> Option<String> {
    let t = s.trim();
    let t = t.strip_prefix("SMTP:").or_else(|| t.strip_prefix("smtp:")).unwrap_or(t);
    let (local, domain) = t.split_once('@')?;
    (!local.is_empty() && !domain.is_empty() && !t.contains(char::is_whitespace) && !local.contains(':')).then(|| t.to_string())
}

/// Первый SMTP-адрес среди строк значения (словарь `EmailAddresses` контакта, массив адресов
/// `Persona`): обход массивов и объектов на глубину до четырёх уровней.
fn first_smtp(v: Option<&Value>) -> Option<String> {
    fn walk(v: &Value, depth: usize) -> Option<String> {
        match v {
            Value::String(s) => smtp_like(s),
            Value::Array(a) if depth > 0 => a.iter().find_map(|x| walk(x, depth - 1)),
            Value::Object(o) if depth > 0 => {
                // Сначала основной член адреса, затем остальные (ключи словаря, имена).
                o.get("EmailAddress").or_else(|| o.get("Value")).and_then(|x| walk(x, depth - 1)).or_else(|| o.values().find_map(|x| walk(x, depth - 1)))
            }
            _ => None,
        }
    }
    walk(v?, 4)
}

/// Телефон одного из видов `kinds` (без учёта регистра) в любой из форм OWA: массив
/// `{ Key, PhoneNumber }` (словарь `PhoneNumbers` контакта `ResolveNames`, § 5l; пустые значения
/// пропускаются) или `{ Key, Value }`, объект
/// `{ "BusinessPhone": "…" }`, телефон `{ Number, Type }` (`Persona.PhoneNumber`) и массив
/// атрибутированных `{ Value: { Number, Type } }`. Пустой `kinds` — телефон любого вида.
fn phone_of(v: Option<&Value>, kinds: &[&str]) -> Option<String> {
    let matches = |k: Option<&str>| kinds.is_empty() || k.is_some_and(|k| kinds.iter().any(|w| w.eq_ignore_ascii_case(k)));
    match v? {
        Value::Array(a) => a.iter().find_map(|e| phone_of(Some(e), kinds)),
        Value::Object(o) => {
            if let Some(inner) = o.get("Value").filter(|x| x.is_object()) {
                return phone_of(Some(inner), kinds);
            }
            if o.contains_key("Key") || o.contains_key("Number") || o.contains_key("PhoneNumber") || o.contains_key("Type") {
                let kind = o.get("Key").or_else(|| o.get("Type")).and_then(Value::as_str);
                return if matches(kind) { o.get("Number").or_else(|| o.get("PhoneNumber")).or_else(|| o.get("Value")).and_then(loose_text) } else { None };
            }
            o.iter().find(|(k, _)| matches(Some(k.as_str()))).and_then(|(_, x)| loose_text(x))
        }
        Value::String(s) if kinds.is_empty() => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        _ => None,
    }
}

const BUSINESS_PHONE_KINDS: &[&str] = &["BusinessPhone", "Business", "BusinessPhone2", "Work"];
const MOBILE_PHONE_KINDS: &[&str] = &["MobilePhone", "Mobile", "Cellular"];

/// Запись `FindPeople` (`Persona`). Адрес — `EmailAddress.EmailAddress`, при адресе `EX` — первый
/// SMTP из `EmailAddresses`; вид — по `PersonaType` или `PersonaTypeString` (так в форме `Default`
/// Exchange 2019: `Person`, `DistributionList`, `Room`), иначе по `EmailAddress.MailboxType`, иначе
/// человек с адресом — ящик. Без имени и адреса — `None`.
pub fn parse_persona(v: &Value) -> Option<DirectoryPerson> {
    let addr = v.get("EmailAddress").filter(|a| a.is_object());
    let primary = addr.and_then(|a| str_of(a, "EmailAddress"));
    let email = primary.and_then(smtp_like).or_else(|| first_smtp(v.get("EmailAddresses"))).or_else(|| primary.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()));
    let display_name = first_text(v, &["DisplayName", "DisplayNameFirstLast", "FileAs"]).or_else(|| addr.and_then(|a| first_text(a, &["Name"])));
    if display_name.is_none() && email.is_none() {
        return None;
    }
    let persona_type = str_of(v, "PersonaType").or_else(|| str_of(v, "PersonaTypeString")).unwrap_or("");
    let kind = PersonKind::from_persona_type(persona_type)
        .or_else(|| addr.and_then(|a| str_of(a, "MailboxType")).and_then(PersonKind::from_mailbox_type))
        .unwrap_or(if email.is_some() && (persona_type.is_empty() || persona_type.eq_ignore_ascii_case("Person")) { PersonKind::Mailbox } else { PersonKind::Other });
    Some(DirectoryPerson {
        display_name,
        email,
        kind,
        title: first_text(v, &["Title", "JobTitle", "Titles", "TitlesArray"]),
        department: first_text(v, &["Department", "Departments", "DepartmentsArray"]),
        company: first_text(v, &["CompanyName", "CompanyNames", "CompanyNamesArray", "Company"]),
        office: first_text(v, &["OfficeLocation", "OfficeLocations", "OfficeLocationsArray", "Office"]),
        business_phone: first_text(v, &["BusinessPhoneNumber", "BusinessPhone"])
            .or_else(|| phone_of(v.get("BusinessPhoneNumbersArray").or_else(|| v.get("BusinessPhoneNumbers")), &[]))
            .or_else(|| phone_of(v.get("PhoneNumber"), BUSINESS_PHONE_KINDS))
            .or_else(|| phone_of(v.get("PhoneNumbers"), BUSINESS_PHONE_KINDS)),
        mobile_phone: first_text(v, &["MobilePhone"])
            .or_else(|| phone_of(v.get("MobilePhonesArray").or_else(|| v.get("MobilePhones")), &[]))
            .or_else(|| phone_of(v.get("PhoneNumber"), MOBILE_PHONE_KINDS))
            .or_else(|| phone_of(v.get("PhoneNumbers"), MOBILE_PHONE_KINDS)),
        alias: first_text(v, &["Alias"]),
        manager: None,
        direct_reports: None,
        address: None,
        notes: None,
    })
}

/// Массив записей `FindPeople`: `ResultSet` в `Body` (так отвечает Exchange 2019 по папке
/// `directory`: ответ без `ResponseMessages`, как у `FindConversation`, § 5l); терпимо к `People`,
/// `Personas` и к первому сообщению `ResponseMessages`.
pub fn find_people_array(v: &Value) -> Option<&Vec<Value>> {
    fn people(m: &Value) -> Option<&Vec<Value>> {
        ["ResultSet", "People", "Personas"].iter().find_map(|k| m.get(*k).and_then(Value::as_array))
    }
    people(v.get("Body").unwrap_or(v)).or_else(|| v.pointer("/Body/ResponseMessages/Items/0").and_then(people))
}

/// Записи ответа `FindPeople` в порядке сервера (без имени и адреса пропускаются).
pub fn parse_find_people(v: &Value) -> Vec<DirectoryPerson> {
    find_people_array(v).map(|a| a.iter().filter_map(parse_persona).collect()).unwrap_or_default()
}

/// Код и текст ошибки уровня `Body` (ответ без `ResponseMessages`, как у `FindPeople` и
/// `FindConversation`), если `Body.ResponseClass` не `Success`.
pub fn body_error(v: &Value) -> Option<(String, String)> {
    let body = v.get("Body")?;
    body.get("ResponseClass")?;
    message_error(body)
}

/// Запись `ResolveNames`: `Mailbox { Name, EmailAddress, RoutingType, MailboxType }` и при полных
/// данных `Contact { DisplayName, JobTitle, Department, CompanyName, OfficeLocation,
/// PhoneNumbers, EmailAddresses, Alias }`. Адрес `EX` заменяется первым SMTP из
/// `Contact.EmailAddresses`. Без имени и адреса — `None`.
pub fn parse_resolution(v: &Value) -> Option<DirectoryPerson> {
    let null = Value::Null;
    let mbx = v.get("Mailbox").filter(|m| m.is_object()).unwrap_or(&null);
    let contact = v.get("Contact").filter(|c| c.is_object()).unwrap_or(&null);
    let primary = str_of(mbx, "EmailAddress");
    let email = primary.and_then(smtp_like).or_else(|| first_smtp(contact.get("EmailAddresses"))).or_else(|| primary.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()));
    let display_name = first_text(contact, &["DisplayName"]).or_else(|| first_text(mbx, &["Name"]));
    if display_name.is_none() && email.is_none() {
        return None;
    }
    let kind = str_of(mbx, "MailboxType").and_then(PersonKind::from_mailbox_type).unwrap_or(PersonKind::Other);
    Some(DirectoryPerson {
        display_name,
        email,
        kind,
        title: first_text(contact, &["JobTitle", "Title"]),
        department: first_text(contact, &["Department"]),
        company: first_text(contact, &["CompanyName", "Company"]),
        office: first_text(contact, &["OfficeLocation", "Office"]),
        business_phone: phone_of(contact.get("PhoneNumbers"), BUSINESS_PHONE_KINDS).or_else(|| first_text(contact, &["BusinessPhone"])),
        mobile_phone: phone_of(contact.get("PhoneNumbers"), MOBILE_PHONE_KINDS).or_else(|| first_text(contact, &["MobilePhone"])),
        alias: first_text(contact, &["Alias"]).or_else(|| first_text(mbx, &["Alias"])),
        // Оргструктура (§ 5l'): только в полной форме; у записи без `Contact` — неизвестна.
        manager: contact.get("ManagerMailbox").and_then(parse_participant).or_else(|| first_text(contact, &["Manager"]).map(|name| Participant { name: Some(name), email: None, routing_type: None })),
        direct_reports: contact.get("DirectReports").map(|d| d.as_array().map(|a| a.iter().filter_map(parse_participant).collect()).unwrap_or_default()),
        address: business_address(contact.get("PhysicalAddresses")),
        notes: first_text(contact, &["Notes"]),
    })
}

/// Рабочий почтовый адрес из словаря `PhysicalAddresses` (запись с ключом `Business`, иначе первая
/// непустая) одной строкой: улица, индекс и город, область, страна.
fn business_address(v: Option<&Value>) -> Option<String> {
    let entries = dict_entries(v);
    let line = |e: &Value| -> Option<String> {
        let part = |keys: &[&str]| first_text(e, keys);
        let city = [part(&["PostalCode"]), part(&["City"])].into_iter().flatten().collect::<Vec<_>>().join(" ");
        let parts: Vec<String> = [part(&["Street"]), (!city.is_empty()).then_some(city), part(&["State"]), part(&["CountryOrRegion", "Country"])].into_iter().flatten().collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    };
    entries.iter().find(|(k, _)| k.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("Business"))).and_then(|(_, e)| line(e)).or_else(|| entries.iter().find_map(|(_, e)| line(e)))
}

/// Вид картинки по первым байтам: `jpeg`, `png`, `gif`, `bmp`, иначе `None`.
pub fn image_kind(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some("jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("png"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        [b'B', b'M', ..] => Some("bmp"),
        _ => None,
    }
}

/// Фото из разрешения `ResolveNames` с полными данными: `Contact.Photo` — base64 картинки
/// (терпимо к пробелам и `data:`-префиксу); нет ключа, пусто или не декодируется — `None`.
pub fn parse_resolution_photo(v: &Value) -> Option<Vec<u8>> {
    let raw = v.get("Contact")?.get("Photo")?.as_str()?.trim();
    let raw = raw.rsplit_once(',').map(|(_, b)| b).unwrap_or(raw);
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return None;
    }
    BASE64_STANDARD.decode(compact.as_bytes()).ok().filter(|b| !b.is_empty())
}

/// Запись справочника и её фото из одного разрешения `ResolveNames`.
pub type ResolvedWithPhoto = (DirectoryPerson, Option<Vec<u8>>);

/// Как `parse_resolve_names`, но каждая запись — вместе с фото (`parse_resolution_photo`).
pub fn parse_resolve_names_full(msg: &Value) -> std::result::Result<Vec<ResolvedWithPhoto>, (String, String)> {
    if let Some((code, message)) = message_error(msg) {
        match code.as_str() {
            "ErrorNameResolutionNoResults" => return Ok(Vec::new()),
            "ErrorNameResolutionMultipleResults" => {}
            _ if resolutions_array(msg).is_some_and(|a| !a.is_empty()) && str_of(msg, "ResponseClass") == Some("Warning") => {}
            _ => return Err((code, message)),
        }
    }
    Ok(resolutions_array(msg).map(|a| a.iter().filter_map(|r| parse_resolution(r).map(|p| (p, parse_resolution_photo(r)))).collect()).unwrap_or_default())
}

/// Массив разрешений из сообщения `ResolveNames`: `ResolutionSet.Resolutions` (терпимо к
/// `Resolution`, `Items` и к `ResolutionSet` массивом).
pub fn resolutions_array(msg: &Value) -> Option<&Vec<Value>> {
    let set = msg.get("ResolutionSet")?;
    set.as_array().or_else(|| ["Resolutions", "Resolution", "Items"].iter().find_map(|k| set.get(*k).and_then(Value::as_array)))
}

/// Итог `ResolveNames` по сообщению ответа: записи `ResolutionSet`. `ErrorNameResolutionNoResults`
/// — пусто; `ErrorNameResolutionMultipleResults` (класс `Warning`) — успех со списком; другой
/// отказ — `Err((код, текст))`.
pub fn parse_resolve_names(msg: &Value) -> std::result::Result<Vec<DirectoryPerson>, (String, String)> {
    if let Some((code, message)) = message_error(msg) {
        match code.as_str() {
            "ErrorNameResolutionNoResults" => return Ok(Vec::new()),
            "ErrorNameResolutionMultipleResults" => {}
            _ if resolutions_array(msg).is_some_and(|a| !a.is_empty()) && str_of(msg, "ResponseClass") == Some("Warning") => {}
            _ => return Err((code, message)),
        }
    }
    Ok(resolutions_array(msg).map(|a| a.iter().filter_map(parse_resolution).collect()).unwrap_or_default())
}

/// Массив участников из сообщения `ExpandDL`: `DLExpansion.Mailbox` (терпимо к `Mailboxes`,
/// `Items` и к `DLExpansion` массивом).
pub fn dl_expansion_array(msg: &Value) -> Option<&Vec<Value>> {
    let set = msg.get("DLExpansion")?;
    set.as_array().or_else(|| ["Mailbox", "Mailboxes", "Items"].iter().find_map(|k| set.get(*k).and_then(Value::as_array)))
}

/// Участник списка из `ExpandDL`: адрес той же формы, что получатель письма
/// (`{ Name, EmailAddress, RoutingType, MailboxType }`), вид — по `MailboxType`.
pub fn parse_dl_member(v: &Value) -> Option<DirectoryPerson> {
    let m = v.get("Mailbox").filter(|m| m.is_object()).unwrap_or(v);
    let primary = str_of(m, "EmailAddress");
    let email = primary.and_then(smtp_like).or_else(|| primary.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()));
    let display_name = first_text(m, &["Name", "DisplayName"]);
    if display_name.is_none() && email.is_none() {
        return None;
    }
    let kind = str_of(m, "MailboxType").and_then(PersonKind::from_mailbox_type).unwrap_or(PersonKind::Other);
    Some(DirectoryPerson { display_name, email, kind, ..Default::default() })
}

/// Участники списка из сообщения ответа `ExpandDL` в порядке сервера.
pub fn parse_expand_dl(msg: &Value) -> Vec<DirectoryPerson> {
    dl_expansion_array(msg).map(|a| a.iter().filter_map(parse_dl_member).collect()).unwrap_or_default()
}

/// Объединение ключей объектов массива с числом объектов, где ключ есть, по алфавиту: форма
/// записей ответа без значений (для журнала и документации протокола).
pub fn key_union(items: &[Value]) -> Vec<(String, usize)> {
    let mut map: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for item in items {
        if let Some(o) = item.as_object() {
            for k in o.keys() {
                *map.entry(k.clone()).or_default() += 1;
            }
        }
    }
    map.into_iter().collect()
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
        // Нет ключа `IsRead` — элемент не письмо (контакт, встреча): он не «непрочитанный».
        is_read: v.get("IsRead").map(|b| b.as_bool().unwrap_or(false)).unwrap_or(true),
        is_draft: bool_of(v, "IsDraft"),
        has_attachments: bool_of(v, "HasAttachments"),
        importance: str_of(v, "Importance").map(Importance::parse).unwrap_or_default(),
        flag: v.get("Flag").and_then(|f| str_of(f, "FlagStatus")).map(FlagStatus::parse).unwrap_or_default(),
        size: i64_of(v, "Size"),
        preview: str_of(v, "Preview").map(String::from),
        last_verb: extended_value(v, "0x1081").and_then(verb_code).and_then(LastVerb::from_code).or_else(|| str_of(v, "IconIndex").and_then(LastVerb::from_icon_index)),
        last_verb_at: extended_value(v, "0x1082").and_then(Value::as_str).and_then(|s| parse_timestamp(Some(s))),
    })
}

/// Значение расширенного свойства по тегу (`ExtendedProperty[].ExtendedFieldURI.PropertyTag`,
/// без учёта регистра; десятичная форма тега тоже принимается).
fn extended_value<'a>(v: &'a Value, tag: &str) -> Option<&'a Value> {
    let wanted = u32::from_str_radix(tag.trim_start_matches("0x"), 16).ok()?;
    v.get("ExtendedProperty")?.as_array()?.iter().find_map(|p| {
        let uri = p.get("ExtendedFieldURI")?;
        let raw = uri.get("PropertyTag")?;
        let found = match raw {
            Value::String(s) => {
                let s = s.trim();
                if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) { u32::from_str_radix(hex, 16).ok() } else { s.parse::<u32>().ok() }
            }
            Value::Number(n) => n.as_u64().map(|n| n as u32),
            _ => None,
        }?;
        (found == wanted).then(|| p.get("Value")).flatten()
    })
}

/// Код из значения расширенного свойства: число или строка с числом.
fn verb_code(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
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
        // `message:ReplyTo` — массив адресов той же формы, что получатели; нет ключа — пусто.
        reply_to: parse_participants(v.get("ReplyTo")),
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

/// Записи словаря EWS (`EmailAddresses`, `PhoneNumbers`, `PhysicalAddresses`, `ImAddresses`):
/// массив записей с `Key` (форма OWA JSON) или объект «ключ → запись». Каждая запись — объект
/// (строка оборачивается в `{"Value": …}`), ключ — `Key` записи или имя поля объекта.
fn dict_entries(v: Option<&Value>) -> Vec<(Option<String>, Value)> {
    fn wrap(e: &Value) -> Value {
        match e {
            Value::String(s) => json!({ "Value": s }),
            other => other.clone(),
        }
    }
    match v {
        Some(Value::Array(a)) => a.iter().map(|e| (str_of(e, "Key").map(String::from), wrap(e))).collect(),
        Some(single @ Value::Object(o)) if o.contains_key("Key") => vec![(str_of(single, "Key").map(String::from), single.clone())],
        Some(Value::Object(o)) => o.iter().map(|(k, e)| (Some(k.clone()), wrap(e))).collect(),
        _ => Vec::new(),
    }
}

/// Контакт или группа из элемента с формой `AllProperties` (§ 5q): `SyncFolderItems` или `GetItem`
/// по папке класса `IPF.Contact`. Терпим к форме: словари адресов, телефонов и почтовых адресов —
/// массивы записей `{Key, …}` или объекты по ключам, участники группы — `Members[].Member.Mailbox`
/// или `Members[].Mailbox`, тело — объект `{BodyType, Value}` или строка. Нет `ItemId` — `None`.
pub fn parse_contact_item(v: &Value) -> Option<ContactItem> {
    let item_id = v.get("ItemId").and_then(parse_item_id)?;
    let mut c = ContactItem::new(item_id);
    c.item_class = str_of(v, "ItemClass").map(String::from);
    c.display_name = first_text(v, &["DisplayName"]);
    c.file_as = first_text(v, &["FileAs"]);
    c.given_name = first_text(v, &["GivenName"]);
    c.middle_name = first_text(v, &["MiddleName"]);
    c.surname = first_text(v, &["Surname"]);
    c.nickname = first_text(v, &["Nickname"]);
    c.company = first_text(v, &["CompanyName"]);
    c.job_title = first_text(v, &["JobTitle"]);
    c.department = first_text(v, &["Department"]);
    c.office = first_text(v, &["OfficeLocation"]);
    c.emails = dict_entries(v.get("EmailAddresses"))
        .into_iter()
        .filter_map(|(key, e)| {
            let raw = e.get("EmailAddress").or_else(|| e.get("Value")).and_then(loose_text)?;
            let email = smtp_like(&raw).unwrap_or(raw);
            Some(ContactEmail { key, email, name: e.get("Name").and_then(loose_text) })
        })
        .collect();
    c.phones = dict_entries(v.get("PhoneNumbers"))
        .into_iter()
        .filter_map(|(key, e)| {
            let number = e.get("PhoneNumber").or_else(|| e.get("Number")).or_else(|| e.get("Value")).and_then(loose_text)?;
            Some(ContactPhone { key, number })
        })
        .collect();
    c.addresses = dict_entries(v.get("PhysicalAddresses"))
        .into_iter()
        .map(|(key, e)| ContactAddress {
            key,
            street: first_text(&e, &["Street"]),
            city: first_text(&e, &["City"]),
            state: first_text(&e, &["State"]),
            country: first_text(&e, &["CountryOrRegion", "Country"]),
            postal_code: first_text(&e, &["PostalCode"]),
        })
        .filter(|a| a.street.is_some() || a.city.is_some() || a.state.is_some() || a.country.is_some() || a.postal_code.is_some())
        .collect();
    c.im_addresses = dict_entries(v.get("ImAddresses")).into_iter().filter_map(|(_, e)| e.get("ImAddress").or_else(|| e.get("Value")).and_then(loose_text)).collect();
    (c.notes, c.notes_is_html) = match v.get("Body") {
        Some(b @ Value::Object(_)) => (b.get("Value").and_then(loose_text), str_of(b, "BodyType").is_some_and(|t| t.eq_ignore_ascii_case("HTML"))),
        Some(b @ Value::String(_)) => (loose_text(b), false),
        _ => (None, false),
    };
    c.birthday = parse_timestamp(str_of(v, "Birthday"));
    c.has_photo = bool_of(v, "HasPicture");
    c.members = v.get("Members").and_then(Value::as_array).map(|a| a.iter().filter_map(|e| parse_participant(e.get("Member").unwrap_or(e))).collect()).unwrap_or_default();
    c.modified_at = parse_timestamp(str_of(v, "LastModifiedTime"));
    Some(c)
}

/// Изменения папки контактов из ответа `SyncFolderItems` (§ 5q): как `parse_item_changes`, но
/// элементы — контакты и группы (`parse_contact_item`); `ReadFlagChange` у контактов не бывает и
/// пропускается.
pub fn parse_contact_changes(msg: &Value, requested: u32) -> (SyncState, Vec<ContactChange>, bool) {
    let state = SyncState(str_of(msg, "SyncState").unwrap_or("").to_string());
    let raw = changes_array(msg);
    let mut out = Vec::with_capacity(raw.len());
    for ch in &raw {
        let kind = str_of(ch, "ChangeType").unwrap_or("");
        match kind {
            "Create" | "Update" => {
                if let Some(c) = ch.get("Item").and_then(parse_contact_item) {
                    out.push(if kind == "Create" { ContactChange::Create(c) } else { ContactChange::Update(c) });
                }
            }
            "Delete" => {
                if let Some(id) = ch.get("ItemId").and_then(parse_item_id).or_else(|| ch.get("Item").and_then(|i| i.get("ItemId")).and_then(parse_item_id)) {
                    out.push(ContactChange::Delete(id));
                }
            }
            _ => {}
        }
    }
    let includes_last = msg.get("IncludesLastItemInRange").and_then(Value::as_bool).unwrap_or(raw.len() < requested as usize);
    (state, out, includes_last)
}

/// Имена ключей формы `AllProperties` контактов в ответе `SyncFolderItems` (`contact_items_keys` по
/// элементам изменений `Create`/`Update`).
pub fn contact_form_keys(msg: &Value) -> String {
    let raw = changes_array(msg);
    contact_items_keys(raw.iter().filter_map(|ch| ch.get("Item")))
}

/// Имена ключей элементов-контактов (`SyncFolderItems`, `GetItem`) — для журнала и документа
/// (§ 5q): объединение ключей элементов и записей словарей адресов, телефонов, почтовых адресов,
/// участников и тела. Только имена ключей, без значений.
pub fn contact_items_keys<'a>(items: impl IntoIterator<Item = &'a Value>) -> String {
    use std::collections::{BTreeMap, BTreeSet};
    let mut item_keys = BTreeSet::new();
    let mut nested: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for item in items {
        let Some(item) = item.as_object() else { continue };
        item_keys.extend(item.keys().cloned());
        for dict in ["EmailAddresses", "PhoneNumbers", "PhysicalAddresses", "ImAddresses", "Members", "Body"] {
            let entries: Vec<&Value> = match item.get(dict) {
                Some(Value::Array(a)) => a.iter().collect(),
                Some(o @ Value::Object(_)) => vec![o],
                _ => continue,
            };
            let set = nested.entry(dict).or_default();
            for e in entries {
                if let Some(o) = e.as_object() {
                    set.extend(o.keys().cloned());
                    if let Some(inner) = o.get("Member").or_else(|| o.get("Mailbox")).and_then(Value::as_object) {
                        set.extend(inner.keys().map(|k| format!("Mailbox.{k}")));
                    }
                }
            }
        }
    }
    let mut s = format!("Item{{{}}}", item_keys.into_iter().collect::<Vec<_>>().join(","));
    for (k, set) in nested {
        s.push_str(&format!(" {k}[{{{}}}]", set.into_iter().collect::<Vec<_>>().join(",")));
    }
    s
}

/// Выдача `FindItem` писем папки (§ 5m).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoundItems {
    /// Письма в порядке ответа сервера (для `find_recent_items_body` — свежие первыми).
    pub items: Vec<MessageMeta>,
    /// Элементов в ответе, включая не разобранные (без `ItemId`).
    pub raw_count: usize,
    /// `RootFolder.IncludesLastItemInRange`; нет ключа — по длине выдачи.
    pub includes_last: bool,
    /// `RootFolder.TotalItemsInView`: сколько всего писем в папке.
    pub total_in_view: Option<i64>,
}

/// Письма из ответа `FindItem` (`RootFolder.Items[]`) в той же форме, что у `SyncFolderItems`.
pub fn parse_find_items(msg: &Value, requested: u32) -> FoundItems {
    let root = msg.get("RootFolder").unwrap_or(msg);
    let raw: &[Value] = root.get("Items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let items: Vec<MessageMeta> = raw.iter().filter_map(parse_message_meta).collect();
    let includes_last = root.get("IncludesLastItemInRange").and_then(Value::as_bool).unwrap_or(raw.len() < requested as usize);
    FoundItems { items, raw_count: raw.len(), includes_last, total_in_view: i64_of(root, "TotalItemsInView") }
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

/// Часовой пояс пользователя из словаря настроек OWA: ключ `timezone` (сравнение без учёта
/// регистра), значение — идентификатор пояса Windows (`Russian Standard Time`). Пустой или
/// отсутствующий — `None`.
pub fn parse_user_time_zone(msg: &Value) -> Option<String> {
    user_configuration_dictionary(msg).into_iter().find(|(key, _)| key.eq_ignore_ascii_case("timezone")).map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Поля события из ответа `GetItem` с `AllProperties`, по которым проверяется запись: время и
/// пояса (`Start`, `End`, `StartTimeZoneId`, `EndTimeZoneId`, `TimeZone`, `StartWallClock`,
/// `EndWallClock`, `IsAllDayEvent`) и место (`Location` как есть — строка или объект
/// `EnhancedLocation` с `DisplayName`; `ExtendedProperty` — запрошенное `PidLidLocation`).
/// Отсутствующие ключи опускаются.
pub fn calendar_probe_fields(item: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in ["IsAllDayEvent", "Start", "End", "StartTimeZoneId", "EndTimeZoneId", "TimeZone", "StartWallClock", "EndWallClock", "Location", "EnhancedLocation", "ExtendedProperty"] {
        if let Some(v) = item.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// Код и текст ошибки сообщения ответа, если `ResponseClass != Success`.
pub fn message_error(msg: &Value) -> Option<(String, String)> {
    let class = str_of(msg, "ResponseClass").unwrap_or("Success");
    if class == "Success" {
        return None;
    }
    Some((str_of(msg, "ResponseCode").unwrap_or("Unknown").to_string(), str_of(msg, "MessageText").unwrap_or("").to_string()))
}

/// Код ответа — отказ по свойству или форме запроса (`ErrorInvalidPropertyRequest`,
/// `ErrorInvalidPropertySet`, `ErrorInvalidShape`…), а не по элементу или сессии.
pub fn is_property_code(code: &str) -> bool {
    code.contains("Property") || code.contains("Shape")
}

/// Все сообщения ответа — отказы по свойству или форме (`is_property_code`) при HTTP 200: так
/// сервер отвергает форму запроса целиком, по каждому элементу. Пустой ответ, хотя бы одно
/// успешное сообщение или отказ с другим кодом (`ErrorItemNotFound`) — `false`: это отказы по
/// элементам, а не по форме.
pub fn all_property_rejections(v: &Value) -> bool {
    let msgs = response_messages(v);
    !msgs.is_empty() && msgs.iter().all(|m| message_error(m).is_some_and(|(code, _)| is_property_code(&code)))
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
    fn contacts_sync_body_and_changes() {
        let folder = FolderRef::Id(FolderId { id: "F-C".into(), change_key: None });
        let body = sync_contact_items_body(&folder, Some(&SyncState("st".into())), 100);
        assert_eq!(body["__type"], "SyncFolderItemsRequest:#Exchange");
        assert_eq!(body["ItemShape"], json!({ "__type": "ItemResponseShape:#Exchange", "BaseShape": "AllProperties", "BodyType": "Text" }), "полная форма без AdditionalProperties");
        assert_eq!(body["SyncFolderId"], target_folder_json(&folder));
        assert_eq!((body["MaxChangesReturned"].as_u64(), body["SyncScope"].as_str(), body["SyncState"].as_str()), (Some(100), Some("NormalItems"), Some("st")));
        assert!(sync_contact_items_body(&folder, None, 10).get("SyncState").is_none());

        let msg: Value = serde_json::from_str(r#"{"SyncState":"s2","IncludesLastItemInRange":true,"Changes":{"Changes":[
          {"__type":"SyncFolderItemsCreateType:#Exchange","ChangeType":"Create","Item":{"__type":"Contact:#Exchange","ItemId":{"Id":"C1","ChangeKey":"ck1"},"ItemClass":"IPM.Contact","DisplayName":"Иван Петров","FileAs":"Петров, Иван","GivenName":"Иван","Surname":"Петров","CompanyName":"ООО Ромашка","JobTitle":"Инженер","EmailAddresses":[{"Key":"EmailAddress1","Name":"Иван Петров","RoutingType":"SMTP","MailboxType":"Contact","EmailAddress":"ivan@example.com"}],"PhoneNumbers":[{"Key":"MobilePhone","PhoneNumber":"+7 916 1234567"},{"Key":"BusinessPhone","PhoneNumber":""}],"PhysicalAddresses":[{"Key":"Business","Street":"Ленина, 1","City":"Москва","CountryOrRegion":"Россия","PostalCode":"101000"}],"ImAddresses":[{"Key":"ImAddress1","ImAddress":"sip:ivan@example.com"}],"Body":{"BodyType":"HTML","Value":"<p>Заметка</p>"},"Birthday":"1980-05-04T00:00:00Z","HasPicture":true,"LastModifiedTime":"2026-10-30T10:00:00Z"}},
          {"__type":"SyncFolderItemsCreateOrUpdateType:#Exchange","ChangeType":"Update","Item":{"__type":"DistributionList:#Exchange","ItemId":{"Id":"G1","ChangeKey":"gk"},"ItemClass":"IPM.DistList","DisplayName":"Семья","Members":[{"__type":"Member:#Exchange","Key":"m1","Mailbox":{"Name":"Анна","EmailAddress":"anna@example.com","RoutingType":"SMTP"},"Status":"Normal"}]}},
          {"__type":"SyncFolderItemsDeleteType:#Exchange","ChangeType":"Delete","ItemId":{"Id":"C0"}},
          {"__type":"SyncFolderItemsReadFlagType:#Exchange","ChangeType":"ReadFlagChange","ItemId":{"Id":"C1"},"IsRead":true}
        ]}}"#).unwrap();
        let (state, changes, last) = parse_contact_changes(&msg, 100);
        assert_eq!(state, SyncState("s2".into()));
        assert!(last);
        assert_eq!(changes.len(), 3, "ReadFlagChange у контактов пропускается");
        let ContactChange::Create(c) = &changes[0] else { panic!("{:?}", changes[0]) };
        assert_eq!(c.item_id.change_key.as_deref(), Some("ck1"));
        assert_eq!(c.item_class.as_deref(), Some("IPM.Contact"));
        assert_eq!(c.file_as.as_deref(), Some("Петров, Иван"));
        assert_eq!(c.list_name().as_deref(), Some("Петров, Иван"));
        assert_eq!(c.primary_email().as_deref(), Some("ivan@example.com"));
        assert_eq!((c.emails[0].key.as_deref(), c.emails[0].name.as_deref()), (Some("EmailAddress1"), Some("Иван Петров")));
        assert_eq!(c.phones.len(), 1, "пустой номер отброшен");
        assert_eq!((c.phones[0].key.as_deref(), c.phones[0].number.as_str()), (Some("MobilePhone"), "+7 916 1234567"));
        assert_eq!(c.addresses.len(), 1);
        assert_eq!((c.addresses[0].key.as_deref(), c.addresses[0].city.as_deref(), c.addresses[0].country.as_deref()), (Some("Business"), Some("Москва"), Some("Россия")));
        assert_eq!(c.im_addresses, vec!["sip:ivan@example.com".to_string()]);
        assert_eq!((c.notes.as_deref(), c.notes_is_html), (Some("<p>Заметка</p>"), true));
        assert!(c.has_photo && c.birthday.is_some() && c.modified_at.is_some());
        assert!(!c.is_group());
        let ContactChange::Update(g) = &changes[1] else { panic!("{:?}", changes[1]) };
        assert!(g.is_group());
        assert_eq!(g.members.len(), 1);
        assert_eq!((g.members[0].name.as_deref(), g.members[0].email.as_deref()), (Some("Анна"), Some("anna@example.com")));
        assert_eq!(changes[2], ContactChange::Delete(ItemId { id: "C0".into(), change_key: None }));

        let keys = contact_form_keys(&msg);
        assert!(keys.starts_with("Item{Birthday,Body,CompanyName,"), "{keys}");
        assert!(keys.contains(" EmailAddresses[{EmailAddress,Key,MailboxType,Name,RoutingType}]"), "{keys}");
        assert!(keys.contains(" Members[{Key,Mailbox,Mailbox.EmailAddress,Mailbox.Name,Mailbox.RoutingType,Status,__type}]"), "{keys}");
        assert!(!keys.contains("ivan@") && !keys.contains("Петров"), "в форме только имена ключей");
        let item = msg.pointer("/Changes/Changes/0/Item").unwrap();
        let one = contact_items_keys([item]);
        assert!(one.starts_with("Item{Birthday,Body,CompanyName,") && !one.contains("Members"), "ключи одного элемента, без участников группы: {one}");

        let body = get_contact_items_body(&[ItemId { id: "C1".into(), change_key: Some("ck".into()) }, ItemId { id: "G1".into(), change_key: None }]);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"AllProperties","BodyType":"Text"},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"C1"},{"__type":"ItemId:#Exchange","Id":"G1"}],"ShapeName":null}"#
        );

        // Словари объектами «ключ → значение» и тело строкой — тоже разбираются.
        let alt = json!({ "ItemId": { "Id": "C2" }, "GivenName": "Зоя", "EmailAddresses": { "EmailAddress1": "smtp:zoya@example.com" }, "PhoneNumbers": { "HomePhone": "123" }, "Body": "просто текст" });
        let c = parse_contact_item(&alt).unwrap();
        assert_eq!(c.list_name().as_deref(), Some("Зоя"));
        assert_eq!((c.emails[0].key.as_deref(), c.emails[0].email.as_str()), (Some("EmailAddress1"), "zoya@example.com"));
        assert_eq!((c.phones[0].key.as_deref(), c.phones[0].number.as_str()), (Some("HomePhone"), "123"));
        assert_eq!((c.notes.as_deref(), c.notes_is_html), (Some("просто текст"), false));
        assert!(parse_contact_item(&json!({ "DisplayName": "без идентификатора" })).is_none());
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
        assert!(m.reply_to.is_empty(), "без ключа ReplyTo список пуст");
        assert_eq!(m.attachments[0].name.as_deref(), Some("a.pdf"));
        assert_eq!(m.body.unwrap().unique_body.as_deref(), Some("<p>x</p>"));
        // ReplyTo — массив адресов той же формы, что получатели (с MailboxType, как отдаёт OWA JSON).
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"A"},"From":{"Mailbox":{"Name":"Список","EmailAddress":"list@x.ru","RoutingType":"SMTP"}},
          "ReplyTo":[{"Name":"Владелец","EmailAddress":"owner@x.ru","RoutingType":"SMTP","MailboxType":"OneOff"}],
          "ToRecipients":[]}"#).unwrap();
        let m = parse_message_full(&v).unwrap();
        assert_eq!(m.reply_to, vec![Participant { name: Some("Владелец".into()), email: Some("owner@x.ru".into()), routing_type: Some("SMTP".into()) }]);
        assert_eq!(m.effective_reply_to().len(), 1);
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"A"},"ReplyTo":[]}"#).unwrap();
        assert!(parse_message_full(&v).unwrap().reply_to.is_empty(), "пустой массив — пусто");
    }

    #[test]
    fn property_rejections_cover_whole_batch_only() {
        let resp = |items: &str| -> Value { serde_json::from_str(&format!(r#"{{"Body":{{"ResponseMessages":{{"Items":[{items}]}}}}}}"#)).unwrap() };
        let prop = r#"{"__type":"GetItemResponseMessage:#Exchange","ResponseClass":"Error","ResponseCode":"ErrorInvalidPropertyRequest","MessageText":"The property message:ReplyTo…","Items":[]}"#;
        let shape = r#"{"ResponseClass":"Error","ResponseCode":"ErrorInvalidShape","MessageText":""}"#;
        let not_found = r#"{"ResponseClass":"Error","ResponseCode":"ErrorItemNotFound","MessageText":"…"}"#;
        let ok = r#"{"ResponseClass":"Success","ResponseCode":"NoError","Items":[{"ItemId":{"Id":"A"}}]}"#;
        assert!(all_property_rejections(&resp(&format!("{prop},{prop}"))), "отказ по свойству в каждом сообщении — форма отвергнута");
        assert!(all_property_rejections(&resp(&format!("{prop},{shape}"))));
        assert!(!all_property_rejections(&resp(&format!("{prop},{ok}"))), "хоть одно успешное — форма принята");
        assert!(!all_property_rejections(&resp(&format!("{prop},{not_found}"))), "смешанная пачка — отказы по элементам");
        assert!(!all_property_rejections(&resp(&format!("{not_found},{not_found}"))));
        assert!(!all_property_rejections(&resp("")), "пустой ответ — не отказ по форме");
        assert!(!all_property_rejections(&json!({"Body": {}})));
        assert!(is_property_code("ErrorInvalidPropertySet"));
        assert!(!is_property_code("ErrorServerBusy"));
    }

    #[test]
    fn get_item_body_exact_with_and_without_reply_to() {
        let s = serde_json::to_string(&get_item_body(&ids()[..1], "ItemPart", 2_097_152, true)).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"IdOnly","FilterHtmlContent":true,"BlockExternalImagesIfSenderUntrusted":true,"MaximumBodySize":2097152,"MaximumRecipientsToReturn":200,"AdditionalProperties":[{"__type":"PropertyUri:#Exchange","FieldURI":"message:ReplyTo"}]},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"A","ChangeKey":"ck1"}],"ShapeName":"ItemPart"}"#
        );
        let s = serde_json::to_string(&get_item_body(&ids(), "MailListItem", 4096, false)).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"IdOnly","FilterHtmlContent":true,"BlockExternalImagesIfSenderUntrusted":true,"MaximumBodySize":4096,"MaximumRecipientsToReturn":200,"AdditionalProperties":[{"__type":"ExtendedPropertyUri:#Exchange","PropertyTag":"0x1081","PropertyType":"Integer"},{"__type":"ExtendedPropertyUri:#Exchange","PropertyTag":"0x1082","PropertyType":"SystemTime"}]},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"A","ChangeKey":"ck1"},{"__type":"ItemId:#Exchange","Id":"B"}],"ShapeName":"MailListItem"}"#,
            "форма списка: без Reply-To, с расширенными свойствами последнего действия"
        );
        let sync = serde_json::to_string(&sync_folder_items_body(&FolderRef::Distinguished(DistinguishedFolder::Inbox), None, 10, true)).unwrap();
        assert!(sync.contains(r#"{"__type":"PropertyUri:#Exchange","FieldURI":"item:IconIndex"}"#), "{sync}");
        assert!(sync.contains(r#"{"__type":"ExtendedPropertyUri:#Exchange","PropertyTag":"0x1081","PropertyType":"Integer"},{"__type":"ExtendedPropertyUri:#Exchange","PropertyTag":"0x1082","PropertyType":"SystemTime"}]},"SyncFolderId""#), "{sync}");
    }

    #[test]
    fn resolution_photo_is_decoded_from_base64() {
        let jpeg = [0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F'];
        let b64 = BASE64_STANDARD.encode(jpeg);
        let v: Value = serde_json::from_str(&format!(r#"{{"Mailbox":{{"Name":"Имя","EmailAddress":"a@x.ru","MailboxType":"Mailbox"}},"Contact":{{"DisplayName":"Имя","HasPicture":true,"Photo":" {b64} "}}}}"#)).unwrap();
        assert_eq!(parse_resolution_photo(&v).as_deref(), Some(&jpeg[..]));
        let v: Value = serde_json::from_str(r#"{"Mailbox":{"Name":"Имя","EmailAddress":"a@x.ru"},"Contact":{"DisplayName":"Имя","HasPicture":false}}"#).unwrap();
        assert_eq!(parse_resolution_photo(&v), None);
        let v: Value = serde_json::from_str(r#"{"Mailbox":{"Name":"Имя","EmailAddress":"a@x.ru"},"Contact":{"Photo":"не base64 !!!"}}"#).unwrap();
        assert_eq!(parse_resolution_photo(&v), None);
        let msg: Value = serde_json::from_str(&format!(r#"{{"ResponseClass":"Success","ResponseCode":"NoError","ResolutionSet":{{"Resolutions":[{{"Mailbox":{{"Name":"Имя","EmailAddress":"a@x.ru","MailboxType":"Mailbox"}},"Contact":{{"Photo":"{b64}"}}}},{{"Mailbox":{{"Name":"Без фото","EmailAddress":"b@x.ru","MailboxType":"Mailbox"}}}}]}}}}"#)).unwrap();
        let full = parse_resolve_names_full(&msg).unwrap();
        assert_eq!(full.len(), 2);
        assert!(full[0].1.is_some() && full[1].1.is_none());
    }

    #[test]
    fn item_without_is_read_is_not_unread() {
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"C","ChangeKey":"k"},"ItemClass":"IPM.Contact","Subject":"Имя"}"#).unwrap();
        assert!(parse_message_meta(&v).unwrap().is_read, "контакт без IsRead — прочитан");
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"M","ChangeKey":"k"},"ItemClass":"IPM.Note","IsRead":false}"#).unwrap();
        assert!(!parse_message_meta(&v).unwrap().is_read);
    }

    #[test]
    fn last_verb_parsed_from_extended_props_or_icon_index() {
        let v: Value = serde_json::from_str(
            r#"{"ItemId":{"Id":"A","ChangeKey":"k"},"Subject":"S","IconIndex":"MailReplied",
                "ExtendedProperty":[{"ExtendedFieldURI":{"PropertyTag":"0x1081","PropertyType":"Integer"},"Value":"104"},
                                    {"ExtendedFieldURI":{"PropertyTag":"0x1082","PropertyType":"SystemTime"},"Value":"2026-10-26T05:02:11Z"}]}"#,
        )
        .unwrap();
        let m = parse_message_meta(&v).unwrap();
        assert_eq!(m.last_verb, Some(LastVerb::Forwarded), "расширенное свойство важнее значка");
        assert_eq!(m.last_verb_at.map(|t| t.to_string()), Some("2026-10-26T05:02:11Z".into()));
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"A","ChangeKey":"k"},"IconIndex":"MailForwarded"}"#).unwrap();
        let m = parse_message_meta(&v).unwrap();
        assert_eq!((m.last_verb, m.last_verb_at), (Some(LastVerb::Forwarded), None));
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"A","ChangeKey":"k"},"IconIndex":"MailRead","ExtendedProperty":[{"ExtendedFieldURI":{"PropertyTag":"4225","PropertyType":"Integer"},"Value":103}]}"#).unwrap();
        assert_eq!(parse_message_meta(&v).unwrap().last_verb, Some(LastVerb::RepliedAll), "десятичный тег и число в Value");
        let v: Value = serde_json::from_str(r#"{"ItemId":{"Id":"A","ChangeKey":"k"},"IconIndex":"MailRead"}"#).unwrap();
        assert_eq!(parse_message_meta(&v).unwrap().last_verb, None);
    }

    #[test]
    fn get_item_ids_body_exact_without_change_keys() {
        let s = serde_json::to_string(&get_item_ids_body(&ids())).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"IdOnly"},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"A"},{"__type":"ItemId:#Exchange","Id":"B"}],"ShapeName":null}"#
        );
        // Ответ разбирается общим разбором операций по элементам: полный ключ приходит в Items[0].ItemId.
        let v: Value = serde_json::from_str(
            r#"{"Body":{"ResponseMessages":{"Items":[
              {"__type":"GetItemResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError","Items":[{"__type":"Message:#Exchange","ItemId":{"__type":"ItemId:#Exchange","Id":"A","ChangeKey":"CQAAABYAAAD7bL0OVIq7Rr0Dd0Rdq5a5AAAbuyX5"}}]},
              {"__type":"GetItemResponseMessage:#Exchange","ResponseClass":"Error","ResponseCode":"ErrorItemNotFound","MessageText":"The specified object was not found in the store.","Items":[]}
            ]}}}"#,
        )
        .unwrap();
        let out = parse_write_results(&response_messages(&v), &ids());
        assert_eq!(out[0], WriteResult::Ok { id: ItemId { id: "A".into(), change_key: Some("CQAAABYAAAD7bL0OVIq7Rr0Dd0Rdq5a5AAAbuyX5".into()) } });
        assert!(matches!(&out[1], WriteResult::Failed { code, .. } if code == "ErrorItemNotFound"));
        assert!(out[0].id().has_full_change_key() && !ids()[0].has_full_change_key());
    }

    fn folder_id() -> FolderId {
        FolderId { id: "F1".into(), change_key: Some("fck".into()) }
    }

    #[test]
    fn create_folder_body_exact() {
        let s = serde_json::to_string(&create_folder_body(&FolderRef::Distinguished(DistinguishedFolder::MsgFolderRoot), "AIOWA тест")).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"CreateFolderRequest:#Exchange","ParentFolderId":{"__type":"TargetFolderId:#Exchange","BaseFolderId":{"__type":"DistinguishedFolderId:#Exchange","Id":"msgfolderroot"}},"Folders":[{"__type":"Folder:#Exchange","DisplayName":"AIOWA тест","FolderClass":"IPF.Note"}]}"#
        );
        let s = serde_json::to_string(&create_folder_body(&FolderRef::Id(folder_id()), "Подпапка")).unwrap();
        assert!(s.contains(r#""ParentFolderId":{"__type":"TargetFolderId:#Exchange","BaseFolderId":{"__type":"FolderId:#Exchange","Id":"F1","ChangeKey":"fck"}}"#), "{s}");
        let env = envelope("CreateFolder", V_2017, "UTC", create_folder_body(&FolderRef::Id(folder_id()), "x"));
        assert!(serde_json::to_string(&env).unwrap().starts_with(r#"{"__type":"CreateFolderJsonRequest:#Exchange","Header":{"__type":"JsonRequestHeaders:#Exchange""#));
    }

    #[test]
    fn rename_move_delete_folder_bodies_exact() {
        let s = serde_json::to_string(&rename_folder_body(&folder_id(), "AIOWA тест 2")).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"UpdateFolderRequest:#Exchange","FolderChanges":[{"__type":"FolderChange:#Exchange","FolderId":{"__type":"FolderId:#Exchange","Id":"F1","ChangeKey":"fck"},"Updates":[{"__type":"SetFolderField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"folder:DisplayName"},"Folder":{"__type":"Folder:#Exchange","DisplayName":"AIOWA тест 2"}}]}]}"#
        );
        let s = serde_json::to_string(&move_folder_body(&FolderId { id: "F1".into(), change_key: None }, &FolderRef::Distinguished(DistinguishedFolder::Inbox))).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"MoveFolderRequest:#Exchange","ToFolderId":{"__type":"TargetFolderId:#Exchange","BaseFolderId":{"__type":"DistinguishedFolderId:#Exchange","Id":"inbox"}},"FolderIds":[{"__type":"FolderId:#Exchange","Id":"F1"}]}"#
        );
        let s = serde_json::to_string(&delete_folder_body(&folder_id(), "HardDelete")).unwrap();
        assert_eq!(s, r#"{"__type":"DeleteFolderRequest:#Exchange","FolderIds":[{"__type":"FolderId:#Exchange","Id":"F1","ChangeKey":"fck"}],"DeleteType":"HardDelete"}"#);
        assert!(serde_json::to_string(&delete_folder_body(&folder_id(), "MoveToDeletedItems")).unwrap().ends_with(r#""DeleteType":"MoveToDeletedItems"}"#));
    }

    #[test]
    fn parses_written_folder_id() {
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"__type":"FolderInfoResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError","Folders":[{"__type":"Folder:#Exchange","FolderId":{"__type":"FolderId:#Exchange","Id":"NEW","ChangeKey":"nck"}}]}]}}}"#).unwrap();
        let msgs = response_messages(&v);
        assert_eq!(parse_written_folder_id(msgs[0]), Some(FolderId { id: "NEW".into(), change_key: Some("nck".into()) }));
        assert!(message_error(msgs[0]).is_none());
        // DeleteFolder: успех без Folders; отказ — ResponseClass Error с кодом.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"__type":"DeleteFolderResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError"}]}}}"#).unwrap();
        assert_eq!(parse_written_folder_id(response_messages(&v)[0]), None);
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"__type":"FolderInfoResponseMessage:#Exchange","ResponseClass":"Error","ResponseCode":"ErrorFolderExists","MessageText":"Папка с таким именем уже есть","Folders":[]}]}}}"#).unwrap();
        assert_eq!(message_error(response_messages(&v)[0]), Some(("ErrorFolderExists".into(), "Папка с таким именем уже есть".into())));
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
            r#"{"__type":"UpdateItemRequest:#Exchange","ConflictResolution":"AlwaysOverwrite","MessageDisposition":"SaveOnly","SuppressReadReceipts":true,"ItemChanges":[{"__type":"ItemChange:#Exchange","ItemId":{"__type":"ItemId:#Exchange","Id":"D","ChangeKey":"dk"},"Updates":[{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:Subject"},"Item":{"__type":"Message:#Exchange","Subject":"Тема"}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:Body"},"Item":{"__type":"Message:#Exchange","Body":{"__type":"BodyContentType:#Exchange","BodyType":"HTML","Value":"<p>Привет</p>"}}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"message:ToRecipients"},"Item":{"__type":"Message:#Exchange","ToRecipients":[{"__type":"EmailAddress:#Exchange","Name":"Иван","EmailAddress":"i@x.ru","RoutingType":"SMTP"}]}},{"__type":"DeleteItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"message:CcRecipients"}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"message:BccRecipients"},"Item":{"__type":"Message:#Exchange","BccRecipients":[{"__type":"EmailAddress:#Exchange","EmailAddress":"b@x.ru","RoutingType":"SMTP"}]}}]}]}"#
        );
    }

    #[test]
    fn update_draft_body_clears_all_recipient_lists_when_empty() {
        let msg = OutgoingMessage { subject: "Т".into(), body_html: "<div>х</div>".into(), ..OutgoingMessage::default() };
        let s = serde_json::to_string(&update_draft_body(&ItemId { id: "D".into(), change_key: None }, &msg)).unwrap();
        assert_eq!(s.matches(r#""__type":"DeleteItemField:#Exchange""#).count(), 3, "три пустых списка — три DeleteItemField: {s}");
        assert_eq!(s.matches(r#""__type":"SetItemField:#Exchange""#).count(), 2, "тема и тело; обычная важность не пишется: {s}");
        assert!(!s.contains("item:Importance"), "{s}");
        assert!(s.contains(r#""ItemId":{"__type":"ItemId:#Exchange","Id":"D"},"Updates""#), "без ChangeKey ключ не передаётся: {s}");
    }

    #[test]
    fn update_draft_body_writes_importance_only_when_not_normal() {
        let msg = OutgoingMessage { subject: "Т".into(), body_html: "<div>х</div>".into(), importance: Importance::High, ..OutgoingMessage::default() };
        let s = serde_json::to_string(&update_draft_body(&ItemId { id: "D".into(), change_key: None }, &msg)).unwrap();
        assert!(s.contains(r#"{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:Importance"},"Item":{"__type":"Message:#Exchange","Importance":"High"}}"#), "{s}");
    }

    #[test]
    fn delete_attachment_body_exact() {
        let s = serde_json::to_string(&delete_attachment_body(&["A1".to_string(), "A2".to_string()])).unwrap();
        assert_eq!(s, r#"{"__type":"DeleteAttachmentRequest:#Exchange","AttachmentIds":[{"__type":"AttachmentId:#Exchange","Id":"A1"},{"__type":"AttachmentId:#Exchange","Id":"A2"}]}"#);
        let env = envelope("DeleteAttachment", V_2017, "UTC", delete_attachment_body(&["A1".to_string()]));
        assert!(serde_json::to_string(&env).unwrap().starts_with(r#"{"__type":"DeleteAttachmentJsonRequest:#Exchange","Header":{"#));
    }

    #[test]
    fn parses_delete_attachment_root() {
        // Форма схемы EWS: атрибуты элемента m:RootItemId становятся членами объекта.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"__type":"DeleteAttachmentResponseMessage:#Exchange","ResponseClass":"Success","ResponseCode":"NoError","RootItemId":{"__type":"RootItemIdType:#Exchange","RootItemId":"D1","RootItemChangeKey":"k3"}}]}}}"#).unwrap();
        assert_eq!(parse_delete_attachment_root(response_messages(&v)[0]), Some(ItemId { id: "D1".into(), change_key: Some("k3".into()) }));
        // Форма ItemId: Id/ChangeKey.
        let m: Value = serde_json::json!({ "ResponseClass": "Success", "RootItemId": { "__type": "ItemId:#Exchange", "Id": "D1", "ChangeKey": "k4" } });
        assert_eq!(parse_delete_attachment_root(&m), Some(ItemId { id: "D1".into(), change_key: Some("k4".into()) }));
        // Строка с ключом рядом.
        let m: Value = serde_json::json!({ "RootItemId": "D1", "RootItemChangeKey": "k5" });
        assert_eq!(parse_delete_attachment_root(&m), Some(ItemId { id: "D1".into(), change_key: Some("k5".into()) }));
        // Без ключа и без идентификатора.
        let m: Value = serde_json::json!({ "RootItemId": { "RootItemId": "D1" } });
        assert_eq!(parse_delete_attachment_root(&m), Some(ItemId { id: "D1".into(), change_key: None }));
        assert_eq!(parse_delete_attachment_root(&serde_json::json!({ "ResponseClass": "Success" })), None);
        assert_eq!(parse_delete_attachment_root(&serde_json::json!({ "RootItemId": "" })), None);
        assert_eq!(parse_delete_attachment_root(&serde_json::json!({ "RootItemId": { "ChangeKey": "k" } })), None);
    }

    #[test]
    fn get_draft_body_exact() {
        let id = ItemId { id: "D".into(), change_key: Some("dk".into()) };
        assert_eq!(
            serde_json::to_string(&get_draft_body(&id, DraftShape::AllProperties, 4096)).unwrap(),
            r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"AllProperties","BodyType":"HTML","FilterHtmlContent":true,"BlockExternalImagesIfSenderUntrusted":true,"MaximumBodySize":4096,"MaximumRecipientsToReturn":500},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"D","ChangeKey":"dk"}],"ShapeName":null}"#
        );
        assert_eq!(
            serde_json::to_string(&get_draft_body(&id, DraftShape::NormalizedBody, 4096)).unwrap(),
            r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"IdOnly","FilterHtmlContent":true,"BlockExternalImagesIfSenderUntrusted":true,"MaximumBodySize":4096,"MaximumRecipientsToReturn":200},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"D","ChangeKey":"dk"}],"ShapeName":"ItemNormalizedBody"}"#
        );
    }

    #[test]
    fn parses_draft_content() {
        let item: Value = serde_json::json!({
            "__type": "Message:#Exchange",
            "ItemId": { "Id": "D1", "ChangeKey": "k1" },
            "Subject": "[AIOWA] черновик",
            "IsDraft": true,
            "Importance": "High",
            "Body": { "BodyType": "HTML", "Value": "<div>Привет</div>", "IsTruncated": false },
            "ToRecipients": [ { "Name": "Я", "EmailAddress": "me@x.ru", "RoutingType": "SMTP", "MailboxType": "Mailbox" } ],
            "CcRecipients": [],
            "Attachments": [
                { "__type": "FileAttachment:#Exchange", "AttachmentId": { "Id": "A1", "RootItemId": "D1", "RootItemChangeKey": "k1" }, "Name": "a.txt", "ContentType": "text/plain", "Size": 120, "IsInline": false },
                { "__type": "FileAttachment:#Exchange", "AttachmentId": { "Id": "A2" }, "Name": "img.png", "ContentType": "image/png", "Size": 5, "IsInline": true, "ContentId": "cid1" }
            ]
        });
        let d = parse_draft_content(&item).unwrap();
        assert_eq!(d.id, ItemId { id: "D1".into(), change_key: Some("k1".into()) });
        assert_eq!((d.subject.as_str(), d.body_html.as_str(), d.importance), ("[AIOWA] черновик", "<div>Привет</div>", Importance::High));
        assert_eq!(d.to.len(), 1);
        assert!(d.cc.is_empty() && d.bcc.is_empty());
        assert_eq!(d.attachments.len(), 2);
        assert_eq!(d.file_attachments().map(|a| a.id.as_str()).collect::<Vec<_>>(), vec!["A1"]);
        assert_eq!(d.attachments[0].size, Some(120));
        assert_eq!(body_key_of(&item), Some("Body"));

        // Запасная форма: NormalizedBody вместо Body; тело типа Text переводится в HTML.
        let item: Value = serde_json::json!({ "ItemId": { "Id": "D2" }, "NormalizedBody": { "BodyType": "Text", "Value": "строка 1\n\nстрока <2> & 3" } });
        let d = parse_draft_content(&item).unwrap();
        assert_eq!(d.body_html, "<div>строка 1</div><div><br></div><div>строка &lt;2&gt; &amp; 3</div>");
        assert_eq!((d.subject.as_str(), d.importance, d.attachments.len()), ("", Importance::Normal, 0));
        assert_eq!(body_key_of(&item), Some("NormalizedBody"));

        // Без тела — пустая строка, без ItemId — None.
        let item: Value = serde_json::json!({ "ItemId": { "Id": "D3" }, "Subject": "x" });
        assert_eq!(parse_draft_content(&item).unwrap().body_html, "");
        assert_eq!(body_key_of(&item), None);
        assert!(parse_draft_content(&serde_json::json!({ "Subject": "x" })).is_none());
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
            r#"{"__type":"FindItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"IdOnly","AdditionalProperties":[{"__type":"PropertyUri:#Exchange","FieldURI":"item:Subject"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Start"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:End"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:IsAllDayEvent"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Location"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:EnhancedLocation"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Organizer"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:LegacyFreeBusyStatus"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:CalendarItemType"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:IsMeeting"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:IsCancelled"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:MyResponseType"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:Sensitivity"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:HasAttachments"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:LastModifiedTime"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:AppointmentState"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:ReminderIsSet"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:ReminderMinutesBeforeStart"}]},"ParentFolderIds":[{"__type":"DistinguishedFolderId:#Exchange","Id":"calendar"}],"Traversal":"Shallow","Paging":{"__type":"CalendarPageView:#Exchange","StartDate":"2026-10-20T00:00:00Z","EndDate":"2026-10-27T00:00:00Z"}}"#
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
        assert!(s.contains(r#"{"__type":"PropertyUri:#Exchange","FieldURI":"item:ReminderIsSet"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:ReminderMinutesBeforeStart"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:RequiredAttendees"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:OptionalAttendees"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Recurrence"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:Body"}]},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"E","ChangeKey":"ek"}],"ShapeName":null}"#), "{s}");
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
            {"__type":"CalendarItem:#Exchange","ItemId":{"Id":"E"},"Start":"2026-10-23T10:00:00Z","End":"2026-10-23T11:00:00Z","FreeBusyType":"Tentative","ResponseType":"Decline","AppointmentState":5,"Location":{"__type":"EnhancedLocation:#Exchange","DisplayName":"Кабинет 7"},"ReminderIsSet":true,"ReminderMinutesBeforeStart":15},
            {"__type":"CalendarItem:#Exchange","ItemId":{"Id":"F"},"Start":"2026-10-23T12:00:00Z","End":"2026-10-23T13:00:00Z","ReminderIsSet":false,"ReminderMinutesBeforeStart":15}
          ]}}"#).unwrap();
        let (items, last) = parse_calendar_view(&msg);
        assert!(!last);
        assert_eq!(items.len(), 5, "событие без Start пропущено");
        // Имена OWA JSON (снято с сервера): FreeBusyType, ResponseType, отмена битом AppointmentState, место объектом.
        let e = &items[3];
        assert_eq!((e.free_busy, e.my_response, e.is_cancelled, e.location.as_deref()), (FreeBusy::Tentative, ResponseType::Decline, true, Some("Кабинет 7")));
        assert_eq!(e.reminder_minutes, Some(15));
        assert_eq!(items[4].reminder_minutes, None, "ReminderIsSet: false гасит минуты");
        assert_eq!(items[0].reminder_minutes, None, "без полей напоминания его нет");
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

    fn new_item() -> NewCalendarItem {
        NewCalendarItem {
            subject: "[AIOWA] проверка календаря".into(),
            start: "2026-10-25T06:00:00Z".parse().unwrap(),
            end: "2026-10-25T06:30:00Z".parse().unwrap(),
            is_all_day: false,
            location: Some(" Переговорная 3 ".into()),
            body_html: Some("<div>Повестка</div>".into()),
            required: vec![],
            optional: vec![],
            reminder_minutes: Some(15),
            free_busy: FreeBusy::Tentative,
            sensitivity_private: false,
        }
    }

    /// Место: объект `EnhancedLocation` с `DisplayName`.
    const LOCATION_JSON: &str = r#"{"__type":"EnhancedLocation:#Exchange","DisplayName":"Переговорная 3"}"#;

    #[test]
    fn create_calendar_item_body_exact() {
        // Без участников: SendToNone, списков участников нет, место обрезано, напоминание включено.
        let s = serde_json::to_string(&create_calendar_item_body(&new_item(), false)).unwrap();
        assert_eq!(
            s,
            format!(
                r#"{{"__type":"CreateItemRequest:#Exchange","SendMeetingInvitations":"SendToNone","SavedItemFolderId":{{"__type":"TargetFolderId:#Exchange","BaseFolderId":{{"__type":"DistinguishedFolderId:#Exchange","Id":"calendar"}}}},"Items":[{{"__type":"CalendarItem:#Exchange","Subject":"[AIOWA] проверка календаря","Body":{{"__type":"BodyContentType:#Exchange","BodyType":"HTML","Value":"<div>Повестка</div>"}},"Start":"2026-10-25T06:00:00Z","End":"2026-10-25T06:30:00Z","IsAllDayEvent":false,"Location":{LOCATION_JSON},"ReminderIsSet":true,"ReminderMinutesBeforeStart":15,"FreeBusyType":"Tentative","Sensitivity":"Normal"}}]}}"#
            )
        );
        let env = envelope("CreateItem", V_2017, "Russian Standard Time", create_calendar_item_body(&new_item(), false));
        assert!(serde_json::to_string(&env).unwrap().starts_with(
            r#"{"__type":"CreateItemJsonRequest:#Exchange","Header":{"__type":"JsonRequestHeaders:#Exchange","RequestServerVersion":"V2017_08_18","TimeZoneContext":{"__type":"TimeZoneContext:#Exchange","TimeZoneDefinition":{"__type":"TimeZoneDefinitionType:#Exchange","Id":"Russian Standard Time"}}},"Body":{"__type":"CreateItemRequest:#Exchange""#
        ));
        // С участниками: SendToAllAndSaveCopy, AttendeeType с Mailbox; без тела, места и напоминания; весь день без
        // пояса пользователя — полуночи UTC с Z (как хранит NewCalendarItem), частное.
        let mut item = new_item();
        item.required = vec![Participant { name: Some("Иван".into()), email: Some("i@x.ru".into()), routing_type: None }];
        item.optional = vec![Participant { name: None, email: Some("o@x.ru".into()), routing_type: Some("SMTP".into()) }];
        item.body_html = None;
        item.location = Some("   ".into());
        item.reminder_minutes = None;
        item.is_all_day = true;
        item.start = "2026-10-26T00:00:00Z".parse().unwrap();
        item.end = "2026-10-27T00:00:00Z".parse().unwrap();
        item.free_busy = FreeBusy::Oof;
        item.sensitivity_private = true;
        let s = serde_json::to_string(&create_calendar_item_body(&item, false)).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"CreateItemRequest:#Exchange","SendMeetingInvitations":"SendToAllAndSaveCopy","SavedItemFolderId":{"__type":"TargetFolderId:#Exchange","BaseFolderId":{"__type":"DistinguishedFolderId:#Exchange","Id":"calendar"}},"Items":[{"__type":"CalendarItem:#Exchange","Subject":"[AIOWA] проверка календаря","Start":"2026-10-26T00:00:00Z","End":"2026-10-27T00:00:00Z","IsAllDayEvent":true,"RequiredAttendees":[{"__type":"AttendeeType:#Exchange","Mailbox":{"__type":"EmailAddress:#Exchange","Name":"Иван","EmailAddress":"i@x.ru","RoutingType":"SMTP"}}],"OptionalAttendees":[{"__type":"AttendeeType:#Exchange","Mailbox":{"__type":"EmailAddress:#Exchange","EmailAddress":"o@x.ru","RoutingType":"SMTP"}}],"ReminderIsSet":false,"FreeBusyType":"OOF","Sensitivity":"Private"}]}"#
        );
        // Весь день в контексте пояса пользователя: местные полуночи дат без зоны; даты — и из полуночей UTC, и из местных (МСК).
        let s = serde_json::to_string(&create_calendar_item_body(&item, true)).unwrap();
        assert!(s.contains(r#""Start":"2026-10-26T00:00:00","End":"2026-10-27T00:00:00","IsAllDayEvent":true"#), "{s}");
        item.start = "2026-10-25T21:00:00Z".parse().unwrap();
        item.end = "2026-10-26T21:00:00Z".parse().unwrap();
        let s = serde_json::to_string(&create_calendar_item_body(&item, true)).unwrap();
        assert!(s.contains(r#""Start":"2026-10-26T00:00:00","End":"2026-10-27T00:00:00","IsAllDayEvent":true"#), "{s}");
        // Не весь день — момент UTC с Z даже в контексте пояса пользователя.
        assert_eq!(calendar_bound("2026-10-25T06:00:00Z".parse().unwrap(), false), "2026-10-25T06:00:00Z");
    }

    #[test]
    fn update_calendar_item_body_exact() {
        let id = ItemId { id: "E".into(), change_key: Some("ek".into()) };
        let changes = CalendarItemChanges {
            start: Some("2026-10-25T07:00:00Z".parse().unwrap()),
            end: Some("2026-10-25T07:45:00Z".parse().unwrap()),
            location: Some(Some(" Переговорная 3 ".into())),
            ..Default::default()
        };
        let s = serde_json::to_string(&update_calendar_item_body(&id, &changes, false, false)).unwrap();
        assert_eq!(
            s,
            format!(
                r#"{{"__type":"UpdateItemRequest:#Exchange","ConflictResolution":"AlwaysOverwrite","SendCalendarInvitationsOrCancellations":"SendToNone","ItemChanges":[{{"__type":"ItemChange:#Exchange","ItemId":{{"__type":"ItemId:#Exchange","Id":"E","ChangeKey":"ek"}},"Updates":[{{"__type":"SetItemField:#Exchange","Path":{{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Start"}},"Item":{{"__type":"CalendarItem:#Exchange","Start":"2026-10-25T07:00:00Z"}}}},{{"__type":"SetItemField:#Exchange","Path":{{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:End"}},"Item":{{"__type":"CalendarItem:#Exchange","End":"2026-10-25T07:45:00Z"}}}},{{"__type":"SetItemField:#Exchange","Path":{{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:EnhancedLocation"}},"Item":{{"__type":"CalendarItem:#Exchange","Location":{LOCATION_JSON}}}}}]}}]}}"#
            )
        );
        // Все остальные поля: снятие места (EnhancedLocation с пустым DisplayName) и напоминания, опустевшие и заполненные списки участников, тело, занятость, рассылка.
        let changes = CalendarItemChanges {
            subject: Some("Тема".into()),
            is_all_day: Some(true),
            location: Some(None),
            body_html: Some("<p>x</p>".into()),
            required: Some(vec![]),
            optional: Some(vec![Participant { name: None, email: Some("o@x.ru".into()), routing_type: None }]),
            reminder_minutes: Some(None),
            free_busy: Some(FreeBusy::Free),
            ..Default::default()
        };
        let s = serde_json::to_string(&update_calendar_item_body(&ItemId { id: "E".into(), change_key: None }, &changes, true, false)).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"UpdateItemRequest:#Exchange","ConflictResolution":"AlwaysOverwrite","SendCalendarInvitationsOrCancellations":"SendToAllAndSaveCopy","ItemChanges":[{"__type":"ItemChange:#Exchange","ItemId":{"__type":"ItemId:#Exchange","Id":"E"},"Updates":[{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:Subject"},"Item":{"__type":"CalendarItem:#Exchange","Subject":"Тема"}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:IsAllDayEvent"},"Item":{"__type":"CalendarItem:#Exchange","IsAllDayEvent":true}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:EnhancedLocation"},"Item":{"__type":"CalendarItem:#Exchange","Location":{"__type":"EnhancedLocation:#Exchange","DisplayName":""}}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:Body"},"Item":{"__type":"CalendarItem:#Exchange","Body":{"__type":"BodyContentType:#Exchange","BodyType":"HTML","Value":"<p>x</p>"}}},{"__type":"DeleteItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:RequiredAttendees"}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:OptionalAttendees"},"Item":{"__type":"CalendarItem:#Exchange","OptionalAttendees":[{"__type":"AttendeeType:#Exchange","Mailbox":{"__type":"EmailAddress:#Exchange","EmailAddress":"o@x.ru","RoutingType":"SMTP"}}]}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:ReminderIsSet"},"Item":{"__type":"CalendarItem:#Exchange","ReminderIsSet":false}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:LegacyFreeBusyStatus"},"Item":{"__type":"CalendarItem:#Exchange","FreeBusyType":"Free"}}]}]}"#
        );
        // Включение напоминания — два поля подряд.
        let s = serde_json::to_string(&update_calendar_item_body(&id, &CalendarItemChanges { reminder_minutes: Some(Some(30)), ..Default::default() }, false, false)).unwrap();
        assert!(s.contains(r#""Updates":[{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:ReminderIsSet"},"Item":{"__type":"CalendarItem:#Exchange","ReminderIsSet":true}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"item:ReminderMinutesBeforeStart"},"Item":{"__type":"CalendarItem:#Exchange","ReminderMinutesBeforeStart":30}}]"#), "{s}");
        assert!(serde_json::to_string(&update_calendar_item_body(&id, &CalendarItemChanges::default(), false, false)).unwrap().contains(r#""Updates":[]"#));
        // Весь день в контексте пояса пользователя: границы — местные полуночи дат без зоны, IsAllDayEvent рядом.
        let all_day = CalendarItemChanges { start: Some("2026-10-26T00:00:00Z".parse().unwrap()), end: Some("2026-10-27T00:00:00Z".parse().unwrap()), is_all_day: Some(true), ..Default::default() };
        let s = serde_json::to_string(&update_calendar_item_body(&id, &all_day, false, true)).unwrap();
        assert!(s.contains(r#""Item":{"__type":"CalendarItem:#Exchange","Start":"2026-10-26T00:00:00"}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:End"},"Item":{"__type":"CalendarItem:#Exchange","End":"2026-10-27T00:00:00"}},{"__type":"SetItemField:#Exchange","Path":{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:IsAllDayEvent"},"Item":{"__type":"CalendarItem:#Exchange","IsAllDayEvent":true}}"#), "{s}");
        // Без пояса те же границы — моменты UTC с Z.
        let s = serde_json::to_string(&update_calendar_item_body(&id, &all_day, false, false)).unwrap();
        assert!(s.contains(r#""Start":"2026-10-26T00:00:00Z"}}"#) && s.contains(r#""End":"2026-10-27T00:00:00Z"}}"#), "{s}");
    }

    #[test]
    fn parses_user_time_zone_and_probe_fields() {
        let msg: Value = serde_json::from_str(r#"{"ResponseClass":"Success","UserConfiguration":{"Dictionary":[
            {"DictionaryKey":{"Type":"String","Value":["signaturehtml"]},"DictionaryValue":{"Type":"String","Value":["<p>x</p>"]}},
            {"DictionaryKey":{"Type":"String","Value":["timezone"]},"DictionaryValue":{"Type":"String","Value":[" Russian Standard Time "]}}]}}"#).unwrap();
        assert_eq!(parse_user_time_zone(&msg).as_deref(), Some("Russian Standard Time"));
        let msg: Value = serde_json::from_str(r#"{"UserConfiguration":{"Dictionary":[{"DictionaryKey":{"Value":["TimeZone"]},"DictionaryValue":{"Value":[""]}}]}}"#).unwrap();
        assert_eq!(parse_user_time_zone(&msg), None);
        assert_eq!(parse_user_time_zone(&json!({"UserConfiguration":{}})), None);
        let item: Value = serde_json::from_str(r#"{"__type":"CalendarItem:#Exchange","Subject":"секрет","Body":{"Value":"x"},"IsAllDayEvent":true,"Start":"2026-10-25T21:00:00Z","End":"2026-10-26T21:00:00Z","StartTimeZoneId":"Russian Standard Time","EndTimeZoneId":"Russian Standard Time","TimeZone":"(UTC+03:00) Moscow","Location":{"__type":"EnhancedLocation:#Exchange","DisplayName":"Кабинет"}}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&calendar_probe_fields(&item)).unwrap(),
            r#"{"IsAllDayEvent":true,"Start":"2026-10-25T21:00:00Z","End":"2026-10-26T21:00:00Z","StartTimeZoneId":"Russian Standard Time","EndTimeZoneId":"Russian Standard Time","TimeZone":"(UTC+03:00) Moscow","Location":{"__type":"EnhancedLocation:#Exchange","DisplayName":"Кабинет"}}"#
        );
        let body = serde_json::to_string(&get_calendar_probe_body(&ItemId { id: "E".into(), change_key: None })).unwrap();
        assert!(body.contains(r#""BaseShape":"AllProperties""#), "{body}");
        assert!(body.ends_with(r#""MaximumRecipientsToReturn":500,"AdditionalProperties":[{"__type":"ExtendedPropertyUri:#Exchange","DistinguishedPropertySetId":"Appointment","PropertyId":33288,"PropertyType":"String"}]},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"E"}],"ShapeName":null}"#), "{body}");
    }

    #[test]
    fn delete_and_respond_bodies_exact() {
        let id = ItemId { id: "E".into(), change_key: Some("ek".into()) };
        assert_eq!(
            serde_json::to_string(&delete_calendar_item_body(&id, "MoveToDeletedItems", false)).unwrap(),
            r#"{"__type":"DeleteItemRequest:#Exchange","ItemIds":[{"__type":"ItemId:#Exchange","Id":"E","ChangeKey":"ek"}],"DeleteType":"MoveToDeletedItems","SendMeetingCancellations":"SendToNone"}"#
        );
        let s = serde_json::to_string(&delete_calendar_item_body(&id, "HardDelete", true)).unwrap();
        assert!(s.ends_with(r#""DeleteType":"HardDelete","SendMeetingCancellations":"SendToAllAndSaveCopy"}"#), "{s}");
        assert_eq!(
            serde_json::to_string(&respond_to_meeting_body(&id, MeetingResponse::Accept, true, None)).unwrap(),
            r#"{"__type":"CreateItemRequest:#Exchange","MessageDisposition":"SendAndSaveCopy","Items":[{"__type":"AcceptItem:#Exchange","ReferenceItemId":{"__type":"ItemId:#Exchange","Id":"E","ChangeKey":"ek"}}]}"#
        );
        assert_eq!(
            serde_json::to_string(&respond_to_meeting_body(&id, MeetingResponse::Tentative, false, Some("<p>Постараюсь</p>"))).unwrap(),
            r#"{"__type":"CreateItemRequest:#Exchange","MessageDisposition":"SaveOnly","Items":[{"__type":"TentativelyAcceptItem:#Exchange","ReferenceItemId":{"__type":"ItemId:#Exchange","Id":"E","ChangeKey":"ek"},"Body":{"__type":"BodyContentType:#Exchange","BodyType":"HTML","Value":"<p>Постараюсь</p>"}}]}"#
        );
        let s = serde_json::to_string(&respond_to_meeting_body(&id, MeetingResponse::Decline, true, Some("  "))).unwrap();
        assert!(s.contains(r#"{"__type":"DeclineItem:#Exchange","ReferenceItemId":{"#) && !s.contains("Body"), "пустое тело не передаётся: {s}");
    }

    #[test]
    fn get_meeting_request_body_exact() {
        let id = ItemId { id: "M".into(), change_key: Some("mk".into()) };
        let s = serde_json::to_string(&get_meeting_request_body(&id, CalendarItemShape::Explicit, 4096)).unwrap();
        assert_eq!(
            s,
            r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"IdOnly","BodyType":"HTML","FilterHtmlContent":true,"BlockExternalImagesIfSenderUntrusted":true,"MaximumBodySize":4096,"AdditionalProperties":[{"__type":"PropertyUri:#Exchange","FieldURI":"item:ItemClass"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:Subject"},{"__type":"PropertyUri:#Exchange","FieldURI":"item:DateTimeReceived"},{"__type":"PropertyUri:#Exchange","FieldURI":"message:From"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Start"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:End"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:IsAllDayEvent"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Location"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:EnhancedLocation"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Organizer"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:MyResponseType"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:CalendarItemType"},{"__type":"PropertyUri:#Exchange","FieldURI":"calendar:Recurrence"},{"__type":"PropertyUri:#Exchange","FieldURI":"meeting:IsOutOfDate"},{"__type":"PropertyUri:#Exchange","FieldURI":"meeting:AssociatedCalendarItemId"},{"__type":"PropertyUri:#Exchange","FieldURI":"meeting:ResponseType"},{"__type":"PropertyUri:#Exchange","FieldURI":"meeting:HasBeenProcessed"},{"__type":"PropertyUri:#Exchange","FieldURI":"meeting:IsDelegated"}]},"ItemIds":[{"__type":"ItemId:#Exchange","Id":"M","ChangeKey":"mk"}],"ShapeName":null}"#
        );
        let s = serde_json::to_string(&get_meeting_request_body(&id, CalendarItemShape::AllProperties, 4096)).unwrap();
        assert!(s.starts_with(r#"{"__type":"GetItemRequest:#Exchange","ItemShape":{"__type":"ItemResponseShape:#Exchange","BaseShape":"AllProperties","BodyType":"HTML","FilterHtmlContent":true,"BlockExternalImagesIfSenderUntrusted":true,"MaximumBodySize":4096},"ItemIds":["#), "{s}");
    }

    #[test]
    fn parses_meeting_request_info() {
        // Приглашение: Organizer, свой ответ под именем OWA JSON ResponseType, связанное событие, повторение.
        let v: Value = serde_json::from_str(r#"{"__type":"MeetingRequest:#Exchange","ItemId":{"Id":"M","ChangeKey":"mk"},"ItemClass":"IPM.Schedule.Meeting.Request","Subject":"Планёрка",
          "From":{"Mailbox":{"Name":"Секретарь","EmailAddress":"s@x.ru"}},"Organizer":{"Mailbox":{"Name":"Иван","EmailAddress":"i@x.ru","RoutingType":"SMTP"}},
          "Start":"2026-10-27T07:00:00Z","End":"2026-10-27T07:30:00Z","IsAllDayEvent":false,"Location":"Зал","ResponseType":"NoResponseReceived","IsOutOfDate":true,
          "AssociatedCalendarItemId":{"Id":"C1","ChangeKey":"ck"},"Recurrence":{"RecurrencePattern":{"__type":"DailyRecurrence:#Exchange","Interval":1},"RecurrenceRange":{"__type":"NoEndRecurrence:#Exchange","StartDate":"2026-10-27"}}}"#).unwrap();
        let info = parse_meeting_request_info(&v);
        assert_eq!(info.kind, MeetingMessageKind::Request);
        assert_eq!(info.start, Some("2026-10-27T07:00:00Z".parse().unwrap()));
        assert_eq!(info.end.unwrap().duration_since(info.start.unwrap()).as_mins(), 30);
        assert_eq!((info.is_all_day, info.location.as_deref(), info.organizer.as_ref().unwrap().email.as_deref()), (false, Some("Зал"), Some("i@x.ru")));
        assert_eq!((info.my_response, info.is_out_of_date, info.response_type_of_sender), (ResponseType::NoResponseReceived, true, None));
        assert_eq!(info.associated_calendar_item, Some(ItemId { id: "C1".into(), change_key: Some("ck".into()) }));
        assert_eq!(info.recurrence_text.as_deref(), Some("Ежедневно, с 27.10.2026"));
        // Имя EWS MyResponseType важнее ResponseType; без Organizer — From.
        let v: Value = serde_json::from_str(r#"{"ItemClass":"IPM.Schedule.Meeting.Request","From":{"Mailbox":{"EmailAddress":"s@x.ru"}},"MyResponseType":"Accept","ResponseType":"Unknown","Start":"2026-10-27T07:00:00Z"}"#).unwrap();
        let info = parse_meeting_request_info(&v);
        assert_eq!((info.my_response, info.organizer.unwrap().email.as_deref(), info.end, info.associated_calendar_item), (ResponseType::Accept, Some("s@x.ru"), None, None));
        // Ответ участника: ResponseType — его ответ; без него — по классу письма. From — сам
        // ответивший, организатором он не считается.
        let v: Value = serde_json::from_str(r#"{"ItemClass":"IPM.Schedule.Meeting.Resp.Tent","From":{"Mailbox":{"EmailAddress":"p@x.ru"}},"ResponseType":"Tentative","Start":"2026-10-27T07:00:00Z","End":"2026-10-27T08:00:00Z"}"#).unwrap();
        let info = parse_meeting_request_info(&v);
        assert_eq!((info.kind, info.my_response, info.response_type_of_sender), (MeetingMessageKind::Response, ResponseType::Unknown, Some(ResponseType::Tentative)));
        assert!(info.organizer.is_none(), "у письма-ответа From — участник, а не организатор");
        let v: Value = serde_json::from_str(r#"{"ItemClass":"IPM.Schedule.Meeting.Resp.Neg"}"#).unwrap();
        assert_eq!(parse_meeting_request_info(&v).response_type_of_sender, Some(ResponseType::Decline));
        // Отмена (организатор — из From) и не письмо о встрече.
        let v: Value = serde_json::from_str(r#"{"ItemClass":"IPM.Schedule.Meeting.Canceled","From":{"Mailbox":{"EmailAddress":"o@x.ru"}},"Start":"2026-10-27T07:00:00Z","IsOutOfDate":false}"#).unwrap();
        let info = parse_meeting_request_info(&v);
        assert_eq!((info.kind, info.organizer.unwrap().email.as_deref()), (MeetingMessageKind::Cancellation, Some("o@x.ru")));
        let info = parse_meeting_request_info(&json!({"ItemClass":"IPM.Note"}));
        assert_eq!((info.kind, info.start, info.my_response), (MeetingMessageKind::Other, None, ResponseType::Unknown));
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

    #[test]
    fn find_people_body_exact() {
        assert_eq!(
            serde_json::to_string(&find_people_body("Пет", 25)).unwrap(),
            r#"{"__type":"FindPeopleRequest:#Exchange","IndexedPageItemView":{"__type":"IndexedPageView:#Exchange","BasePoint":"Beginning","Offset":0,"MaxEntriesReturned":25},"QueryString":"Пет","ParentFolderId":{"__type":"TargetFolderId:#Exchange","BaseFolderId":{"__type":"DistinguishedFolderId:#Exchange","Id":"directory"}},"PersonaShape":{"__type":"PersonaResponseShape:#Exchange","BaseShape":"Default"},"ShouldResolveOneOffEmailAddress":false,"SearchPeopleSuggestionIndex":false}"#
        );
        let env = envelope("FindPeople", V_2017, "UTC", find_people_body("x", 5));
        assert!(serde_json::to_string(&env).unwrap().starts_with(r#"{"__type":"FindPeopleJsonRequest:#Exchange","Header":{"__type":"JsonRequestHeaders:#Exchange""#));
        let s = serde_json::to_string(&find_people_body_with("x", 5, &["persona:Title", "persona:Department"])).unwrap();
        assert!(
            s.contains(r#""PersonaShape":{"__type":"PersonaResponseShape:#Exchange","BaseShape":"Default","AdditionalProperties":[{"__type":"PropertyUri:#Exchange","FieldURI":"persona:Title"},{"__type":"PropertyUri:#Exchange","FieldURI":"persona:Department"}]},"ShouldResolveOneOffEmailAddress""#),
            "{s}"
        );
        assert_eq!(find_people_body_with("x", 5, &[]), find_people_body("x", 5));
        assert!(serde_json::to_string(&resolve_names_body_shaped("x", Some("Default"))).unwrap().ends_with(r#""ReturnFullContactData":true,"SearchScope":"ActiveDirectory","ContactDataShape":"Default"}"#));
    }

    #[test]
    fn resolve_names_and_expand_dl_bodies_exact() {
        assert_eq!(
            serde_json::to_string(&resolve_names_body("ivan@x.ru", true)).unwrap(),
            r#"{"__type":"ResolveNamesRequest:#Exchange","UnresolvedEntry":"ivan@x.ru","ReturnFullContactData":true,"SearchScope":"ActiveDirectory","ContactDataShape":"AllProperties"}"#
        );
        assert_eq!(
            serde_json::to_string(&resolve_names_body("Иван", false)).unwrap(),
            r#"{"__type":"ResolveNamesRequest:#Exchange","UnresolvedEntry":"Иван","ReturnFullContactData":false,"SearchScope":"ActiveDirectory"}"#
        );
        assert_eq!(
            serde_json::to_string(&expand_dl_body("all@x.ru")).unwrap(),
            r#"{"__type":"ExpandDLRequest:#Exchange","Mailbox":{"__type":"EmailAddress:#Exchange","EmailAddress":"all@x.ru","RoutingType":"SMTP"}}"#
        );
    }

    #[test]
    fn parses_find_people_forms() {
        // Форма Exchange 2019: ResultSet в Body без ResponseMessages; значения строками и атрибутированными массивами.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseClass":"Success","ResponseCode":"NoError","ResultSet":[
            {"__type":"Persona:#Exchange","PersonaType":"Person","DisplayName":"Петров Иван","EmailAddress":{"Name":"Петров Иван","EmailAddress":"Ivan.Petrov@x.ru","RoutingType":"SMTP","MailboxType":"Mailbox"},
             "Title":"Инженер","Department":"ИТ","CompanyName":"ООО Х","OfficeLocations":[{"Value":"к. 101","Attributions":["a"]}],
             "BusinessPhoneNumbers":[{"Value":{"Number":"+7 495 000","Type":"Business"},"Attributions":["a"]}],"MobilePhones":[{"Value":{"Number":"+7 900 000","Type":"Mobile"}}],"Alias":"ipetrov"},
            {"PersonaTypeString":"DistributionList","DisplayName":"Все сотрудники","EmailAddress":{"EmailAddress":"all@x.ru","RoutingType":"SMTP","MailboxType":"PublicDL"}},
            {"PersonaType":"Room","DisplayName":"Переговорная 1","EmailAddress":{"EmailAddress":"room1@x.ru","MailboxType":"Mailbox"}},
            {"PersonaType":"Person","DisplayName":"Старый адрес","EmailAddress":{"EmailAddress":"/o=Org/cn=old","RoutingType":"EX"},"EmailAddresses":[{"Name":"x","EmailAddress":"old@x.ru","RoutingType":"SMTP"}],"PhoneNumber":{"Number":"+7 901","Type":"Mobile"}},
            {"PersonaType":"Person","EmailAddress":{"EmailAddress":"/o=Org/cn=noname","RoutingType":"EX"}},
            {"PersonaType":"Person"}
        ],"TotalNumberOfPeopleInView":6}}"#).unwrap();
        assert!(body_error(&v).is_none());
        let p = parse_find_people(&v);
        assert_eq!(p.len(), 5, "запись без имени и адреса пропущена: {p:?}");
        assert_eq!(p[0].display_name.as_deref(), Some("Петров Иван"));
        assert_eq!(p[0].email.as_deref(), Some("Ivan.Petrov@x.ru"));
        assert_eq!(p[0].kind, PersonKind::Mailbox);
        assert_eq!((p[0].title.as_deref(), p[0].department.as_deref(), p[0].company.as_deref(), p[0].office.as_deref()), (Some("Инженер"), Some("ИТ"), Some("ООО Х"), Some("к. 101")));
        assert_eq!(p[0].business_phone.as_deref(), Some("+7 495 000"));
        assert_eq!(p[0].mobile_phone.as_deref(), Some("+7 900 000"));
        assert_eq!(p[0].alias.as_deref(), Some("ipetrov"));
        assert_eq!(p[1].kind, PersonKind::DistributionList);
        assert_eq!(p[2].kind, PersonKind::Room, "PersonaType главнее MailboxType");
        assert_eq!(p[3].email.as_deref(), Some("old@x.ru"), "адрес EX заменён SMTP из EmailAddresses");
        assert_eq!(p[3].mobile_phone.as_deref(), Some("+7 901"));
        assert_eq!(p[3].business_phone, None, "мобильный не считается рабочим");
        assert_eq!(p[4].email.as_deref(), Some("/o=Org/cn=noname"), "без SMTP остаётся адрес EX");
        assert_eq!(p[4].email_key(), None);
        // Дополнительные свойства PERSONA_EXTRA_FIELDS в форме Exchange 2019: Title строкой,
        // *Array — атрибутированные значения, телефоны с NormalizedNumber.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseClass":"Success","ResponseCode":"NoError","ResultSet":[
            {"PersonaId":{"Id":"P1"},"PersonaTypeString":"Person","DisplayName":"Сидоров Пётр","EmailAddress":{"Name":"Сидоров Пётр","EmailAddress":"p@x.ru","RoutingType":"SMTP","MailboxType":"Mailbox"},
             "CompanyName":"ООО Х","WorkCity":"Город","Title":"Начальник","OfficeLocationsArray":[{"Value":"к. 5","Attributions":["0"]}],
             "BusinessPhoneNumbersArray":[{"Value":{"Number":"1234","Type":"Business","NormalizedNumber":"1234"},"Attributions":["0"]}],
             "MobilePhonesArray":[{"Value":{"Number":"+7 999","Type":"Mobile","NormalizedNumber":"7999"},"Attributions":["0"]}]}
        ]}}"#).unwrap();
        let p = parse_find_people(&v);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].title.as_deref(), p[0].office.as_deref(), p[0].company.as_deref()), (Some("Начальник"), Some("к. 5"), Some("ООО Х")));
        assert_eq!((p[0].business_phone.as_deref(), p[0].mobile_phone.as_deref()), (Some("1234"), Some("+7 999")));
        assert_eq!((p[0].kind, p[0].department.as_deref()), (PersonKind::Mailbox, None), "WorkCity — не офис и не отдел");
        // Та же выдача в первом сообщении ResponseMessages.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseMessages":{"Items":[{"ResponseClass":"Success","ResponseCode":"NoError","People":[{"DisplayName":"А","EmailAddress":{"EmailAddress":"a@x.ru"}}]}]}}}"#).unwrap();
        assert_eq!(parse_find_people(&v).len(), 1);
        // Отказ уровня Body.
        let v: Value = serde_json::from_str(r#"{"Body":{"ResponseClass":"Error","ResponseCode":"ErrorInvalidRequest","MessageText":"bad"}}"#).unwrap();
        assert_eq!(body_error(&v), Some(("ErrorInvalidRequest".into(), "bad".into())));
        assert!(parse_find_people(&v).is_empty());
        assert!(body_error(&json!({"Body": {"ResponseMessages": {}}})).is_none(), "без ResponseClass в Body ошибки уровня Body нет");
    }

    #[test]
    fn parses_resolve_names_forms() {
        let msg: Value = serde_json::from_str(r#"{"__type":"ResolveNamesResponseMessage:#Exchange","ResponseClass":"Warning","ResponseCode":"ErrorNameResolutionMultipleResults","MessageText":"Multiple results were found.",
          "ResolutionSet":{"TotalItemsInView":2,"IncludesLastItemInRange":true,"Resolutions":[
            {"Mailbox":{"Name":"Петров Иван","EmailAddress":"ivan@x.ru","RoutingType":"SMTP","MailboxType":"Mailbox"},
             "Contact":{"DisplayName":"Петров Иван Сергеевич","JobTitle":"Инженер","Department":"ИТ","CompanyName":"ООО Х","OfficeLocation":"к. 101",
                        "Manager":"Сидоров Пётр","ManagerMailbox":{"Mailbox":{"Name":"Сидоров Пётр","EmailAddress":"petr@x.ru","RoutingType":"SMTP","MailboxType":"Mailbox"}},
                        "DirectReports":[{"Name":"Зайцев","EmailAddress":"z@x.ru","RoutingType":"SMTP","MailboxType":"Mailbox"}],
                        "PhysicalAddresses":[{"Key":"Home","City":"Дома"},{"Key":"Business","Street":"Ленина, 1","City":"Москва","PostalCode":"101000","CountryOrRegion":"Россия"}],"Notes":"",
                        "PhoneNumbers":[{"Key":"AssistantPhone","PhoneNumber":""},{"Key":"BusinessPhone","PhoneNumber":"+7 495 000"},{"Key":"HomePhone","PhoneNumber":"дом"},{"Key":"MobilePhone","PhoneNumber":"+7 900 000"}],
                        "EmailAddresses":[{"Key":"EmailAddress1","EmailAddress":"SMTP:ivan@x.ru"}],"ContactSource":"ActiveDirectory"}},
            {"Mailbox":{"Name":"Список","EmailAddress":"/o=Org/cn=list","RoutingType":"EX","MailboxType":"PublicDL"},
             "Contact":{"EmailAddresses":{"EmailAddress1":"smtp:list@x.ru"},"PhoneNumbers":{"BusinessPhone":"+7 495 111"}}}
          ]}}"#).unwrap();
        let p = parse_resolve_names(&msg).unwrap();
        assert_eq!(p.len(), 2, "несколько совпадений — успех со списком");
        assert_eq!(p[0].display_name.as_deref(), Some("Петров Иван Сергеевич"), "имя контакта главнее имени ящика");
        assert_eq!(p[0].email.as_deref(), Some("ivan@x.ru"));
        assert_eq!(p[0].kind, PersonKind::Mailbox);
        assert_eq!((p[0].title.as_deref(), p[0].department.as_deref(), p[0].company.as_deref(), p[0].office.as_deref()), (Some("Инженер"), Some("ИТ"), Some("ООО Х"), Some("к. 101")));
        assert_eq!(p[0].business_phone.as_deref(), Some("+7 495 000"));
        assert_eq!(p[0].mobile_phone.as_deref(), Some("+7 900 000"));
        let mgr = p[0].manager.as_ref().expect("руководитель");
        assert_eq!((mgr.name.as_deref(), mgr.email.as_deref()), (Some("Сидоров Пётр"), Some("petr@x.ru")), "ManagerMailbox главнее строки Manager");
        assert_eq!(p[0].direct_reports.as_ref().map(|d| d.len()), Some(1));
        assert_eq!(p[0].direct_reports.as_ref().unwrap()[0].email.as_deref(), Some("z@x.ru"));
        assert_eq!(p[0].address.as_deref(), Some("Ленина, 1, 101000 Москва, Россия"), "рабочий адрес, не домашний");
        assert_eq!(p[0].notes, None, "пустые заметки — None");
        assert_eq!(p[1].direct_reports, None, "без Contact.DirectReports — неизвестно");
        assert_eq!(p[1].email.as_deref(), Some("list@x.ru"), "EX заменён SMTP из словаря без префикса smtp:");
        assert_eq!(p[1].kind, PersonKind::DistributionList);
        assert_eq!(p[1].display_name.as_deref(), Some("Список"));
        assert_eq!(p[1].business_phone.as_deref(), Some("+7 495 111"), "словарь телефонов объектом");
        // Одно совпадение: Success; ResolutionSet массивом и Resolution вместо Resolutions.
        let msg: Value = serde_json::from_str(r#"{"ResponseClass":"Success","ResponseCode":"NoError","ResolutionSet":{"Resolution":[{"Mailbox":{"Name":"А","EmailAddress":"a@x.ru","MailboxType":"Contact"}}]}}"#).unwrap();
        let p = parse_resolve_names(&msg).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, PersonKind::Contact);
        let msg: Value = serde_json::from_str(r#"{"ResponseClass":"Success","ResponseCode":"NoError","ResolutionSet":[{"Mailbox":{"EmailAddress":"b@x.ru"}}]}"#).unwrap();
        assert_eq!(parse_resolve_names(&msg).unwrap()[0].kind, PersonKind::Other);
        // Адрес EX: из словаря EmailAddresses берётся SMTP, а не SIP или X500.
        let msg: Value = serde_json::from_str(r#"{"ResponseClass":"Success","ResponseCode":"NoError","ResolutionSet":{"Resolutions":[{"Mailbox":{"Name":"В","EmailAddress":"/o=Org/cn=v","RoutingType":"EX","MailboxType":"Mailbox"},
            "Contact":{"EmailAddresses":[{"Key":"EmailAddress1","EmailAddress":"SIP:v@x.ru"},{"Key":"EmailAddress2","EmailAddress":"X500:/o=Org/cn=v"},{"Key":"EmailAddress3","EmailAddress":"SMTP:v.smtp@x.ru"}]}}]}}"#).unwrap();
        assert_eq!(parse_resolve_names(&msg).unwrap()[0].email.as_deref(), Some("v.smtp@x.ru"));
        // Ничего не найдено — пусто; другой отказ — ошибка с кодом.
        let msg: Value = serde_json::from_str(r#"{"ResponseClass":"Error","ResponseCode":"ErrorNameResolutionNoResults","MessageText":"No results were found."}"#).unwrap();
        assert_eq!(parse_resolve_names(&msg), Ok(vec![]));
        let msg: Value = serde_json::from_str(r#"{"ResponseClass":"Error","ResponseCode":"ErrorAccessDenied","MessageText":"нет"}"#).unwrap();
        assert_eq!(parse_resolve_names(&msg), Err(("ErrorAccessDenied".into(), "нет".into())));
    }

    #[test]
    fn parses_expand_dl_forms() {
        let msg: Value = serde_json::from_str(r#"{"ResponseClass":"Success","ResponseCode":"NoError","DLExpansion":{"TotalItemsInView":3,"IncludesLastItemInRange":true,"Mailbox":[
            {"Name":"Петров Иван","EmailAddress":"ivan@x.ru","RoutingType":"SMTP","MailboxType":"Mailbox"},
            {"Name":"Вложенный список","EmailAddress":"sub@x.ru","RoutingType":"SMTP","MailboxType":"PublicDL"},
            {"Name":"Внешний","EmailAddress":"ext@y.com","RoutingType":"SMTP","MailboxType":"Contact"},
            {"RoutingType":"SMTP"}
        ]}}"#).unwrap();
        let p = parse_expand_dl(&msg);
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].kind, p[1].kind, p[2].kind), (PersonKind::Mailbox, PersonKind::DistributionList, PersonKind::Contact));
        assert_eq!(p[1].email.as_deref(), Some("sub@x.ru"));
        let msg: Value = serde_json::from_str(r#"{"DLExpansion":{"Mailboxes":[{"Mailbox":{"Name":"А","EmailAddress":"a@x.ru"}}]}}"#).unwrap();
        assert_eq!(parse_expand_dl(&msg).len(), 1, "терпимо к Mailboxes и вложенному Mailbox");
        assert!(parse_expand_dl(&json!({"ResponseClass": "Success"})).is_empty());
        let keys = key_union(&[json!({"A": 1, "B": 2}), json!({"B": 3}), json!("x")]);
        assert_eq!(keys, vec![("A".to_string(), 1), ("B".to_string(), 2)]);
    }

    /// § 5m: запрос последних писем и разбор ответа в форме, снятой с сервера 25.10.2026.
    #[test]
    fn recent_items_request_and_response() {
        let folder = FolderRef::Id(FolderId { id: "F1".into(), change_key: None });
        let body = find_recent_items_body(&folder, 50, 100);
        assert_eq!(body["__type"], "FindItemRequest:#Exchange");
        assert_eq!(body["Paging"], json!({ "__type": "IndexedPageView:#Exchange", "BasePoint": "Beginning", "Offset": 100, "MaxEntriesReturned": 50 }));
        assert_eq!(body["SortOrder"][0]["Path"]["FieldURI"], RECENT_SORT_FIELD);
        assert_eq!(body["ItemShape"]["AdditionalProperties"].as_array().map(Vec::len), Some(META_FIELDS.len() + verb_extended_props().len()), "свойства списка и два расширенных свойства последнего действия");
        assert_eq!(body.as_object().and_then(|o| o.keys().next()).map(String::as_str), Some("__type"));
        let msg = json!({
            "__type": "FindItemResponseMessage:#Exchange", "ResponseCode": "NoError", "ResponseClass": "Success", "HighlightTerms": null,
            "RootFolder": { "IncludesLastItemInRange": false, "IndexedPagingOffset": 2, "TotalItemsInView": 77267, "Groups": null, "Items": [
                { "__type": "Message:#Exchange", "ItemId": { "Id": "I1", "ChangeKey": "C1" }, "DateTimeReceived": "2026-10-25T10:00:00Z", "IsRead": false, "Flag": { "FlagStatus": "Flagged" }, "InternetMessageId": "<1@x>" },
                { "__type": "Message:#Exchange", "ItemId": { "Id": "I2", "ChangeKey": "C2" }, "DateTimeReceived": "2026-10-25T09:00:00Z", "IsRead": true },
                { "__type": "Message:#Exchange" }
            ] }
        });
        let found = parse_find_items(&msg, 3);
        assert_eq!(found.items.iter().map(|m| m.item_id.id.as_str()).collect::<Vec<_>>(), vec!["I1", "I2"]);
        assert_eq!((found.raw_count, found.includes_last, found.total_in_view), (3, false, Some(77267)));
        assert_eq!(found.items[0].flag, FlagStatus::Flagged);
        assert!(!found.items[0].is_read && found.items[1].is_read);
        // Без `IncludesLastItemInRange` конец выдачи — по короткой странице.
        assert!(parse_find_items(&json!({ "RootFolder": { "Items": [] } }), 10).includes_last);
    }
}
