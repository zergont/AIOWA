//! Движок синхронизации на фейковом транспорте: без сети, без tokio-таймеров.

use aiowa_core::*;
use aiowa_store::Store;
use aiowa_sync::{Engine, FolderSelection, SyncOptions};
use aiowa_transport::{async_trait, AttachmentContent, FolderCounts, ItemFetchShape, ItemResult, MailTransport, Result as TResult, ServerInfo, TransportError};
use jiff::Timestamp;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn folder(id: &str, name: &str, dist: Option<&str>, total: i64) -> Folder {
    Folder {
        id: FolderId { id: id.to_string(), change_key: Some("ck".into()) },
        parent_id: Some(FolderId { id: "ROOT".into(), change_key: None }),
        display_name: name.to_string(),
        distinguished: dist.map(String::from),
        folder_class: Some("IPF.Note".into()),
        total_count: total,
        unread_count: 0,
        child_folder_count: 0,
    }
}

fn meta(id: &str, subject: &str, days_ago: i64) -> MessageMeta {
    let received = Timestamp::now() - jiff::SignedDuration::from_hours(24 * days_ago);
    MessageMeta {
        item_id: ItemId { id: id.to_string(), change_key: Some("c1".into()) },
        parent_folder_id: None,
        conversation_id: Some(ConversationId("conv".into())),
        internet_message_id: Some(format!("<{id}@fake>")),
        item_class: Some("IPM.Note".into()),
        subject: Some(subject.to_string()),
        from: Some(Participant { name: Some("Вася Пупкин".into()), email: Some("vasya@fake.ru".into()), routing_type: None }),
        sender: None,
        display_to: None,
        received_at: Some(received),
        sent_at: None,
        last_modified_at: None,
        is_read: false,
        is_draft: false,
        has_attachments: false,
        importance: Importance::Normal,
        flag: FlagStatus::NotFlagged,
        size: Some(1000),
        preview: None,
    }
}

/// Сценарий: входящие отдаются двумя страницами, вторая с признаком конца.
/// Тело письма "gone" на сервере отсутствует. У письма "a" два вложения: текстовое
/// скачивается, второе сервер отвергает. Папка «Проекты» пуста, нежелательная почта
/// синхронизироваться не должна.
struct Fake {
    sync_calls: AtomicUsize,
    get_calls: AtomicUsize,
    attachment_calls: AtomicUsize,
    /// Состояния, которые видел сервер в запросах `SyncFolderItems`.
    seen_states: Mutex<Vec<Option<String>>>,
    /// Папки, по которым запрашивались элементы.
    seen_folders: Mutex<Vec<String>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self { sync_calls: AtomicUsize::new(0), get_calls: AtomicUsize::new(0), attachment_calls: AtomicUsize::new(0), seen_states: Mutex::new(vec![]), seen_folders: Mutex::new(vec![]) })
    }
}

#[async_trait]
impl MailTransport for Fake {
    async fn sync_folder_hierarchy(&self, state: Option<&SyncState>) -> TResult<FolderChanges> {
        if state.is_some() {
            return Ok(FolderChanges { state: SyncState("H1".into()), changes: vec![], includes_last: true });
        }
        Ok(FolderChanges {
            state: SyncState("H1".into()),
            changes: vec![
                FolderChange::Create(folder("F-INBOX", "Входящие", Some("inbox"), 3)),
                FolderChange::Create(folder("F-SENT", "Отправленные", Some("sentitems"), 0)),
                FolderChange::Create(folder("F-PROJ", "Проекты", None, 0)),
                FolderChange::Create(folder("F-JUNK", "Нежелательная почта", Some("junkemail"), 100)),
            ],
            includes_last: true,
        })
    }

    async fn sync_folder_items(&self, folder: &FolderRef, state: Option<&SyncState>, _max: u32, _with_meta: bool) -> TResult<ItemChanges> {
        self.sync_calls.fetch_add(1, Ordering::SeqCst);
        self.seen_states.lock().unwrap().push(state.map(|s| s.0.clone()));
        let FolderRef::Id(fid) = folder else { panic!("ожидается FolderId") };
        self.seen_folders.lock().unwrap().push(fid.id.clone());
        if fid.id != "F-INBOX" {
            return Ok(ItemChanges { state: SyncState(format!("{}-done", fid.id)), changes: vec![], includes_last: true });
        }
        match state.map(|s| s.0.as_str()) {
            None => Ok(ItemChanges {
                state: SyncState("I1".into()),
                changes: vec![ItemChange::Create(meta("a", "Запчасти на Колыму", 1)), ItemChange::Create(meta("b", "Старое письмо", 400))],
                includes_last: false,
            }),
            Some("I1") => Ok(ItemChanges {
                state: SyncState("I2".into()),
                changes: vec![ItemChange::Create(meta("gone", "Исчезнувшее", 2)), ItemChange::ReadFlag { item_id: ItemId { id: "a".into(), change_key: None }, is_read: true }],
                includes_last: true,
            }),
            Some("I2") => Ok(ItemChanges { state: SyncState("I2".into()), changes: vec![], includes_last: true }),
            Some(other) => Err(TransportError::Server { code: "ErrorInvalidSyncStateData".into(), message: other.to_string() }),
        }
    }

