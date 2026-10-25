//! Чей ящик в базе: проверка при входе (`sync.rs`) и поиск базы ящика в мастере (`setup.rs`).
//!
//! После входа сервер сообщает адрес ящика и `MailboxGuid` (`OwaTransport::mailbox_info`,
//! `sessiondata.ashx`). В обычном режиме при каждом входе они сверяются с базой: GUID в базе не
//! записан — записывается (вместе с адресом); записан GUID другого ящика — синхронизации в эту
//! базу нет, транспорт процесса не выдаётся до перезапуска, окно показывает ошибку. Сведения не
//! пришли (сбой сервера) — вход не блокируется, проверка повторится при следующем входе.
//!
//! Мастер ищет базы этого сервера (`%LOCALAPPDATA%\AIOWA\<сервер>\*\mail.db`): совпал GUID — та
//! же база; у старых баз GUID нет — кандидат, если совпал записанный адрес ящика или логин без
//! домена (`savvidi` = `savvidi@ngenergo` = `NGENERGO\savvidi`). Новая база заводится в каталоге
//! по адресу ящика, а не по введённому логину.
//!
//! Политика журналов: адреса и имена не пишутся — только признаки и счётчики.

use aiowa_core::AccountId;
use aiowa_store::identity::{email_local_part, login_key};
use aiowa_store::{safe_path_component, DbSummary, MailboxIdentity, Store};
use aiowa_transport_owa::mailbox::MailboxInfo;
use aiowa_transport_owa::OwaTransport;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{info, warn};

/// Предел запроса сведений о ящике при входе: дольше — вход без проверки.
const INFO_TIMEOUT: Duration = Duration::from_secs(30);

/// Почему база подходит ящику.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MatchKind {
    /// Совпал логин без домена (база заведена до записи личности ящика).
    Login,
    /// Совпал записанный в базе адрес ящика.
    Email,
    /// Совпал `MailboxGuid`: точно тот же ящик.
    Guid,
}

/// Подходит ли база ящику: лучшее совпадение среди учётных записей базы этого сервера. Запись с
/// GUID другого ящика или с другим записанным адресом не подходит, даже если логин похож.
pub(crate) fn classify(summary: &DbSummary, host: &str, user: &str, info: &MailboxInfo) -> Option<MatchKind> {
    let key = login_key(user);
    let email_key = info.email.as_deref().map(email_local_part);
    summary
        .accounts
        .iter()
        .filter(|a| a.host.eq_ignore_ascii_case(host))
        .filter_map(|a| {
            if let (Some(server), Some(stored)) = (&info.mailbox_guid, &a.identity.mailbox_guid) {
                return (server.eq_ignore_ascii_case(stored)).then_some(MatchKind::Guid);
            }
            if let (Some(server), Some(stored)) = (&info.email, &a.identity.email) {
                return server.eq_ignore_ascii_case(stored).then_some(MatchKind::Email);
            }
            let row = login_key(&a.login);
            (!row.is_empty() && (row == key || email_key.as_deref() == Some(row.as_str()))).then_some(MatchKind::Login)
        })
        .max()
}

/// Найденная база ящика.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub path: PathBuf,
    pub summary: DbSummary,
    pub kind: MatchKind,
}

/// Базы из `paths`, подходящие ящику: сначала полнее (больше писем), затем свежее. Полнота
/// важнее вида совпадения: база, заведённая с нуля под новой формой логина, уже несёт GUID, а
/// старая, полностью синхронизированная, — ещё нет, хотя ящик тот же.
pub(crate) fn find_candidates(paths: &[PathBuf], host: &str, user: &str, info: &MailboxInfo) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = paths
        .iter()
        .filter_map(|p| match crate::data::summary_cached(p) {
            Ok(summary) => classify(&summary, host, user, info).map(|kind| Candidate { path: p.clone(), summary, kind }),
            Err(e) => {
                warn!(error = %e, "база не прочитана при поиске базы ящика");
                None
            }
        })
        .collect();
    out.sort_by(|a, b| b.summary.items.cmp(&a.summary.items).then(b.summary.last_sync_ms.cmp(&a.summary.last_sync_ms)).then(b.kind.cmp(&a.kind)));
    out
}

/// Базы этого сервера под `root`: `<root>\<сервер>\*\mail.db`.
pub(crate) fn host_databases(root: &Path, host: &str) -> Vec<PathBuf> {
    let host_dir = root.join(safe_path_component(host));
    aiowa_store::identity::scan_databases(root).into_iter().filter(|p| p.parent().and_then(Path::parent) == Some(host_dir.as_path())).collect()
}

