//! Настоящий `OwaTransport` по HTTP против фейкового сервера OWA (`common`): вход и канарейка,
//! синхронизация через трейт, запись, отправка, повторный вход, лимит сессий, `logoff`, полная
//! синхронизация движком во временную базу. Без сети: сервер слушает `127.0.0.1:0`.

mod common;

use aiowa_core::{ComposeKind, DistinguishedFolder, FlagStatus, FolderChange, FolderId, FolderRef, Importance, ItemChange, ItemId, MessageMeta, OutgoingAttachment, OutgoingMessage, Participant, RecipientKind, SyncState};
use aiowa_store::Store;
use aiowa_sync::{Engine, FolderSelection, SyncOptions};
use aiowa_transport::{Credentials, DeleteMode, ItemFetchShape, ItemResult, MailTransport, TransportError, WriteResult};
use aiowa_transport_owa::{protocol, OwaConfig, OwaTransport};
use common::{Addr, Expiry, FakeAttachment, FakeOwa, Gather, Hold, Mailbox, BUILD, DELETED, DRAFTS, INBOX, MAILBOX_GUID, OWNER, OWNER_NAME, PASSWORD, PROJECTS, ROOT, SENT, USER};
use secrecy::SecretString;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

const SHIPMENT: &str = "[AIOWA test] Отгрузка запчастей на Колыму";
const BUDGET: &str = "[AIOWA test] Совещание по бюджету";
const REPORT: &str = "[AIOWA test] Отчёт за сентябрь";

fn creds(server: &FakeOwa, password: &str) -> Credentials {
    Credentials { host: server.host(), user: USER.into(), password: SecretString::from(password.to_string()) }
}

fn config(server: &FakeOwa) -> OwaConfig {
    OwaConfig { base_url: Some(server.base_url()), timeout: Duration::from_secs(30), ..OwaConfig::default() }
}

async fn connect(server: &FakeOwa) -> OwaTransport {
    OwaTransport::connect(creds(server, PASSWORD), config(server)).await.expect("вход на фейковый сервер")
}

fn folder(id: &str) -> FolderRef {
    FolderRef::Id(FolderId { id: id.into(), change_key: None })
}

fn inbox() -> FolderRef {
    FolderRef::Distinguished(DistinguishedFolder::Inbox)
}

fn person(name: &str, email: &str) -> Participant {
    Participant { name: Some(name.into()), email: Some(email.into()), routing_type: None }
}

/// Все письма папки с начала: пачки `SyncFolderItems` до признака конца.
async fn sync_folder(t: &OwaTransport, id: &str) -> (Vec<MessageMeta>, SyncState) {
    let mut state: Option<SyncState> = None;
    let mut out = Vec::new();
    loop {
        let page = t.sync_folder_items(&folder(id), state.as_ref(), 512, true).await.expect("SyncFolderItems");
        for ch in page.changes {
            match ch {
                ItemChange::Create(m) => out.push(m),
                other => panic!("при первой синхронизации только Create: {other:?}"),
            }
        }
        state = Some(page.state);
        if page.includes_last {
            return (out, state.expect("состояние"));
        }
    }
}

fn by_subject<'a>(metas: &'a [MessageMeta], subject: &str) -> &'a MessageMeta {
    metas.iter().find(|m| m.subject.as_deref() == Some(subject)).unwrap_or_else(|| panic!("нет письма «{subject}»"))
}

/// `Body` последнего вызова операции на сервере.
fn last_body(server: &FakeOwa, op: &str) -> Value {
    server.state().calls_of(op).last().unwrap_or_else(|| panic!("не было {op}")).body.clone()
}

/// По каждому `GetItem` на сервере, в порядке прихода: просил ли клиент `message:ReplyTo`.
fn reply_to_requested(server: &FakeOwa) -> Vec<bool> {
    server.state().calls_of("GetItem").iter().map(|c| c.body["ItemShape"].get("AdditionalProperties").is_some_and(|p| p.to_string().contains("message:ReplyTo"))).collect()
}

/// Идентификатор файлового (не инлайнового) вложения письма «Отгрузка».
fn shipment_file_id(server: &FakeOwa) -> String {
    let st = server.state();
    let it = st.item_by_subject(SHIPMENT).unwrap();
    it.attachment_id(&it.attachments[0])
}

/// Вход формой (§ 1): поля формы, cookies сессии, канарейка в каждом вызове; `__type` первым
/// ключом проверяется сервером; отказ CSRF ведёт к одному повторному входу с закрытием прежней сессии.
#[tokio::test]
async fn login_cookies_and_canary() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    {
        let st = server.state();
        assert_eq!((st.logins_ok(), st.active_sessions()), (1, 1));
        let login = &st.logins[0];
        assert_eq!(login.username, USER);
        assert_eq!((login.flags.as_str(), login.forcedownlevel.as_str(), login.is_utf8.as_str()), ("4", "0", "1"), "поля формы § 1");
        assert_eq!(login.destination, format!("{}owa", server.base_url()));
        assert!(login.content_type.starts_with("application/x-www-form-urlencoded"), "{}", login.content_type);
    }
    assert_eq!(t.server_info().build.as_deref(), Some(BUILD), "сборка из x-owa-version");

    let counts = t.get_folder_counts(&[inbox()]).await.unwrap();
    assert_eq!(counts.len(), 1);
    assert_eq!((counts[0].id.id.as_str(), counts[0].total, counts[0].unread), (INBOX, 3, 2));
    {
        let st = server.state();
        let call = st.calls_of("GetFolder")[0];
        assert_eq!((call.version.as_str(), call.time_zone.as_str()), ("Exchange2013", "UTC"));
        assert_eq!(call.client_build.as_deref(), Some(BUILD), "X-OWA-ClientBuildVersion из ответа сервера");
        assert_eq!(call.body["FolderIds"][0], json!({ "__type": "DistinguishedFolderId:#Exchange", "Id": "inbox" }));
        assert!(st.rejected.is_empty(), "{:?}", st.rejected);
    }

    // `__type` не первым ключом — HTTP 500 «абстрактный класс» (§ 5b), повторного входа нет.
    let r = t.call("GetFolder", protocol::V_2013, json!({ "FolderIds": [], "__type": "GetFolderRequest:#Exchange" })).await;
    assert!(matches!(&r, Err(TransportError::Protocol { status: 500, raw, .. }) if raw.contains("абстрактный класс")), "{r:?}");
    assert_eq!(server.state().logins_ok(), 1);

    // Сервер сменил канарейку: отказ CSRF (редирект на errorFE.aspx), один повторный вход.
    server.state().faults.rotate_canary_next = true;
    let counts = t.get_folder_counts(&[inbox()]).await.unwrap();
    assert_eq!(counts[0].total, 3);
    {
        let st = server.state();
        assert_eq!(st.rejected.last().map(String::as_str), Some("csrf"));
        assert_eq!(st.logins_ok(), 2, "ровно один повторный вход");
        let calls = st.calls_of("GetFolder");
        assert_ne!(calls[0].session, calls.last().unwrap().session, "вызов после входа идёт в новой сессии");
        // § 5c: перед повторным входом прежняя (живая) сессия закрыта, иначе копились бы до лимита.
        assert_eq!((st.active_sessions(), st.logoffs), (1, 1));
    }
    t.logoff().await.unwrap();
    assert_eq!(server.state().active_sessions(), 0);
}