    async fn get_items(&self, ids: &[ItemId], _shape: ItemFetchShape) -> TResult<Vec<ItemResult>> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ids
            .iter()
            .map(|id| {
                if id.id == "gone" {
                    ItemResult::Failed { id: id.clone(), code: "ErrorItemNotFound".into(), message: "нет".into() }
                } else {
                    // Сервер отдаёт текущее состояние: письмо «a» к этому моменту уже прочитано.
                    let mut m = meta(&id.id, if id.id == "a" { "Запчасти на Колыму" } else { "Старое письмо" }, 1);
                    m.is_read = id.id == "a";
                    let attachments = if id.id == "a" {
                        m.has_attachments = true;
                        vec![
                            AttachmentMeta { id: "att-ok".into(), name: Some("спецификация.txt".into()), content_type: Some("text/plain".into()), size: Some(60), is_inline: false, content_id: None },
                            AttachmentMeta { id: "att-bad".into(), name: Some("битое.pdf".into()), content_type: Some("application/pdf".into()), size: Some(100), is_inline: false, content_id: None },
                            AttachmentMeta { id: "att-inline".into(), name: Some("logo.png".into()), content_type: Some("image/png".into()), size: Some(10), is_inline: true, content_id: Some("cid1".into()) },
                        ]
                    } else {
                        vec![]
                    };
                    ItemResult::Ok(MessageFull {
                        meta: m,
                        to: vec![Participant { name: Some("Я".into()), email: Some("me@fake.ru".into()), routing_type: None }],
                        cc: vec![],
                        bcc: vec![],
                        body: Some(MessageBody { body_type: BodyType::Html, unique_body: Some("<p>Отгрузили <b>запчасти</b> на Колыму</p>".into()), full_body: None, is_truncated: false }),
                        attachments,
                    })
                }
            })
            .collect())
    }

    async fn get_folder_counts(&self, _folders: &[FolderRef]) -> TResult<Vec<FolderCounts>> {
        Ok(vec![])
    }

    async fn get_attachment(&self, attachment_id: &str, _max_bytes: u64) -> TResult<AttachmentContent> {
        self.attachment_calls.fetch_add(1, Ordering::SeqCst);
        match attachment_id {
            "att-ok" => Ok(AttachmentContent { bytes: "Поставка шлёпанцев, 40 пар".as_bytes().to_vec(), content_type: Some("text/plain".into()), file_name: Some("спецификация.txt".into()) }),
            _ => Err(TransportError::Server { code: "ErrorInvalidAttachmentId".into(), message: "нет такого вложения".into() }),
        }
    }

    async fn logoff(&self) -> TResult<()> {
        Ok(())
    }

    fn server_info(&self) -> ServerInfo {
        ServerInfo { host: "fake".into(), build: Some("15.2.0.0".into()) }
    }
}