/// Путь новой базы: каталог из адреса ящика (`ivanov@x.ru` → `ivanov_x.ru`), а если адреса нет —
/// из логина. Каталог с базой уже занят — к имени добавляется `_2`, `_3`…
pub(crate) fn new_db_path(root: &Path, host: &str, user: &str, info: &MailboxInfo) -> PathBuf {
    let host_dir = root.join(safe_path_component(host));
    let base = safe_path_component(info.email.as_deref().unwrap_or(user));
    let first = host_dir.join(&base).join("mail.db");
    if !first.exists() {
        return first;
    }
    (2..1000).map(|n| host_dir.join(format!("{base}_{n}")).join("mail.db")).find(|p| !p.exists()).unwrap_or(first)
}

// --- проверка при входе ------------------------------------------------------------------

/// Итог сверки сведений сервера с базой.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Тот же ящик; `write` — что дописать в базу (GUID или адрес, которых не было).
    Same { write: Option<MailboxIdentity> },
    /// В базе записан GUID другого ящика.
    Other,
}

/// Сверка: GUID в базе и на сервере — равны или нет; GUID в базе нет — дописать известное.
pub(crate) fn decide(stored: &MailboxIdentity, server: &MailboxInfo) -> Verdict {
    if let (Some(a), Some(b)) = (&stored.mailbox_guid, &server.mailbox_guid)
        && !a.eq_ignore_ascii_case(b)
    {
        return Verdict::Other;
    }
    let write = MailboxIdentity {
        mailbox_guid: server.mailbox_guid.clone().filter(|_| stored.mailbox_guid.is_none()),
        email: server.email.clone().filter(|e| stored.email.as_deref().is_none_or(|s| !s.eq_ignore_ascii_case(e))),
    };
    Verdict::Same { write: (write.mailbox_guid.is_some() || write.email.is_some()).then_some(write) }
}

/// Текст для окна: база чужого ящика.
pub(crate) const WRONG_MAILBOX: &str = "Эта база принадлежит другому ящику: синхронизация в неё остановлена. Откройте настройки → «Учётная запись» → «Забыть учётную запись…» и пройдите мастер заново: он найдёт базу этого ящика или заведёт новую";

/// Сверка после входа: `Err(())` — база чужого ящика (транспорт выдавать нельзя).
pub(crate) async fn verify_login(store: &Store, account: AccountId, t: &OwaTransport) -> Result<(), ()> {
    let info = match tokio::time::timeout(INFO_TIMEOUT, t.mailbox_info()).await {
        Ok(Ok(info)) => info,
        Ok(Err(e)) => {
            warn!(error = %e, "сведения о ящике не получены: вход без сверки базы");
            return Ok(());
        }
        Err(_) => {
            warn!("сведения о ящике не пришли за {} с: вход без сверки базы", INFO_TIMEOUT.as_secs());
            return Ok(());
        }
    };
    let stored = match store.mailbox_identity(account) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "личность ящика в базе не прочитана: вход без сверки");
            return Ok(());
        }
    };
    match decide(&stored, &info) {
        Verdict::Other => {
            warn!("в базе записан GUID другого ящика: синхронизация в неё остановлена");
            Err(())
        }
        Verdict::Same { write: None } => Ok(()),
        Verdict::Same { write: Some(w) } => {
            match store.set_mailbox_identity(account, &w) {
                Ok(()) => info!(guid = w.mailbox_guid.is_some(), email = w.email.is_some(), "личность ящика записана в базу"),
                Err(e) => warn!(error = %e, "личность ящика не записана в базу"),
            }
            Ok(())
        }
    }
}

/// Защёлка «база чужого ящика»: после неё транспорт процесса не выдаётся до перезапуска (иначе
/// каждый таймер и канал уведомлений входили бы на сервер заново ради того же отказа).
#[derive(Debug, Default)]
pub(crate) struct IdentityGuard {
    mismatch: AtomicBool,
}

impl IdentityGuard {
    pub(crate) fn blocked(&self) -> bool {
        self.mismatch.load(Ordering::SeqCst)
    }