#[tokio::test]
async fn wrong_password_is_auth_failed() {
    let server = FakeOwa::start().await;
    match OwaTransport::connect(creds(&server, "не тот пароль"), config(&server)).await {
        Err(TransportError::AuthFailed(m)) => assert!(m.contains("reason=2"), "{m}"),
        Err(e) => panic!("ожидался AuthFailed: {e:?}"),
        Ok(_) => panic!("вход с неверным паролем прошёл"),
    }
    let st = server.state();
    assert_eq!((st.logins.len(), st.logins_ok(), st.active_sessions()), (1, 0, 0));
    assert!(st.calls.is_empty());
}

/// Чтение через трейт: иерархия, пачки `SyncFolderItems` с состоянием, тела `GetItem` в трёх
/// формах, вложение GET-ом и испорченное состояние синхронизации.
#[tokio::test]
async fn folder_sync_through_trait() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;

    let h = t.sync_folder_hierarchy(None).await.unwrap();
    assert!(h.includes_last);
    let folders: Vec<_> = h
        .changes
        .iter()
        .map(|c| match c {
            FolderChange::Create(f) => f.clone(),
            other => panic!("ожидалось Create: {other:?}"),
        })
        .collect();
    assert_eq!(folders.len(), 5, "корень в изменения не входит: {folders:?}");
    let inbox_folder = folders.iter().find(|f| f.distinguished.as_deref() == Some("inbox")).unwrap();
    assert_eq!((inbox_folder.display_name.as_str(), inbox_folder.total_count, inbox_folder.unread_count), ("Входящие", 3, 2));
    assert_eq!(inbox_folder.parent_id.as_ref().map(|p| p.id.as_str()), Some(ROOT));
    assert_eq!(inbox_folder.child_folder_count, 1);
    let projects = folders.iter().find(|f| f.display_name == "Проекты").unwrap();
    assert_eq!((projects.distinguished.as_deref(), projects.parent_id.as_ref().map(|p| p.id.as_str())), (None, Some(INBOX)));
    assert!(t.sync_folder_hierarchy(Some(&h.state)).await.unwrap().changes.is_empty(), "без изменений — пусто");
    let body = last_body(&server, "SyncFolderHierarchy");
    assert_eq!(body["FolderShape"]["BaseShape"], "AllProperties");
    assert_eq!(body["SyncFolderId"]["BaseFolderId"]["Id"], "msgfolderroot");

    // Пачки по два: первая полная (конец не достигнут), вторая короче — конец папки (§ 5b').
    let p1 = t.sync_folder_items(&folder(INBOX), None, 2, true).await.unwrap();
    assert_eq!((p1.changes.len(), p1.includes_last), (2, false));
    let p2 = t.sync_folder_items(&folder(INBOX), Some(&p1.state), 2, true).await.unwrap();
    assert_eq!((p2.changes.len(), p2.includes_last), (1, true));
    let p3 = t.sync_folder_items(&folder(INBOX), Some(&p2.state), 2, true).await.unwrap();
    assert!(p3.changes.is_empty() && p3.includes_last);
    let metas: Vec<MessageMeta> = p1
        .changes
        .into_iter()
        .chain(p2.changes)
        .map(|c| match c {
            ItemChange::Create(m) => m,
            other => panic!("ожидалось Create: {other:?}"),
        })
        .collect();
    let shipment = by_subject(&metas, SHIPMENT);
    assert_eq!(shipment.from.as_ref().and_then(|p| p.email.as_deref()), Some("vasya@aiowa.test"));
    assert_eq!(shipment.received_at.map(|t| t.to_string()).as_deref(), Some("2026-10-20T08:15:00Z"));
    assert!(!shipment.is_read && shipment.has_attachments);
    assert_eq!(shipment.parent_folder_id.as_ref().map(|f| f.id.as_str()), Some(INBOX));
    assert_eq!(shipment.item_id.change_key.as_deref().map(str::len), Some(40));
    assert!(by_subject(&metas, BUDGET).is_read);
    {
        let st = server.state();
        let calls = st.calls_of("SyncFolderItems");
        assert_eq!(calls[0].version, "Exchange2013");
        assert_eq!(calls[0].body["ItemShape"]["AdditionalProperties"].as_array().map(Vec::len), Some(protocol::META_FIELDS.len() + 2), "свойства списка и два расширенных свойства последнего действия");
        assert_eq!(calls[0].body["SyncFolderId"]["BaseFolderId"], json!({ "__type": "FolderId:#Exchange", "Id": INBOX }));
        assert_eq!((calls[0].body["MaxChangesReturned"].as_u64(), calls[0].body.get("SyncState").is_none()), (Some(2), true));
        assert!(calls[1].body["SyncState"].is_string(), "следующая пачка — с состоянием из ответа");
    }

    // Тела: ItemPart с Reply-To поверх формы (§ 5j); исчезнувшее письмо — отказ по позиции.
    let mut ids: Vec<ItemId> = metas.iter().map(|m| m.item_id.clone()).collect();
    ids.push(ItemId { id: "AAMkItem-9999".into(), change_key: None });
    let results = t.get_items(&ids, ItemFetchShape::WithUniqueBody).await.unwrap();
    assert_eq!(results.len(), 4);
    assert!(results[3].is_not_found(), "{:?}", results[3]);
    let full = results
        .iter()
        .find_map(|r| match r {
            ItemResult::Ok(m) if m.meta.item_id.id == shipment.item_id.id => Some(m.clone()),
            _ => None,
        })
        .unwrap();
    let body = full.body.as_ref().unwrap();
    assert!(body.unique_body.as_deref().unwrap().contains("на Колыму") && !body.is_truncated);
    assert_eq!(full.to, vec![Participant { name: Some(common::OWNER_NAME.into()), email: Some(OWNER.into()), routing_type: Some("SMTP".into()) }]);
    assert_eq!(full.cc.iter().map(|p| p.email.as_deref().unwrap()).collect::<Vec<_>>(), vec!["petr@aiowa.test"]);
    assert_eq!(full.reply_to.iter().map(|p| p.email.as_deref().unwrap()).collect::<Vec<_>>(), vec!["supply@aiowa.test"]);
    assert_eq!(full.attachments.len(), 2);
    let inline = full.attachments.iter().find(|a| a.is_inline).unwrap();
    assert_eq!(inline.content_id.as_deref(), Some("logo@aiowa.test"));
    let file = full.attachments.iter().find(|a| !a.is_inline).unwrap().clone();
    assert_eq!((file.name.as_deref(), file.content_type.as_deref()), (Some("спецификация.txt"), Some("text/plain")));
    let body = last_body(&server, "GetItem");
    assert_eq!(body["ShapeName"], "ItemPart");
    assert_eq!(body["ItemShape"]["AdditionalProperties"], json!([{ "__type": "PropertyUri:#Exchange", "FieldURI": "message:ReplyTo" }]));
    assert_eq!(server.state().calls_of("GetItem")[0].version, "V2017_08_18");

    // Форма списка без тел и без Reply-To; полное тело — ItemNormalizedBody.
    let meta_only = t.get_items(std::slice::from_ref(&shipment.item_id), ItemFetchShape::Meta).await.unwrap();
    assert!(matches!(&meta_only[0], ItemResult::Ok(m) if m.body.is_none() && m.to.is_empty() && m.meta.subject.as_deref() == Some(SHIPMENT)));
    let body = last_body(&server, "GetItem");
    assert_eq!(body["ShapeName"].as_str(), Some("MailListItem"));
    assert_eq!(body["ItemShape"]["AdditionalProperties"], json!(protocol::verb_extended_props()), "форма списка: без Reply-To, с последним действием");
    let normalized = t.get_items(std::slice::from_ref(&shipment.item_id), ItemFetchShape::WithFullBody).await.unwrap();
    assert!(matches!(&normalized[0], ItemResult::Ok(m) if m.body.as_ref().and_then(|b| b.full_body.as_deref()).is_some_and(|b| b.contains("Колыму"))));
    assert_eq!(last_body(&server, "GetItem")["ShapeName"], "ItemNormalizedBody");

    // Вложение: GET service.svc/s/GetFileAttachment, имя из Content-Disposition в процентах (§ 5).
    let content = t.get_attachment(&file.id, 1 << 20).await.unwrap();
    assert_eq!(content.bytes, "Поставка шлёпанцев, 40 пар".as_bytes());
    assert_eq!((content.content_type.as_deref(), content.file_name.as_deref()), (Some("text/plain"), Some("спецификация.txt")));
    let r = t.get_attachment(&file.id, 5).await;
    assert!(matches!(r, Err(TransportError::TooLarge { limit: 5 })), "{r:?}");
    assert_eq!(server.state().downloads, vec![file.id.clone(), file.id.clone()]);
    assert!(server.state().calls_of("GetAttachment").is_empty(), "запасной путь не понадобился");

    // Испорченное состояние: отказ сервера, по которому движок синхронизирует папку заново.
    let r = t.sync_folder_items(&folder(INBOX), Some(&SyncState("испорчено".into())), 2, true).await;
    assert!(r.as_ref().err().is_some_and(TransportError::invalidates_sync_state), "{r:?}");
    t.logoff().await.unwrap();
}