fn test_store(tag: &str) -> Arc<Store> {
    let dir = std::env::temp_dir().join(format!("aiowa-sync-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Arc::new(Store::open(dir.join("mail.db")).unwrap())
}

#[tokio::test]
async fn full_then_incremental_sync() {
    let store = test_store("full");
    let account = store.account("fake", "u").unwrap();
    let fake = Fake::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let engine = Engine::new(fake.clone(), store.clone(), account, cancel.clone());
    let opts = SyncOptions { folders: FolderSelection::Named(vec!["inbox".into(), "sentitems".into()]), body_days: Some(30), body_batch: 10, ..SyncOptions::default() };

    let report = engine.run(&opts).await.unwrap();
    assert_eq!(report.folders_in_hierarchy, 4);
    assert_eq!(report.folders_synced, 2);
    assert_eq!(report.items_created, 3);
    assert_eq!(report.read_flags, 1);
    // Тела только за окно 30 дней: "a" и "gone"; "b" слишком старое. "gone" исчезло на сервере.
    assert_eq!(report.bodies_fetched, 1);
    assert_eq!(report.bodies_gone, 1);
    assert_eq!(report.bodies_failed, 0);
    // Вложения: текстовое скачано и разобрано, битое помечено, инлайновое не запрашивалось.
    assert_eq!((report.attachments_fetched, report.attachments_failed, report.attachments_with_text), (1, 1, 1));
    assert_eq!(fake.attachment_calls.load(Ordering::SeqCst), 2);
    assert!(report.attachments_bytes > 0);

    // Состояние папки сохранилось вместе с последней пачкой.
    let (inbox_pk, _) = store.folder_by_distinguished(account, "inbox").unwrap().unwrap();
    assert_eq!(store.folder_sync_state(inbox_pk).unwrap().unwrap().0, "I2");
    let rows = store.list_items(account, inbox_pk, 10).unwrap();
    assert_eq!(rows.len(), 2, "исчезнувшее письмо помечено удалённым: {rows:?}");
    assert!(rows.iter().find(|r| r.item_id.id == "a").unwrap().is_read);

    let hits = store.search(account, "колым", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("Колыму") || hits[0].subject.as_deref().unwrap().contains("Колыму"));
    // Слово из текста вложения находится, в выдаче имя файла.
    let hits = store.search(account, "шлепанц", 10).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].attachment_name.as_deref(), Some("спецификация.txt"));
    let att = store.attachments_of_item(hits[0].item_pk).unwrap();
    let ok = att.iter().find(|a| a.server_id == "att-ok").unwrap();
    assert!(ok.local_path.as_ref().unwrap().exists());
    assert_eq!(att.iter().find(|a| a.server_id == "att-bad").unwrap().text_state, aiowa_store::FETCH_FAILED);

    // Беседы входящих: одна беседа на два письма, одно непрочитанное.
    let convs = store.list_conversations(account, inbox_pk, 10).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!((convs[0].item_count, convs[0].unread_count), (2, 1));
    assert_eq!(convs[0].participants, vec!["Вася Пупкин"]);

    // Повторный запуск: сервер получает сохранённое состояние, ничего не меняется и не перекачивается.
    let before_sync = fake.sync_calls.load(Ordering::SeqCst);
    let before_att = fake.attachment_calls.load(Ordering::SeqCst);
    let report2 = engine.run(&opts).await.unwrap();
    assert_eq!(report2.items_created, 0);
    assert_eq!(report2.bodies_fetched, 0);
    assert_eq!(report2.attachments_fetched, 0);
    assert_eq!(fake.attachment_calls.load(Ordering::SeqCst), before_att, "битое вложение не запрашивается повторно");
    let states = fake.seen_states.lock().unwrap().clone();
    assert!(states.contains(&Some("I2".to_string())), "сервер должен получить сохранённое состояние входящих: {states:?}");
    assert!(states.contains(&Some("F-SENT-done".to_string())), "и отправленных: {states:?}");
    assert!(fake.sync_calls.load(Ordering::SeqCst) > before_sync);
    assert_eq!(store.stats(account).unwrap().items, 2);
}

#[tokio::test]
async fn all_mail_folders_skip_service_ones() {
    let store = test_store("all");
    let account = store.account("fake", "u").unwrap();
    let fake = Fake::new();
    let engine = Engine::new(fake.clone(), store.clone(), account, Arc::new(AtomicBool::new(false)));
    let opts = SyncOptions { folders: FolderSelection::AllMail, fetch_bodies: false, fetch_attachments: false, ..SyncOptions::default() };
    let report = engine.run(&opts).await.unwrap();
    assert_eq!(report.folders_synced, 3);
    let seen = fake.seen_folders.lock().unwrap().clone();
    assert!(seen.contains(&"F-PROJ".to_string()) && !seen.contains(&"F-JUNK".to_string()), "{seen:?}");
    assert_eq!(seen[0], "F-INBOX", "входящие первыми");
    assert_eq!(seen.iter().filter(|f| *f == "F-SENT").count(), 1);
    // Явный выбор по отображаемому имени и неизвестная папка.
    let named = SyncOptions { folders: FolderSelection::Named(vec!["проекты".into()]), fetch_bodies: false, fetch_attachments: false, ..SyncOptions::default() };
    assert_eq!(engine.run(&named).await.unwrap().folders_synced, 1);
    let missing = SyncOptions { folders: FolderSelection::Named(vec!["нет такой".into()]), ..SyncOptions::default() };
    assert!(matches!(engine.run(&missing).await, Err(aiowa_sync::SyncError::FolderNotFound(_))));
}

#[tokio::test]
async fn invalid_state_triggers_resync() {
    let store = test_store("resync");
    let account = store.account("fake", "u").unwrap();
    // Подложим папкам заведомо битое состояние.
    let fake = Fake::new();
    let engine = Engine::new(fake.clone(), store.clone(), account, Arc::new(AtomicBool::new(false)));
    let opts = SyncOptions { folders: FolderSelection::Named(vec!["inbox".into()]), fetch_bodies: false, fetch_attachments: false, ..SyncOptions::default() };
    engine.run(&opts).await.unwrap();
    let (inbox_pk, _) = store.folder_by_distinguished(account, "inbox").unwrap().unwrap();
    store.set_folder_sync_state(inbox_pk, Some(&SyncState("BROKEN".into())), false).unwrap();

    let report = engine.run(&opts).await.unwrap();
    // После сброса состояния папка выгружена заново; известные письма стали обновлениями.
    assert_eq!(report.items_created, 0);
    assert_eq!(report.items_updated, 3);
    assert_eq!(store.folder_sync_state(inbox_pk).unwrap().unwrap().0, "I2");
}
