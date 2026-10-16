//! Движок синхронизации на фейковом транспорте: без сети, без tokio-таймеров.

use aiowa_core::*;
use aiowa_store::Store;
use aiowa_sync::{Engine, SyncOptions};
use aiowa_transport::{async_trait, FolderCounts, ItemFetchShape, ItemResult, MailTransport, Result as TResult, ServerInfo, TransportError};
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

use jiff::Timestamp;

/// Сценарий: входящие отдаются двумя страницами, вторая с признаком конца.
/// Тело письма "gone" на сервере отсутствует.
struct Fake {
    sync_calls: AtomicUsize,
    get_calls: AtomicUsize,
    /// Состояния, которые видел сервер в запросах `SyncFolderItems`.
    seen_states: Mutex<Vec<Option<String>>>,
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
            ],
            includes_last: true,
        })
    }

    async fn sync_folder_items(&self, folder: &FolderRef, state: Option<&SyncState>, _max: u32, _with_meta: bool) -> TResult<ItemChanges> {
        self.sync_calls.fetch_add(1, Ordering::SeqCst);
        self.seen_states.lock().unwrap().push(state.map(|s| s.0.clone()));
        let FolderRef::Id(fid) = folder else { panic!("ожидается FolderId") };
        if fid.id == "F-SENT" {
            return Ok(ItemChanges { state: SyncState("S-done".into()), changes: vec![], includes_last: true });
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
                    ItemResult::Ok(MessageFull {
                        meta: m,
                        to: vec![Participant { name: Some("Я".into()), email: Some("me@fake.ru".into()), routing_type: None }],
                        cc: vec![],
                        bcc: vec![],
                        body: Some(MessageBody { body_type: BodyType::Html, unique_body: Some("<p>Отгрузили <b>запчасти</b> на Колыму</p>".into()), full_body: None, is_truncated: false }),
                        attachments: vec![],
                    })
                }
            })
            .collect())
    }

    async fn get_folder_counts(&self, _folders: &[FolderRef]) -> TResult<Vec<FolderCounts>> {
        Ok(vec![])
    }

    async fn logoff(&self) -> TResult<()> {
        Ok(())
    }

    fn server_info(&self) -> ServerInfo {
        ServerInfo { host: "fake".into(), build: Some("15.2.0.0".into()) }
    }
}

#[tokio::test]
async fn full_then_incremental_sync() {
    let dir = std::env::temp_dir().join(format!("aiowa-sync-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Store::open(dir.join("mail.db")).unwrap());
    let account = store.account("fake", "u").unwrap();
    let fake = Arc::new(Fake { sync_calls: AtomicUsize::new(0), get_calls: AtomicUsize::new(0), seen_states: Mutex::new(vec![]) });
    let cancel = Arc::new(AtomicBool::new(false));
    let engine = Engine::new(fake.clone(), store.clone(), account, cancel.clone());
    let opts = SyncOptions { body_days: Some(30), body_batch: 10, ..SyncOptions::default() };

    let report = engine.run(&opts).await.unwrap();
    assert_eq!(report.folders_in_hierarchy, 2);
    assert_eq!(report.folders_synced, 2);
    assert_eq!(report.items_created, 3);
    assert_eq!(report.read_flags, 1);
    // Тела только за окно 30 дней: "a" и "gone"; "b" слишком старое. "gone" исчезло на сервере.
    assert_eq!(report.bodies_fetched, 1);
    assert_eq!(report.bodies_gone, 1);
    assert_eq!(report.bodies_failed, 0);

    // Состояние папки сохранилось вместе с последней пачкой.
    let (inbox_pk, _) = store.folder_by_distinguished(account, "inbox").unwrap().unwrap();
    assert_eq!(store.folder_sync_state(inbox_pk).unwrap().unwrap().0, "I2");
    let rows = store.list_items(account, inbox_pk, 10).unwrap();
    assert_eq!(rows.len(), 2, "исчезнувшее письмо помечено удалённым: {rows:?}");
    assert!(rows.iter().find(|r| r.item_id.id == "a").unwrap().is_read);

    let hits = store.search(account, "колым", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("Колыму") || hits[0].subject.as_deref().unwrap().contains("Колыму"));

    // Повторный запуск: сервер получает сохранённое состояние, ничего не меняется.
    let before_sync = fake.sync_calls.load(Ordering::SeqCst);
    let report2 = engine.run(&opts).await.unwrap();
    assert_eq!(report2.items_created, 0);
    assert_eq!(report2.bodies_fetched, 0);
    let states = fake.seen_states.lock().unwrap().clone();
    assert!(states.contains(&Some("I2".to_string())), "сервер должен получить сохранённое состояние входящих: {states:?}");
    assert!(states.contains(&Some("S-done".to_string())), "и отправленных: {states:?}");
    assert!(fake.sync_calls.load(Ordering::SeqCst) > before_sync);
    assert_eq!(store.stats(account).unwrap().items, 2);
}

#[tokio::test]
async fn invalid_state_triggers_resync() {
    let dir = std::env::temp_dir().join(format!("aiowa-sync-test-resync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Store::open(dir.join("mail.db")).unwrap());
    let account = store.account("fake", "u").unwrap();
    // Подложим папкам заведомо битое состояние.
    let fake = Arc::new(Fake { sync_calls: AtomicUsize::new(0), get_calls: AtomicUsize::new(0), seen_states: Mutex::new(vec![]) });
    let engine = Engine::new(fake.clone(), store.clone(), account, Arc::new(AtomicBool::new(false)));
    let opts = SyncOptions { folders: vec![DistinguishedFolder::Inbox], fetch_bodies: false, ..SyncOptions::default() };
    engine.run(&opts).await.unwrap();
    let (inbox_pk, _) = store.folder_by_distinguished(account, "inbox").unwrap().unwrap();
    store.set_folder_sync_state(inbox_pk, Some(&SyncState("BROKEN".into())), false).unwrap();

    let report = engine.run(&opts).await.unwrap();
    // После сброса состояния папка выгружена заново; известные письма стали обновлениями.
    assert_eq!(report.items_created, 0);
    assert_eq!(report.items_updated, 3);
    assert_eq!(store.folder_sync_state(inbox_pk).unwrap().unwrap().0, "I2");
}