/// GET вложения отвергнут сервером: транспорт переходит на операцию `GetAttachment` (§ 5) и
/// запоминает это на сессию.
#[tokio::test]
async fn attachment_falls_back_to_get_attachment_operation() {
    let server = FakeOwa::start().await;
    server.state().faults.reject_file_get = true;
    let t = connect(&server).await;
    let id = shipment_file_id(&server);
    for round in 1..=2 {
        let c = t.get_attachment(&id, 1 << 20).await.unwrap();
        assert_eq!(c.bytes, "Поставка шлёпанцев, 40 пар".as_bytes());
        assert_eq!((c.content_type.as_deref(), c.file_name.as_deref()), (Some("text/plain"), Some("спецификация.txt")));
        assert_eq!(server.state().calls_of("GetAttachment").len(), round);
        // GET пробуется один раз за сессию: второе вложение — сразу операцией.
        assert_eq!(server.state().rejected, vec!["file_get".to_string()], "раунд {round}");
    }
    let body = last_body(&server, "GetAttachment");
    assert_eq!(body["AttachmentIds"], json!([{ "__type": "AttachmentId:#Exchange", "Id": id }]));
    assert!(server.state().downloads.is_empty());
    t.logoff().await.unwrap();
}

/// HTML-файл во вложении (`text/html` с `Content-Disposition: attachment`) — файл, а не страница
/// ошибки: скачивается GET-ом как есть, запасной путь не нужен.
#[tokio::test]
async fn html_attachment_downloads_by_get() {
    let page = "<html><body>[AIOWA test] Прайс-лист на шлёпанцы</body></html>";
    let mut mailbox = Mailbox::seeded();
    let it = mailbox.items.iter_mut().find(|i| i.subject == SHIPMENT).unwrap();
    it.attachments.push(FakeAttachment { seq: 3, name: "прайс.html".into(), content_type: "text/html; charset=utf-8".into(), bytes: page.as_bytes().to_vec(), is_inline: false, content_id: None });
    let id = it.attachment_id(it.attachments.last().unwrap());
    let server = FakeOwa::start_with(mailbox).await;
    let t = connect(&server).await;
    let c = t.get_attachment(&id, 1 << 20).await.unwrap();
    assert_eq!(c.bytes, page.as_bytes());
    assert_eq!((c.content_type.as_deref(), c.file_name.as_deref()), (Some("text/html"), Some("прайс.html")));
    {
        let st = server.state();
        assert_eq!(st.downloads, vec![id.clone()]);
        assert!(st.calls_of("GetAttachment").is_empty(), "запасной путь не понадобился");
    }
    t.logoff().await.unwrap();
}

/// Ответ без `Content-Length` (`Transfer-Encoding: chunked`): файл приходит целиком, а предел
/// размера срабатывает по счётчику прочитанных байт — заголовка, по которому отказать заранее, нет.
#[tokio::test]
async fn chunked_attachment_is_limited_by_bytes_read() {
    let server = FakeOwa::start().await;
    server.state().faults.chunked_file_get = true;
    let t = connect(&server).await;
    let id = shipment_file_id(&server);
    let c = t.get_attachment(&id, 1 << 20).await.unwrap();
    assert_eq!(c.bytes, "Поставка шлёпанцев, 40 пар".as_bytes());
    assert_eq!((c.content_type.as_deref(), c.file_name.as_deref()), (Some("text/plain"), Some("спецификация.txt")));
    let r = t.get_attachment(&id, 5).await;
    assert!(matches!(r, Err(TransportError::TooLarge { limit: 5 })), "{r:?}");
    {
        let st = server.state();
        assert_eq!(st.downloads, vec![id.clone(), id.clone()], "оба раза GET");
        assert!(st.calls_of("GetAttachment").is_empty(), "запасной путь не понадобился");
    }
    t.logoff().await.unwrap();
}