    pub(crate) fn block(&self) {
        self.mismatch.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_store::DbAccount;

    const GUID: &str = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
    const OTHER_GUID: &str = "9f8e7d6c-5b4a-4392-8170-6f5e4d3c2b1a";

    fn info(email: Option<&str>, guid: Option<&str>) -> MailboxInfo {
        MailboxInfo { email: email.map(String::from), mailbox_guid: guid.map(String::from), display_name: None }
    }

    fn summary(host: &str, login: &str, guid: Option<&str>, email: Option<&str>, items: i64) -> DbSummary {
        DbSummary {
            accounts: vec![DbAccount { host: host.into(), login: login.into(), identity: MailboxIdentity { mailbox_guid: guid.map(String::from), email: email.map(String::from) }, items, last_sync_ms: None }],
            items,
            ..DbSummary::default()
        }
    }

    #[test]
    fn candidates_by_guid_email_and_login() {
        let server = info(Some("savvidi@ngenergo.ru"), Some(GUID));
        let h = "mail.ngenergo.ru";
        // GUID совпал — та же база при любом логине.
        assert_eq!(classify(&summary(h, "kto-to", Some(GUID), None, 1), h, "savvidi", &server), Some(MatchKind::Guid));
        // GUID другой — не подходит, даже с тем же логином.
        assert_eq!(classify(&summary(h, "savvidi", Some(OTHER_GUID), None, 1), h, "savvidi", &server), None);
        // Старая база без личности: логин без домена в любой форме, без учёта регистра.
        for stored in ["savvidi@ngenergo", "NGENERGO\\Savvidi", "SAVVIDI"] {
            for typed in ["savvidi", "savvidi@ngenergo", "ngenergo\\savvidi"] {
                assert_eq!(classify(&summary(h, stored, None, None, 1), h, typed, &server), Some(MatchKind::Login), "{stored} / {typed}");
            }
        }
        // Логин непохож, но совпадает с адресом ящика до @.
        assert_eq!(classify(&summary(h, "NGENERGO\\savvidi", None, None, 1), h, "s.avvidi@corp.local", &server), Some(MatchKind::Login));
        // Записанный адрес ящика.
        assert_eq!(classify(&summary(h, "ktoto", None, Some("savvidi@ngenergo.ru"), 1), h, "x", &server), Some(MatchKind::Email));
        assert_eq!(classify(&summary(h, "savvidi", None, Some("drugoi@ngenergo.ru"), 1), h, "savvidi", &server), None, "записан другой адрес");
        // Чужой логин и другой сервер — нет.
        assert_eq!(classify(&summary(h, "petrov", None, None, 1), h, "savvidi", &server), None);
        assert_eq!(classify(&summary("mail.other.ru", "savvidi", None, None, 1), h, "savvidi", &server), None);
        // Сервер не сообщил сведений — только по логину.
        assert_eq!(classify(&summary(h, "savvidi@ngenergo", Some(GUID), None, 1), h, "savvidi", &MailboxInfo::default()), Some(MatchKind::Login));
        assert_eq!(classify(&DbSummary::default(), h, "savvidi", &server), None, "база без учётных записей");
    }

    #[test]
    fn new_database_dir_comes_from_mailbox_address() {
        let root = std::env::temp_dir().join(format!("aiowa-desktop-newdb-{}", std::process::id())).join("AIOWA");
        let _ = std::fs::remove_dir_all(&root);
        let server = info(Some("savvidi@ngenergo.ru"), Some(GUID));
        let p = new_db_path(&root, "mail.ngenergo.ru", "savvidi", &server);
        assert_eq!(p, root.join("mail.ngenergo.ru").join("savvidi_ngenergo.ru").join("mail.db"));
        // Тот же ящик под другой формой логина — тот же каталог.
        assert_eq!(new_db_path(&root, "mail.ngenergo.ru", "NGENERGO\\savvidi", &server), p);
        // Адреса нет — из логина, как раньше.
        assert_eq!(new_db_path(&root, "mail.ngenergo.ru", "savvidi", &MailboxInfo::default()), root.join("mail.ngenergo.ru").join("savvidi").join("mail.db"));
        // Каталог занят базой — следующее свободное имя.
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"").unwrap();
        assert_eq!(new_db_path(&root, "mail.ngenergo.ru", "savvidi", &server), root.join("mail.ngenergo.ru").join("savvidi_ngenergo.ru_2").join("mail.db"));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn candidates_are_ordered_by_completeness() {
        let root = std::env::temp_dir().join(format!("aiowa-desktop-candidates-{}", std::process::id())).join("AIOWA");
        let _ = std::fs::remove_dir_all(&root);
        let host = "mail.x.ru";
        let make = |dir: &str, login: &str, guid: Option<&str>, items: usize| {
            let path = root.join(host).join(dir).join("mail.db");
            let store = Store::open(&path).unwrap();
            let acc = store.account(host, login).unwrap();
            store.set_mailbox_identity(acc, &MailboxIdentity { mailbox_guid: guid.map(String::from), email: None }).unwrap();
            store.upsert_folders(acc, &[aiowa_core::Folder { id: aiowa_core::FolderId { id: "F".into(), change_key: None }, parent_id: None, display_name: "Входящие".into(), distinguished: Some("inbox".into()), folder_class: Some("IPF.Note".into()), total_count: 0, unread_count: 0, child_folder_count: 0 }]).unwrap();
            let fpk = store.folder_pk(acc, "F").unwrap().unwrap();
            let changes: Vec<_> = (0..items).map(|i| aiowa_core::ItemChange::Create(meta(&format!("{dir}-{i}")))).collect();
            store.apply_item_changes(acc, fpk, &changes).unwrap();
            path
        };
        let fresh = make("savvidi", "savvidi", Some(GUID), 1);
        let full = make("savvidi_ngenergo", "savvidi@ngenergo", None, 3);
        let foreign = make("petrov", "savvidi", Some(OTHER_GUID), 5);
        let paths = host_databases(&root, host);
        assert_eq!(paths.len(), 3);
        let found = find_candidates(&paths, host, "savvidi", &info(Some("savvidi@x.ru"), Some(GUID)));
        let got: Vec<(&Path, MatchKind)> = found.iter().map(|c| (c.path.as_path(), c.kind)).collect();
        assert_eq!(got, vec![(full.as_path(), MatchKind::Login), (fresh.as_path(), MatchKind::Guid)], "чужая база отброшена, полная — первой");
        assert!(!found.iter().any(|c| c.path == foreign));
        assert!(host_databases(&root, "mail.other.ru").is_empty());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    fn meta(id: &str) -> aiowa_core::MessageMeta {
        aiowa_core::MessageMeta {
            item_id: aiowa_core::ItemId { id: id.into(), change_key: None },
            parent_folder_id: None,
            conversation_id: None,
            internet_message_id: None,
            item_class: Some("IPM.Note".into()),
            subject: Some("[AIOWA test]".into()),
            from: None,
            sender: None,
            display_to: None,
            received_at: Some("2026-10-20T08:00:00Z".parse().unwrap()),
            sent_at: None,
            last_modified_at: None,
            is_read: true,
            is_draft: false,
            has_attachments: false,
            importance: aiowa_core::Importance::Normal,
            flag: Default::default(),
            size: None,
            preview: None,
        }
    }

    #[test]
    fn login_check_records_or_refuses() {
        let none = MailboxIdentity::default();
        // Первый вход: записать GUID и адрес.
        assert_eq!(decide(&none, &info(Some("a@x.ru"), Some(GUID))), Verdict::Same { write: Some(MailboxIdentity { mailbox_guid: Some(GUID.into()), email: Some("a@x.ru".into()) }) });
        // Всё уже записано — ничего не писать.
        let full = MailboxIdentity { mailbox_guid: Some(GUID.into()), email: Some("a@x.ru".into()) };
        assert_eq!(decide(&full, &info(Some("A@X.RU"), Some(&GUID.to_uppercase()))), Verdict::Same { write: None });
        // Другой GUID — чужая база.
        assert_eq!(decide(&full, &info(Some("a@x.ru"), Some(OTHER_GUID))), Verdict::Other);
        // Адрес сменился (переименование ящика), GUID тот же — адрес обновляется.
        assert_eq!(decide(&full, &info(Some("b@x.ru"), Some(GUID))), Verdict::Same { write: Some(MailboxIdentity { mailbox_guid: None, email: Some("b@x.ru".into()) }) });
        // Сервер ничего не сообщил — ничего не делать.
        assert_eq!(decide(&full, &MailboxInfo::default()), Verdict::Same { write: None });
        // В базе только адрес (старая запись), сервер сообщил GUID — дописать GUID.
        let email_only = MailboxIdentity { mailbox_guid: None, email: Some("a@x.ru".into()) };
        assert_eq!(decide(&email_only, &info(Some("a@x.ru"), Some(GUID))), Verdict::Same { write: Some(MailboxIdentity { mailbox_guid: Some(GUID.into()), email: None }) });
        let g = IdentityGuard::default();
        assert!(!g.blocked());
        g.block();
        assert!(g.blocked());
    }
}