/// Прочитано, флаг, перенос, удаление: состояние ящика на сервере, формы запросов (§ 5e) и
/// следующая инкрементальная синхронизация.
#[tokio::test]
async fn write_operations_change_server_state() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    let (metas, state) = sync_folder(&t, INBOX).await;
    let shipment = by_subject(&metas, SHIPMENT).item_id.clone();
    let budget = by_subject(&metas, BUDGET).item_id.clone();
    let report = by_subject(&metas, REPORT).item_id.clone();

    // Прочитано: тот же Id, ключ в ответе усечён до 8 символов, как у настоящего сервера.
    let r = t.set_read(std::slice::from_ref(&shipment), true).await.unwrap();
    match &r[..] {
        [WriteResult::Ok { id }] => assert_eq!((id.id.as_str(), id.change_key.as_deref().map(str::len)), (shipment.id.as_str(), Some(8))),
        other => panic!("{other:?}"),
    }
    assert!(server.state().item(&shipment.id).unwrap().is_read);
    let body = last_body(&server, "UpdateItem");
    assert_eq!((body["ConflictResolution"].as_str(), body["MessageDisposition"].as_str(), body["SuppressReadReceipts"].as_bool()), (Some("AlwaysOverwrite"), Some("SaveOnly"), Some(true)));
    let update = &body["ItemChanges"][0]["Updates"][0];
    assert_eq!(update["__type"], "SetItemField:#Exchange");
    assert_eq!(update["Path"], json!({ "__type": "PropertyUri:#Exchange", "FieldURI": "message:IsRead" }));
    assert_eq!(update["Item"], json!({ "__type": "Message:#Exchange", "IsRead": true }));
    assert_eq!(server.state().calls_of("UpdateItem")[0].version, "V2017_08_18");
    let inc = t.sync_folder_items(&folder(INBOX), Some(&state), 512, true).await.unwrap();
    assert!(matches!(&inc.changes[..], [ItemChange::ReadFlag { item_id, is_read: true }] if item_id.id == shipment.id), "{:?}", inc.changes);

    // Флаг: поле item:Flag, в синхронизации — Update с новым флагом.
    let r = t.set_flag(std::slice::from_ref(&budget), true).await.unwrap();
    assert!(r[0].is_ok(), "{r:?}");
    assert_eq!(server.state().item(&budget.id).unwrap().flag, "Flagged");
    let update = &last_body(&server, "UpdateItem")["ItemChanges"][0]["Updates"][0];
    assert_eq!(update["Path"]["FieldURI"], "item:Flag");
    assert_eq!(update["Item"], json!({ "__type": "Message:#Exchange", "Flag": { "__type": "FlagType:#Exchange", "FlagStatus": "Flagged" } }));
    let inc = t.sync_folder_items(&folder(INBOX), Some(&inc.state), 512, true).await.unwrap();
    assert!(matches!(&inc.changes[..], [ItemChange::Update(m)] if m.item_id.id == budget.id && m.flag == FlagStatus::Flagged), "{:?}", inc.changes);

    // Перенос в «Проекты»: новый идентификатор; несуществующее письмо пачки — отказ по позиции.
    let ghost = ItemId { id: "AAMkItem-9999".into(), change_key: None };
    let r = t.move_items(&[report.clone(), ghost], &folder(PROJECTS)).await.unwrap();
    let moved = match &r[..] {
        [WriteResult::Ok { id }, WriteResult::Failed { code, .. }] if code == "ErrorItemNotFound" => id.clone(),
        other => panic!("{other:?}"),
    };
    assert_ne!(moved.id, report.id);
    {
        let st = server.state();
        assert_eq!(st.item(&moved.id).unwrap().folder, PROJECTS);
        assert!(st.item(&report.id).is_none());
    }
    let body = last_body(&server, "MoveItem");
    assert_eq!(body["ToFolderId"], json!({ "__type": "TargetFolderId:#Exchange", "BaseFolderId": { "__type": "FolderId:#Exchange", "Id": PROJECTS } }));
    assert_eq!((body["ItemIds"].as_array().map(Vec::len), body["ReturnNewItemIds"].as_bool()), (Some(2), Some(true)));
    let inc = t.sync_folder_items(&folder(INBOX), Some(&inc.state), 512, true).await.unwrap();
    assert!(matches!(&inc.changes[..], [ItemChange::Delete(id)] if id.id == report.id), "{:?}", inc.changes);

    // В «Удалённые»: письмо переехало с новым идентификатором.
    let r = t.delete_items(std::slice::from_ref(&shipment), DeleteMode::ToDeletedItems).await.unwrap();
    let trashed = match &r[..] {
        [WriteResult::Ok { id }] => id.clone(),
        other => panic!("{other:?}"),
    };
    assert_ne!(trashed.id, shipment.id);
    assert_eq!(server.state().item(&trashed.id).unwrap().folder, DELETED);
    let body = last_body(&server, "DeleteItem");
    assert_eq!((body["DeleteType"].as_str(), body["ReturnMovedItemIds"].as_bool(), body["SuppressReadReceipts"].as_bool()), (Some("MoveToDeletedItems"), Some(true), Some(true)));

    // Навсегда: письма больше нет, идентификатор в результате исходный.
    let r = t.delete_items(std::slice::from_ref(&budget), DeleteMode::Hard).await.unwrap();
    assert!(matches!(&r[..], [WriteResult::Ok { id }] if id.id == budget.id), "{r:?}");
    assert_eq!(last_body(&server, "DeleteItem")["DeleteType"], "HardDelete");
    {
        let st = server.state();
        assert!(st.item(&budget.id).is_none());
        assert!(st.items_in(INBOX).is_empty());
    }
    t.logoff().await.unwrap();
}

/// Черновик → правка → два вложения → отправка (§ 5f): ключ черновика идёт по цепочке ответов
/// `CreateAttachment`, устаревший ключ сервер отвергает по письму.
#[tokio::test]
async fn send_message_with_attachments() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    let msg = OutgoingMessage {
        to: vec![person("Вася Пупкин", "vasya@aiowa.test"), Participant { name: None, email: Some(OWNER.into()), routing_type: None }],
        cc: vec![],
        bcc: vec![person("Анна Смирнова", "anna@aiowa.test")],
        subject: "[AIOWA test] Проверка отправки".into(),
        body_html: "<div>Проверка отправки</div>".into(),
        importance: Importance::Normal,
    };
    let draft = t.create_draft(&ComposeKind::New, &msg).await.unwrap();
    assert_eq!(draft.change_key.as_deref().map(str::len), Some(40));
    {
        let st = server.state();
        let it = st.item(&draft.id).unwrap();
        assert_eq!((it.folder.as_str(), it.is_draft, it.subject.as_str()), (DRAFTS, true, "[AIOWA test] Проверка отправки"));
    }
    let body = last_body(&server, "CreateItem");
    assert_eq!(body["MessageDisposition"], "SaveOnly");
    assert_eq!(body["SavedItemFolderId"]["BaseFolderId"], json!({ "__type": "DistinguishedFolderId:#Exchange", "Id": "drafts" }));
    let item = &body["Items"][0];
    assert_eq!(item["__type"], "Message:#Exchange");
    assert_eq!(item["ToRecipients"][0], json!({ "__type": "EmailAddress:#Exchange", "Name": "Вася Пупкин", "EmailAddress": "vasya@aiowa.test", "RoutingType": "SMTP" }));
    assert!(item.get("CcRecipients").is_none(), "пустой список у нового письма не передаётся");
    assert_eq!((item["Importance"].as_str(), item["IsRead"].as_bool()), (Some("Normal"), Some(true)));

    // Правка: тема и «Копия»; опустевшая «Скрытая копия» снимается DeleteItemField.
    let edited = OutgoingMessage { subject: "[AIOWA test] Проверка отправки (правка)".into(), cc: vec![person("Пётр Петров", "petr@aiowa.test")], bcc: vec![], ..msg.clone() };
    let draft = t.update_draft(&draft, &edited).await.unwrap();
    {
        let st = server.state();
        let it = st.item(&draft.id).unwrap();
        assert_eq!(it.subject, edited.subject);
        assert_eq!(it.cc, vec![Addr::new("Пётр Петров", "petr@aiowa.test")]);
        assert!(it.bcc.is_empty());
    }
    let updates = last_body(&server, "UpdateItem")["ItemChanges"][0]["Updates"].clone();
    assert!(updates.as_array().unwrap().iter().any(|u| u["__type"] == "DeleteItemField:#Exchange" && u["Path"]["FieldURI"] == "message:BccRecipients"), "{updates}");

    let files = vec![
        OutgoingAttachment { name: "отчёт.txt".into(), content_type: Some("text/plain".into()), bytes: "Итоги квартала".as_bytes().to_vec() },
        OutgoingAttachment { name: "data.bin".into(), content_type: None, bytes: vec![0, 1, 2, 3, 255] },
    ];
    let stale = draft.clone();
    let draft = t.add_attachments(&draft, &files).await.unwrap();
    assert_eq!(draft.id, stale.id);
    assert_ne!(draft.change_key, stale.change_key);
    {
        let st = server.state();
        let calls = st.calls_of("CreateAttachment");
        assert_eq!(calls.len(), 2, "по одному файлу на запрос");
        assert_ne!(calls[0].body["ParentItemId"]["ChangeKey"], calls[1].body["ParentItemId"]["ChangeKey"], "второй файл — с ключом из ответа на первый");
        assert_eq!(calls[1].body["Attachments"][0]["ContentType"], "application/octet-stream");
        let it = st.item(&draft.id).unwrap();
        assert_eq!(it.attachments.iter().map(|a| (a.name.as_str(), a.bytes.clone())).collect::<Vec<_>>(), files.iter().map(|f| (f.name.as_str(), f.bytes.clone())).collect::<Vec<_>>());
    }

    // Ключ до вложений устарел: отказ по письму, ничего не ушло.
    let r = t.send_draft(&stale).await;
    assert!(matches!(&r, Err(TransportError::Server { code, .. }) if code == "ErrorItemNotFound"), "{r:?}");
    assert!(server.state().sent.is_empty());

    t.send_draft(&draft).await.unwrap();
    let body = last_body(&server, "SendItem");
    assert_eq!(body["ItemIds"][0]["ChangeKey"].as_str(), draft.change_key.as_deref());
    assert_eq!(body["SaveItemToFolder"], true);
    assert_eq!(body["SavedItemFolderId"]["BaseFolderId"]["Id"], "sentitems");
    {
        let st = server.state();
        assert_eq!(st.sent.len(), 1);
        let sent = &st.sent[0];
        assert_eq!(sent.subject, edited.subject);
        assert_eq!(sent.body_html, "<div>Проверка отправки</div>");
        assert_eq!(sent.to.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), vec!["vasya@aiowa.test", OWNER]);
        assert_eq!((sent.cc.len(), sent.bcc.len()), (1, 0));
        assert_eq!(sent.attachments.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), vec!["отчёт.txt", "data.bin"]);
        assert!(st.items_in(DRAFTS).is_empty(), "черновик ушёл из «Черновиков»");
        assert!(st.items_in(SENT).iter().any(|i| i.subject == edited.subject && !i.is_draft), "копия в «Отправленных»");
        assert!(st.items_in(INBOX).iter().any(|i| i.subject == edited.subject && !i.is_read), "письмо себе пришло во «Входящие»");
    }
    t.logoff().await.unwrap();
}

/// Истечение сессии (440, 401, редирект на `logon.aspx`): ровно один повторный вход; сессия,
/// которая не поднимается, отдаёт `SessionExpired` без третьего входа; параллельные вызовы
/// входят один раз на всех; вложение — тот же путь.
#[tokio::test]
async fn expired_session_relogs_in_exactly_once() {
    for style in [Expiry::Status440, Expiry::Status401, Expiry::RedirectToLogon] {
        let server = FakeOwa::start().await;
        let t = connect(&server).await;
        server.state().faults.expire_next = Some(style);
        let counts = t.get_folder_counts(&[inbox()]).await.unwrap();
        assert_eq!(counts[0].total, 3, "{style:?}");
        {
            let st = server.state();
            assert_eq!(st.logins_ok(), 2, "{style:?}: ровно один повторный вход");
            assert_eq!(st.rejected, vec!["session".to_string()], "{style:?}");
            assert_eq!(st.calls_of("GetFolder").len(), 1, "{style:?}: операция выполнена один раз");
            assert_eq!(st.active_sessions(), 1, "{style:?}");
        }
        t.logoff().await.unwrap();
    }

    // Сервер не держит ни одной сессии: после повторного входа отказ уходит наверх.
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    server.state().faults.expire_every = Some(Expiry::Status440);
    let r = t.sync_folder_items(&folder(INBOX), None, 10, true).await;
    assert!(matches!(r, Err(TransportError::SessionExpired)), "{r:?}");
    assert_eq!(server.state().logins_ok(), 2, "второго повторного входа нет");
    assert_eq!(server.state().rejected.len(), 2);
    server.state().faults.expire_every = None;
    t.logoff().await.unwrap();

    // Два параллельных вызова застают сессию истёкшей: вход один на оба.
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    server.state().faults.expire_next = Some(Expiry::Status440);
    let folders = [inbox()];
    let (a, b) = tokio::join!(t.get_folder_counts(&folders), t.sync_folder_hierarchy(None));
    assert_eq!(a.unwrap()[0].total, 3);
    assert_eq!(b.unwrap().changes.len(), 5);
    assert_eq!(server.state().logins_ok(), 2);

    // Скачивание вложения после истечения сессии.
    server.state().faults.expire_next = Some(Expiry::Status440);
    let id = {
        let st = server.state();
        let it = st.item_by_subject(SHIPMENT).unwrap();
        it.attachment_id(&it.attachments[0])
    };
    let c = t.get_attachment(&id, 1 << 20).await.unwrap();
    assert_eq!(c.file_name.as_deref(), Some("спецификация.txt"));
    assert_eq!(server.state().logins_ok(), 3);
    t.logoff().await.unwrap();
}

/// Лимит сессий на ящик (§ 5c): вход сверх лимита — `SessionLimit`; `logoff` закрывает
/// сессию на сервере, после него транспорт не входит заново, а новый вход проходит.
#[tokio::test]
async fn session_limit_and_logoff() {
    let mut mailbox = Mailbox::seeded();
    mailbox.max_sessions = 1;
    let server = FakeOwa::start_with(mailbox).await;
    let first = connect(&server).await;
    match OwaTransport::connect(creds(&server, PASSWORD), config(&server)).await {
        Err(TransportError::SessionLimit) => {}
        Err(e) => panic!("ожидался SessionLimit: {e:?}"),
        Ok(_) => panic!("вход сверх лимита прошёл"),
    }
    assert_eq!(server.state().active_sessions(), 1);

    first.logoff().await.unwrap();
    {
        let st = server.state();
        assert_eq!((st.active_sessions(), st.logoffs), (0, 1));
    }
    let r = first.get_folder_counts(&[inbox()]).await;
    assert!(matches!(r, Err(TransportError::SessionExpired)), "{r:?}");
    assert_eq!(server.state().logins.len(), 2, "после logoff транспорт не входит заново");

    let second = connect(&server).await;
    assert_eq!(second.sync_folder_hierarchy(None).await.unwrap().changes.len(), 5);
    second.logoff().await.unwrap();
    let st = server.state();
    assert_eq!((st.active_sessions(), st.logoffs, st.logins_ok()), (0, 2, 3));
}

/// `logoff` посреди повторного входа: защёлка уже проверена, `POST auth.owa` задержан сервером.
/// Сессию, которую открыл этот вход, транспорт сразу закрывает сам: вызов получает
/// `SessionExpired`, операция не выполняется, открытых сессий на сервере не остаётся.
#[tokio::test]
async fn logoff_during_relogin_closes_the_new_session() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    let hold = Arc::new(Hold::default());
    {
        let mut st = server.state();
        st.faults.expire_next = Some(Expiry::Status440);
        st.faults.hold_auth = Some(hold.clone());
    }
    let call = tokio::spawn({
        let t = t.clone();
        async move { t.get_folder_counts(&[inbox()]).await }
    });
    // Повторный вход дошёл до формы: защёлка пройдена, новой сессии ещё нет.
    tokio::time::timeout(std::time::Duration::from_secs(10), hold.arrived.notified()).await.expect("повторный вход не дошёл до auth.owa");
    t.logoff().await.unwrap();
    hold.release.notify_one();
    let r = tokio::time::timeout(std::time::Duration::from_secs(30), call).await.expect("вызов не завершился").expect("задача вызова");
    assert!(matches!(r, Err(TransportError::SessionExpired)), "{r:?}");
    let st = server.state();
    assert_eq!(st.logins_ok(), 2, "повторный вход был, третьего нет");
    assert!(st.calls_of("GetFolder").is_empty(), "после logoff операция не выполняется");
    assert_eq!((st.active_sessions(), st.logoffs), (0, 1), "сессия, открытая после logoff, закрыта");
}

/// HTTP 500 на операцию — `Protocol` с текстом `FaultMessage`, без повторного входа. Для `GetItem`
/// с `message:ReplyTo` транспорт повторяет пачку без свойства, но разовый сбой Reply-To не
/// отключает: следующая пачка снова просит его.
#[tokio::test]
async fn operation_500_is_protocol_error_without_relogin() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    server.state().faults.fail_op.insert("SyncFolderItems".into(), 1);
    let r = t.sync_folder_items(&folder(INBOX), None, 10, true).await;
    assert!(matches!(&r, Err(TransportError::Protocol { op, status: 500, raw }) if op == "SyncFolderItems" && raw.contains("fail_op")), "{r:?}");
    assert!(!r.as_ref().unwrap_err().is_retryable());
    assert_eq!(server.state().logins_ok(), 1);
    let (metas, _) = sync_folder(&t, INBOX).await;
    assert_eq!(metas.len(), 3, "сбой был разовым");

    server.state().faults.fail_op.insert("GetItem".into(), 1);
    let ids: Vec<ItemId> = metas.iter().map(|m| m.item_id.clone()).collect();
    let results = t.get_items(&ids, ItemFetchShape::WithUniqueBody).await.unwrap();
    assert!(results.iter().all(|r| matches!(r, ItemResult::Ok(m) if m.reply_to.is_empty())), "без Reply-To: {results:?}");
    {
        let st = server.state();
        let calls = st.calls_of("GetItem");
        assert_eq!(calls.len(), 2);
        assert!(calls[0].body["ItemShape"].get("AdditionalProperties").is_some());
        assert!(calls[1].body["ItemShape"].get("AdditionalProperties").is_none());
    }
    let results = t.get_items(&ids, ItemFetchShape::WithUniqueBody).await.unwrap();
    {
        let st = server.state();
        let calls = st.calls_of("GetItem");
        assert_eq!(calls.len(), 3);
        assert!(calls[2].body["ItemShape"].get("AdditionalProperties").is_some(), "после разового 500 Reply-To снова запрашивается");
    }
    assert!(results.iter().any(|r| matches!(r, ItemResult::Ok(m) if !m.reply_to.is_empty())), "Reply-To вернулся: {results:?}");
    t.logoff().await.unwrap();
}

/// Полная синхронизация движком через настоящий транспорт во временную базу, затем
/// инкрементальная после чужих изменений на сервере.
#[tokio::test]
async fn engine_full_and_incremental_sync_into_store() {
    let server = FakeOwa::start().await;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("fake-owa-engine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Store::open(dir.join("mail.db")).unwrap());
    let account = store.account(&server.host(), USER).unwrap();
    let t = connect(&server).await;
    let engine = Engine::new(Arc::new(t.clone()), store.clone(), account, Arc::new(AtomicBool::new(false)));
    // Пачки по два письма и тела по два: путь через состояние и короткую пачку.
    let opts = SyncOptions { folders: FolderSelection::AllMail, body_days: None, page_size: 2, body_batch: 2, ..SyncOptions::default() };

    let report = engine.run(&opts).await.unwrap();
    assert_eq!(report.folders_in_hierarchy, 5);
    assert_eq!(report.folders_synced, 4, "«Удалённые» служебные, остальные синхронизируются");
    assert_eq!((report.items_created, report.bodies_fetched, report.bodies_failed), (5, 5, 0));
    assert_eq!((report.attachments_fetched, report.attachments_with_text, report.attachments_failed), (1, 1, 0));
    let file_id = {
        let st = server.state();
        let it = st.item_by_subject(SHIPMENT).unwrap();
        it.attachment_id(&it.attachments[0])
    };
    assert_eq!(server.state().downloads, vec![file_id], "инлайновая картинка не скачивается");

    let (inbox_pk, _) = store.folder_by_distinguished(account, "inbox").unwrap().unwrap();
    assert!(store.folder_sync_state(inbox_pk).unwrap().is_some());
    let rows = store.list_items(account, inbox_pk, 10).unwrap();
    assert_eq!(rows.len(), 3);
    let shipment_pk = rows.iter().find(|r| r.subject.as_deref() == Some(SHIPMENT)).unwrap().item_pk;
    assert!(store.message_body(shipment_pk).unwrap().is_some());
    let reply_to: Vec<_> = store.recipients(shipment_pk).unwrap().into_iter().filter(|r| r.kind == RecipientKind::ReplyTo.as_i64()).map(|r| r.email).collect();
    assert_eq!(reply_to, vec![Some("supply@aiowa.test".to_string())]);
    let hits = store.search(account, "шлепанц", 10).unwrap();
    assert_eq!(hits.len(), 1, "слово из текста вложения: {hits:?}");
    assert_eq!(hits[0].attachment_name.as_deref(), Some("спецификация.txt"));
    assert_eq!(store.search(account, "колым", 10).unwrap().len(), 1);

    // Другой клиент: прочитал отчёт, удалил совещание; пришло новое письмо.
    {
        let mut st = server.state();
        let report_id = st.item_by_subject(REPORT).unwrap().id.clone();
        let budget_id = st.item_by_subject(BUDGET).unwrap().id.clone();
        st.set_read_elsewhere(&report_id, true);
        st.remove_elsewhere(&budget_id);
        st.deliver("[AIOWA test] Новый шлагбаум", Addr::new("Анна Смирнова", "anna@aiowa.test"), "<div>Поставили новый шлагбаум у склада.</div>");
    }
    let report2 = engine.run(&opts).await.unwrap();
    assert_eq!((report2.items_created, report2.items_deleted, report2.read_flags, report2.bodies_fetched), (1, 1, 1, 1));
    let rows = store.list_items(account, inbox_pk, 10).unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.subject.as_deref() != Some(BUDGET)));
    assert!(rows.iter().find(|r| r.subject.as_deref() == Some(REPORT)).unwrap().is_read);
    assert_eq!(store.search(account, "шлагбаум", 10).unwrap().len(), 1);
    t.logoff().await.unwrap();
    assert_eq!(server.state().active_sessions(), 0);
    // Сначала движок и база (открытый SQLite на Windows не удалить), потом каталог.
    drop(engine);
    drop(store);
    std::fs::remove_dir_all(&dir).expect("временный каталог базы удаляется");
}

/// Сервер, который не принимает `message:ReplyTo`: с ним HTTP 500, без него ответ. После двух таких
/// пачек подряд транспорт перестаёт просить свойство; письма при этом приходят всегда.
#[tokio::test]
async fn reply_to_is_disabled_after_two_rejections() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    let (metas, _) = sync_folder(&t, INBOX).await;
    let ids: Vec<ItemId> = metas.iter().map(|m| m.item_id.clone()).collect();
    server.state().faults.reject_reply_to = true;
    for _ in 0..3 {
        let results = t.get_items(&ids, ItemFetchShape::WithUniqueBody).await.unwrap();
        assert!(results.iter().all(|r| matches!(r, ItemResult::Ok(m) if m.reply_to.is_empty())), "{results:?}");
    }
    // Пачки 1 и 2: с ним (500) и повтор без; пачка 3 — сразу без.
    assert_eq!(reply_to_requested(&server), vec![true, false, true, false, false]);
    t.logoff().await.unwrap();
}

/// Один сбой сервера застаёт две параллельные пачки с `message:ReplyTo` (`fail_op` на два
/// запроса, оба доходят до сервера раньше ответа): это один удар, а не два — Reply-To не
/// отключается, следующая пачка снова просит свойство и получает его.
#[tokio::test]
async fn parallel_batches_failing_together_count_one_strike() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    let (metas, _) = sync_folder(&t, INBOX).await;
    let ids: Vec<ItemId> = metas.iter().map(|m| m.item_id.clone()).collect();
    {
        let mut st = server.state();
        st.faults.fail_op.insert("GetItem".into(), 2);
        st.faults.gather = Some(Gather::new("GetItem", 2));
    }
    let (a, b) = tokio::join!(t.get_items(&ids[..1], ItemFetchShape::WithUniqueBody), t.get_items(&ids[1..], ItemFetchShape::WithUniqueBody));
    for results in [a.unwrap(), b.unwrap()] {
        assert!(results.iter().all(|r| matches!(r, ItemResult::Ok(m) if m.reply_to.is_empty())), "повтор без Reply-To: {results:?}");
    }
    assert_eq!(reply_to_requested(&server), vec![true, true, false, false]);
    let results = t.get_items(&ids, ItemFetchShape::WithUniqueBody).await.unwrap();
    assert!(results.iter().any(|r| matches!(r, ItemResult::Ok(m) if !m.reply_to.is_empty())), "Reply-To вернулся: {results:?}");
    assert_eq!(reply_to_requested(&server), vec![true, true, false, false, true], "после одного сбоя на две пачки Reply-To снова запрашивается");
    t.logoff().await.unwrap();
}

/// «Подряд» значит подряд: ответ 200 на запрос с `message:ReplyTo` (даже если все письма пачки
/// не найдены и форма ещё не признана принятой) сбрасывает счёт, и два разовых сбоя через такой
/// ответ Reply-To не отключают.
#[tokio::test]
async fn reply_to_strikes_reset_on_success() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    let (metas, _) = sync_folder(&t, INBOX).await;
    let ids: Vec<ItemId> = metas.iter().map(|m| m.item_id.clone()).collect();
    let ghost = [ItemId { id: "AAMkItem-9999".into(), change_key: None }];

    server.state().faults.fail_op.insert("GetItem".into(), 1);
    t.get_items(&ids, ItemFetchShape::WithUniqueBody).await.unwrap();
    let r = t.get_items(&ghost, ItemFetchShape::WithUniqueBody).await.unwrap();
    assert!(r[0].is_not_found(), "{r:?}");
    server.state().faults.fail_op.insert("GetItem".into(), 1);
    t.get_items(&ids, ItemFetchShape::WithUniqueBody).await.unwrap();

    let results = t.get_items(&ids, ItemFetchShape::WithUniqueBody).await.unwrap();
    assert!(results.iter().any(|r| matches!(r, ItemResult::Ok(m) if !m.reply_to.is_empty())), "Reply-To не отключён: {results:?}");
    // 500 и повтор без; 200 (не найдено); 500 и повтор без; снова со свойством.
    assert_eq!(reply_to_requested(&server), vec![true, false, true, true, false, true]);
    t.logoff().await.unwrap();
}

/// GET `GetFileAttachment` вернул страницу (200 `text/html` без `Content-Disposition`): это не файл,
/// транспорт берёт вложение операцией `GetAttachment`, а HTML страницы в содержимое не попадает.
#[tokio::test]
async fn html_page_instead_of_file_falls_back() {
    let server = FakeOwa::start().await;
    server.state().faults.html_page_file_get = true;
    let t = connect(&server).await;
    let id = shipment_file_id(&server);
    let c = t.get_attachment(&id, 1 << 20).await.unwrap();
    assert_eq!(c.bytes, "Поставка шлёпанцев, 40 пар".as_bytes());
    assert_eq!(server.state().calls_of("GetAttachment").len(), 1);
    assert_eq!(server.state().rejected, vec!["file_get_page".to_string()]);
    t.logoff().await.unwrap();
}

/// Сведения о ящике после входа (§ 2): из `sessiondata.ashx`; при его сбое — из
/// `GetOwaUserConfiguration`; истёкшая сессия — повторный вход и тот же ответ.
#[tokio::test]
async fn mailbox_info_from_session_data_and_fallback() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    let info = t.mailbox_info().await.unwrap();
    assert_eq!(info.email.as_deref(), Some(OWNER));
    assert_eq!(info.mailbox_guid.as_deref(), Some(MAILBOX_GUID));
    assert_eq!(info.display_name.as_deref(), Some(OWNER_NAME));
    assert_eq!(server.state().calls_of("sessiondata.ashx").len(), 1);
    assert!(server.state().calls_of("GetOwaUserConfiguration").is_empty(), "запасной путь не нужен");

    // sessiondata.ashx отвечает 500: те же сведения из GetOwaUserConfiguration.
    server.state().faults.fail_op.insert("sessiondata.ashx".into(), 1);
    assert_eq!(t.mailbox_info().await.unwrap(), info);
    assert_eq!(server.state().calls_of("GetOwaUserConfiguration").len(), 1);
    assert_eq!(server.state().calls_of("GetOwaUserConfiguration")[0].body, json!({}), "тело — пустой объект без конверта");

    // Оба пути отказали — ошибка основного.
    server.state().faults.fail_op.insert("sessiondata.ashx".into(), 1);
    server.state().faults.fail_op.insert("GetOwaUserConfiguration".into(), 1);
    assert!(matches!(t.mailbox_info().await, Err(TransportError::Protocol { status: 500, .. })));

    // Сессия истекла: один повторный вход, ответ тот же.
    server.state().faults.expire_next = Some(Expiry::Status440);
    assert_eq!(t.mailbox_info().await.unwrap(), info);
    assert_eq!(server.state().logins_ok(), 2);
    t.logoff().await.unwrap();
    assert_eq!(server.state().active_sessions(), 0);
}

/// § 5m: последние письма папки — запрос в форме, снятой с сервера (`FindItem`, `IndexedPageView`,
/// `SortResults` по `item:DateTimeReceived` по убыванию, свойства как у `SyncFolderItems`); свежие
/// первыми, с теми же идентификаторами и метаданными, что пришлёт полная выгрузка.
#[tokio::test]
async fn find_recent_items_newest_first_with_sync_ids() {
    let server = FakeOwa::start().await;
    let fresh = server.state().deliver("[AIOWA test] Новый шлагбаум", Addr::new("Анна Смирнова", "anna@aiowa.test"), "<div>Поставили шлагбаум у склада.</div>");
    let t = connect(&server).await;
    let recent = t.find_recent_items(&folder(INBOX), 2).await.unwrap();
    assert_eq!(recent.iter().map(|m| m.subject.as_deref().unwrap_or("")).collect::<Vec<_>>(), vec!["[AIOWA test] Новый шлагбаум", SHIPMENT]);
    assert_eq!(recent[0].item_id.id, fresh);
    {
        let st = server.state();
        let call = st.calls_of("FindItem")[0].clone();
        assert_eq!(call.version, "V2017_08_18");
        assert_eq!(call.body["Paging"], json!({ "__type": "IndexedPageView:#Exchange", "BasePoint": "Beginning", "Offset": 0, "MaxEntriesReturned": 2 }));
        assert_eq!(call.body["SortOrder"], json!([{ "__type": "SortResults:#Exchange", "Order": "Descending", "Path": { "__type": "PropertyUri:#Exchange", "FieldURI": "item:DateTimeReceived" } }]));
        assert_eq!(call.body["ParentFolderIds"], json!([{ "__type": "FolderId:#Exchange", "Id": INBOX }]));
        let props: Vec<&str> = call.body["ItemShape"]["AdditionalProperties"].as_array().unwrap().iter().filter_map(|p| p["FieldURI"].as_str()).collect();
        assert_eq!(props, protocol::META_FIELDS);
    }
    // Полная выгрузка присылает те же письма с теми же метаданными: запись поверх свежей страницы
    // идёт по идентификатору без дублей.
    let (all, _) = sync_folder(&t, INBOX).await;
    for m in &recent {
        assert_eq!(all.iter().find(|x| x.item_id.id == m.item_id.id), Some(m), "то же письмо в SyncFolderItems");
    }
    // Страница больше папки: вся папка, по убыванию даты.
    let everything = t.find_recent_items(&folder(INBOX), 100).await.unwrap();
    assert_eq!(everything.len(), 4);
    assert!(everything.windows(2).all(|w| w[0].received_at >= w[1].received_at));
    t.logoff().await.unwrap();
}

/// Первая синхронизация движком через настоящий транспорт: свежие страницы всех непустых папок
/// (`FindItem`) — раньше первого `SyncFolderItems`, полная выгрузка ложится поверх без дублей.
#[tokio::test]
async fn engine_takes_recent_pages_before_full_sync() {
    let server = FakeOwa::start().await;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("fake-owa-recent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Store::open(dir.join("mail.db")).unwrap());
    let account = store.account(&server.host(), USER).unwrap();
    let t = connect(&server).await;
    let engine = Engine::new(Arc::new(t.clone()), store.clone(), account, Arc::new(AtomicBool::new(false)));
    let opts = SyncOptions { folders: FolderSelection::AllMail, fetch_bodies: false, fetch_attachments: false, page_size: 2, recent_first: 2, ..SyncOptions::default() };
    let report = engine.run(&opts).await.unwrap();
    assert_eq!(report.recent_items, 4, "два свежих письма «Входящих», по одному из «Отправленных» и «Проектов»; пустые «Черновики» без запроса");
    assert_eq!((report.items_created, report.recent_vanished), (5, 0));
    assert_eq!(store.stats(account).unwrap().items, 5, "без дублей");
    {
        let st = server.state();
        let ops: Vec<&str> = st.calls.iter().map(|c| c.op.as_str()).collect();
        let last_find = ops.iter().rposition(|op| *op == "FindItem").expect("FindItem был");
        let first_sync = ops.iter().position(|op| *op == "SyncFolderItems").expect("SyncFolderItems был");
        assert!(last_find < first_sync, "свежие страницы раньше полных выгрузок: {ops:?}");
        assert_eq!(ops.iter().filter(|op| **op == "FindItem").count(), 3);
    }
    // Повторный прогон: у папок есть состояние, свежие страницы не запрашиваются.
    engine.run(&opts).await.unwrap();
    assert_eq!(server.state().calls_of("FindItem").len(), 3);
    t.logoff().await.unwrap();
}

/// Долгий запрос синхронизации занимает одно разрешение транспорта: тело письма и пометка из окна
/// проходят, не дожидаясь его ответа.
#[tokio::test]
async fn window_requests_pass_while_sync_request_is_pending() {
    let server = FakeOwa::start().await;
    let t = connect(&server).await;
    let (metas, _) = sync_folder(&t, INBOX).await;
    let shipment = by_subject(&metas, SHIPMENT).item_id.clone();
    let hold = Arc::new(Hold::default());
    server.state().faults.hold_op = Some(("SyncFolderItems".into(), hold.clone()));
    let pending = {
        let t = t.clone();
        tokio::spawn(async move { t.sync_folder_items(&folder(INBOX), None, 512, true).await })
    };
    tokio::time::timeout(Duration::from_secs(10), hold.arrived.notified()).await.expect("SyncFolderItems не дошёл до сервера");
    let window = async {
        let body = t.get_items(std::slice::from_ref(&shipment), ItemFetchShape::WithUniqueBody).await.unwrap();
        assert!(matches!(&body[0], ItemResult::Ok(m) if m.body.is_some()), "{body:?}");
        let marked = t.set_read(std::slice::from_ref(&shipment), true).await.unwrap();
        assert!(marked[0].is_ok(), "{marked:?}");
    };
    tokio::time::timeout(Duration::from_secs(10), window).await.expect("команды окна ждут ответа на запрос синхронизации");
    assert!(!pending.is_finished(), "запрос синхронизации всё ещё задержан сервером");
    hold.release.notify_one();
    let changes = tokio::time::timeout(Duration::from_secs(10), pending).await.expect("SyncFolderItems не завершился").unwrap().unwrap();
    assert_eq!(changes.changes.len(), 3);
    t.logoff().await.unwrap();
}
