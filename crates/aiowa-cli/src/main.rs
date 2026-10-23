//! `aiowa`: консольная утилита этапа 1. Вход, папки, синхронизация в локальную базу,
//! список бесед, поиск с операторами, вложения, операции записи (прочитано, флаг, перенос, удаление).
//! Пароль хранится в диспетчере учётных данных Windows и не передаётся аргументом.

use aiowa_core::{FolderChange, FolderId, FolderRef, SyncState};
use aiowa_store::{AttachmentTodo, Store};
use aiowa_sync::{Engine, FolderSelection, SyncOptions};
use aiowa_transport::{Credentials, DeleteMode, MailTransport, WriteResult};
use aiowa_transport_owa::{OwaConfig, OwaTransport};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// Дополняет строку пробелами до `width` символов (не байт), обрезая длинную.
fn pad(s: &str, width: usize) -> String {
    let t = truncate(s, width);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(width.saturating_sub(n)))
}

const KEYRING_SERVICE: &str = "AIOWA";

#[derive(Parser)]
#[command(name = "aiowa", version, about = "AIOWA: оффлайн клиент Exchange. Этап 1: синхронизация и поиск из консоли")]
struct Cli {
    /// Хост OWA, например mail.example.com
    #[arg(long, env = "AIOWA_HOST")]
    host: String,
    /// Логин как в форме OWA: DOMAIN\user или user@domain
    #[arg(long, env = "AIOWA_USER")]
    user: String,
    /// Путь к базе; по умолчанию %LOCALAPPDATA%\AIOWA\<host>\<user>\mail.db
    #[arg(long, env = "AIOWA_DB")]
    db: Option<PathBuf>,
    /// Часовой пояс в заголовке запросов к серверу; даты хранятся в UTC
    #[arg(long, default_value = "UTC")]
    tz: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Запросить пароль и сохранить в диспетчере учётных данных Windows
    SetPassword,
    /// Удалить сохранённый пароль
    ForgetPassword,
    /// Проверить вход: сборка сервера и число папок
    Login,
    /// Показать дерево папок с сервера
    Folders,
    /// Синхронизировать почту в локальную базу
    Sync {
        /// Глубина в днях для загрузки тел и вложений (метаданные грузятся за всё время); 0 = без ограничения
        #[arg(long, default_value_t = 90)]
        days: u32,
        /// Какие папки синхронизировать: all (все почтовые кроме служебных) или список через запятую
        /// известных имён (inbox, sentitems) и отображаемых имён
        #[arg(long, default_value = "all")]
        folders: String,
        /// Только метаданные, без тел и вложений
        #[arg(long)]
        no_bodies: bool,
        /// Не скачивать вложения
        #[arg(long)]
        no_attachments: bool,
        /// Лимит размера одного вложения, МБ
        #[arg(long, default_value_t = 25)]
        max_attachment_mb: u64,
        /// Квота кэша файлов вложений на диске, МБ (текст остаётся в базе и после вытеснения)
        #[arg(long, default_value_t = 2048)]
        cache_mb: u64,
        /// Предел пачек SyncFolderItems на папку, 0 = без предела (для проб)
        #[arg(long, default_value_t = 0)]
        pages: u32,
    },
    /// Список бесед папки, как в OWA: тема, участники, писем, непрочитанных, дата последнего
    List {
        /// Папка: известное имя (inbox, sentitems) или отображаемое
        #[arg(long, default_value = "inbox")]
        folder: String,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Поиск по локальной базе. Операторы: от:/from:, кому:/to:, тема:/subject:,
    /// вложение:/has:attachment, папка:/in:, после:/до: (даты), остальное ищется по словам
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Скачать вложения одного письма с сервера (проба пути скачивания) и показать извлечённый текст
    FetchAttachments {
        /// item_pk письма в локальной базе (см. `search` или `sql`)
        #[arg(long)]
        item_pk: i64,
        #[arg(long, default_value_t = 25)]
        max_mb: u64,
    },
    /// Размер базы и покрытие по папкам
    Stats,
    /// Отладка: выполнить SELECT или PRAGMA к локальной базе и напечатать строки
    Sql { query: String },
    /// Пометить письмо прочитанным или непрочитанным на сервере (UpdateItem) и в базе
    MarkRead {
        /// item_pk письма в локальной базе
        #[arg(long)]
        item_pk: i64,
        /// true — прочитано, false — не прочитано
        #[arg(long, action = clap::ArgAction::Set)]
        read: bool,
    },
    /// Поставить или снять флаг «к исполнению» на сервере (UpdateItem) и в базе
    Flag {
        #[arg(long)]
        item_pk: i64,
        /// true — поставить, false — снять
        #[arg(long, action = clap::ArgAction::Set)]
        flagged: bool,
    },
    /// Перенести письмо в папку на сервере (MoveItem) и в базе. Без --yes-i-know только показывает, что сделает
    Move {
        #[arg(long)]
        item_pk: i64,
        /// Папка назначения: известное имя (inbox, deleteditems) или отображаемое
        #[arg(long)]
        to: String,
        /// Подтверждение: это запись в настоящий ящик
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Удалить письмо на сервере (DeleteItem): в «Удалённые» или навсегда. Без --yes-i-know только показывает, что сделает
    Delete {
        #[arg(long)]
        item_pk: i64,
        /// Удалить навсегда (HardDelete) вместо переноса в «Удалённые»
        #[arg(long)]
        permanent: bool,
        /// Подтверждение: это запись в настоящий ящик
        #[arg(long)]
        yes_i_know: bool,
    },
}

fn keyring_entry(user: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, user).map_err(|e| anyhow!("keyring: {e}"))
}

fn load_password(user: &str) -> Result<SecretString> {
    let pw = keyring_entry(user)?.get_password().map_err(|e| anyhow!("пароль не найден в диспетчере учётных данных ({e}); выполните: aiowa set-password"))?;
    Ok(SecretString::from(pw))
}

fn default_db_path(host: &str, user: &str) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let safe_user: String = user.chars().map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect();
    base.join("AIOWA").join(host).join(safe_user).join("mail.db")
}

async fn connect(cli: &Cli) -> Result<OwaTransport> {
    let password = load_password(&cli.user)?;
    let creds = Credentials { host: cli.host.clone(), user: cli.user.clone(), password };
    let config = OwaConfig { time_zone: cli.tz.clone(), ..OwaConfig::default() };
    OwaTransport::connect(creds, config).await.map_err(|e| anyhow!("вход не удался: {e}"))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,reqwest=warn,pdf_extract=error,lopdf=error"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).without_time().init();
}

fn folder_selection(spec: &str) -> FolderSelection {
    let names: Vec<String> = spec.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    if names.is_empty() || (names.len() == 1 && names[0].eq_ignore_ascii_case("all")) { FolderSelection::AllMail } else { FolderSelection::Named(names) }
}

fn fmt_date(t: Option<jiff::Timestamp>) -> String {
    t.map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match &cli.cmd {
        Cmd::SetPassword => {
            let pw = rpassword::prompt_password(format!("Пароль для {} на {}: ", cli.user, cli.host)).context("не удалось прочитать пароль")?;
            if pw.is_empty() {
                bail!("пустой пароль не сохраняю");
            }
            keyring_entry(&cli.user)?.set_password(&pw).map_err(|e| anyhow!("keyring: {e}"))?;
            println!("Пароль сохранён (служба {KEYRING_SERVICE}, пользователь {}).", cli.user);
            return Ok(());
        }
        Cmd::ForgetPassword => {
            keyring_entry(&cli.user)?.delete_credential().map_err(|e| anyhow!("keyring: {e}"))?;
            println!("Пароль удалён.");
            return Ok(());
        }
        _ => {}
    }

    let db_path = cli.db.clone().unwrap_or_else(|| default_db_path(&cli.host, &cli.user));

    match &cli.cmd {
        Cmd::Login => {
            let t = connect(&cli).await?;
            let info = t.server_info();
            let started = std::time::Instant::now();
            let folders = t.sync_folder_hierarchy(None).await.map_err(|e| anyhow!("{e}"))?;
            println!("Сервер {} сборка {}; папок в иерархии {} (SyncState {} символов) за {} мс", info.host, info.build.as_deref().unwrap_or("?"), folders.changes.len(), folders.state.0.len(), started.elapsed().as_millis());
            t.logoff().await.ok();
        }
        Cmd::Folders => {
            let t = connect(&cli).await?;
            let changes = t.sync_folder_hierarchy(None).await.map_err(|e| anyhow!("{e}"))?;
            print_folder_tree(&changes.changes);
            t.logoff().await.ok();
        }
        Cmd::Sync { days, folders, no_bodies, no_attachments, max_attachment_mb, cache_mb, pages } => {
            let store = Arc::new(Store::open(&db_path)?);
            let account = store.account(&cli.host, &cli.user)?;
            let opts = SyncOptions {
                folders: folder_selection(folders),
                body_days: if *days == 0 { None } else { Some(*days) },
                fetch_bodies: !no_bodies,
                fetch_attachments: !no_bodies && !no_attachments,
                max_attachment_bytes: max_attachment_mb * 1024 * 1024,
                attachment_cache_bytes: cache_mb * 1024 * 1024,
                max_pages: *pages,
                ..SyncOptions::default()
            };
            let cancel = Arc::new(AtomicBool::new(false));
            {
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        eprintln!("\nОстанавливаюсь после текущей пачки...");
                        cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            let transport = connect(&cli).await?;
            let engine = Engine::new(Arc::new(transport.clone()), store.clone(), account, cancel);
            let result = engine.run(&opts).await;
            transport.logoff().await.ok();
            let report = result.map_err(|e| anyhow!("синхронизация прервана: {e}"))?;
            println!(
                "Готово за {:.1} с: папок в иерархии {}, синхронизировано {}, пачек {}, писем создано {} / обновлено {} / удалено {}, флагов {}, тел загружено {} (ошибок {}, исчезло на сервере {}), вложений скачано {} ({:.1} МБ, с текстом {}, ошибок {}), из кэша вытеснено {} файлов ({:.1} МБ).\nБаза: {}",
                report.elapsed.as_secs_f32(),
                report.folders_in_hierarchy,
                report.folders_synced,
                report.pages,
                report.items_created,
                report.items_updated,
                report.items_deleted,
                report.read_flags,
                report.bodies_fetched,
                report.bodies_failed,
                report.bodies_gone,
                report.attachments_fetched,
                report.attachments_bytes as f64 / 1e6,
                report.attachments_with_text,
                report.attachments_failed,
                report.cache_files_evicted,
                report.cache_bytes_evicted as f64 / 1e6,
                db_path.display()
            );
        }
        Cmd::List { folder, limit } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let f = store.resolve_folder(account, folder)?.ok_or_else(|| anyhow!("папка «{folder}» не найдена в локальной базе"))?;
            let started = std::time::Instant::now();
            let rows = store.list_conversations(account, f.folder_pk, *limit)?;
            println!("{}: {} бесед за {} мс\n", f.name, rows.len(), started.elapsed().as_millis());
            println!("{}  {}  {:>5} {:>6}  {}", pad("Дата", 16), pad("Участники", 36), "Писем", "Непроч", "Тема");
            for c in rows {
                let who = c.participants.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
                let marker = if c.unread_count > 0 { "●" } else { " " };
                let clip = if c.has_attachments { " 📎" } else { "" };
                let count = if c.total_count != c.item_count { format!("{}/{}", c.item_count, c.total_count) } else { c.item_count.to_string() };
                println!("{}  {}  {:>5} {:>5}{}  {}{}", pad(&fmt_date(c.last_received_at), 16), pad(&who, 36), count, c.unread_count, marker, truncate(&c.topic.as_deref().unwrap_or("(без темы)").split_whitespace().collect::<Vec<_>>().join(" "), 70), clip);
            }
        }
        Cmd::Search { query, limit } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let started = std::time::Instant::now();
            let hits = store.search(account, query, *limit)?;
            println!("{} результатов за {} мс\n", hits.len(), started.elapsed().as_millis());
            for h in hits {
                let from = h.from_name.or(h.from_email).unwrap_or_default();
                let snippet = h.snippet.replace('\n', " ");
                let snippet = match &h.attachment_name {
                    Some(name) => format!("📎 {name}: {snippet}"),
                    None => snippet,
                };
                println!("{}  {}  [{}]  #{}\n    {}\n    {}\n", fmt_date(h.received_at), pad(&from, 28), h.folder_name.unwrap_or_default(), h.item_pk, truncate(h.subject.as_deref().unwrap_or("(без темы)"), 90), snippet);
            }
        }
        Cmd::FetchAttachments { item_pk, max_mb } => {
            let store = Arc::new(Store::open(&db_path)?);
            let account = store.account(&cli.host, &cli.user)?;
            let rows = store.attachments_of_item(*item_pk)?;
            if rows.is_empty() {
                bail!("у письма #{item_pk} нет вложений в базе (тело ещё не загружено?)");
            }
            let transport = connect(&cli).await?;
            let engine = Engine::new(Arc::new(transport.clone()), store.clone(), account, Arc::new(AtomicBool::new(false)));
            let max_bytes = max_mb * 1024 * 1024;
            for a in rows.iter().filter(|a| !a.is_inline) {
                store.reset_attachment(a.attachment_pk)?;
                let todo = AttachmentTodo { attachment_pk: a.attachment_pk, item_pk: a.item_pk, server_id: a.server_id.clone(), name: a.name.clone(), content_type: a.content_type.clone(), size: a.size };
                let started = std::time::Instant::now();
                match engine.fetch_one_attachment(&todo, max_bytes, aiowa_extract_limit()).await {
                    Ok(f) => {
                        let text = store.attachment_text(a.attachment_pk)?.unwrap_or_default();
                        println!(
                            "{}: {} байт за {} мс, текст {} символов (состояние {}), файл {}\n    {}",
                            a.name.as_deref().unwrap_or("?"),
                            f.bytes,
                            started.elapsed().as_millis(),
                            f.text_chars,
                            f.text_state,
                            f.path.display(),
                            truncate(&text.replace('\n', " "), 300)
                        );
                    }
                    Err(e) => println!("{}: ошибка: {e}", a.name.as_deref().unwrap_or("?")),
                }
            }
            transport.logoff().await.ok();
        }
        Cmd::Stats => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let s = store.stats(account)?;
            println!(
                "База {} : {:.1} МБ, писем {}, тел {}, бесед {}, вложений {} (скачано {}, с текстом {}, в кэше {:.1} МБ)\n",
                db_path.display(),
                s.db_bytes as f64 / 1e6,
                s.items,
                s.bodies,
                s.conversations,
                s.attachments,
                s.attachments_fetched,
                s.attachments_with_text,
                s.attachment_cache_bytes as f64 / 1e6
            );
            println!("{} {:>8} {:>8} {:>8}  {:<10} {:<10} {}", pad("Папка", 34), "Сервер", "Локально", "Тел", "Старейшее", "Новейшее", "Синхр.");
            for f in s.folders {
                let fmt = |t: Option<jiff::Timestamp>| t.map(|t| t.strftime("%Y-%m-%d").to_string()).unwrap_or_default();
                println!(
                    "{} {:>8} {:>8} {:>8}  {:<10} {:<10} {}",
                    pad(&format!("{}{}", f.display_name, f.distinguished.as_deref().map(|d| format!(" ({d})")).unwrap_or_default()), 34),
                    f.server_total,
                    f.local_items,
                    f.local_bodies,
                    fmt(f.oldest),
                    fmt(f.newest),
                    if f.synced { "полностью" } else { "частично" }
                );
            }
        }
        Cmd::MarkRead { item_pk, read } => {
            let store = Store::open(&db_path)?;
            let r = store.item_ref(*item_pk)?.ok_or_else(|| anyhow!("письмо #{item_pk} не найдено в базе или удалено"))?;
            println!("#{item_pk}: папка {}, is_read {} → {}, ChangeKey {}", r.folder_pk, r.is_read, read, if r.id.change_key.is_some() { "есть" } else { "нет" });
            let t = connect(&cli).await?;
            let result = t.set_read(std::slice::from_ref(&r.id), *read).await;
            t.logoff().await.ok();
            let results = result.map_err(|e| anyhow!("UpdateItem: {e}"))?;
            match results.first() {
                Some(WriteResult::Ok { id }) => {
                    let convs = store.set_read_local(&[*item_pk], *read)?;
                    store.update_item_id(*item_pk, id)?;
                    println!("UpdateItem: Ok; ItemId в ответе {}, ChangeKey {}; локально is_read = {}, бесед затронуто {}", if id.id == r.id.id { "тот же" } else { "другой" }, describe_change_key(&r.id, id), read, convs.len());
                }
                Some(WriteResult::Failed { code, message, .. }) => println!("UpdateItem: Failed {code}: {message}"),
                None => println!("UpdateItem: пустой ответ"),
            }
        }
        Cmd::Flag { item_pk, flagged } => {
            let store = Store::open(&db_path)?;
            let r = store.item_ref(*item_pk)?.ok_or_else(|| anyhow!("письмо #{item_pk} не найдено в базе или удалено"))?;
            println!("#{item_pk}: папка {}, is_flagged {} → {}", r.folder_pk, r.is_flagged, flagged);
            let t = connect(&cli).await?;
            let result = t.set_flag(std::slice::from_ref(&r.id), *flagged).await;
            t.logoff().await.ok();
            let results = result.map_err(|e| anyhow!("UpdateItem: {e}"))?;
            match results.first() {
                Some(WriteResult::Ok { id }) => {
                    let convs = store.set_flag_local(&[*item_pk], *flagged)?;
                    store.update_item_id(*item_pk, id)?;
                    println!("UpdateItem: Ok; ChangeKey {}; локально флаг = {}, бесед затронуто {}", describe_change_key(&r.id, id), flagged, convs.len());
                }
                Some(WriteResult::Failed { code, message, .. }) => println!("UpdateItem: Failed {code}: {message}"),
                None => println!("UpdateItem: пустой ответ"),
            }
        }
        Cmd::Move { item_pk, to, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let r = store.item_ref(*item_pk)?.ok_or_else(|| anyhow!("письмо #{item_pk} не найдено в базе или удалено"))?;
            let target = store.resolve_folder(account, to)?.ok_or_else(|| anyhow!("папка «{to}» не найдена в локальной базе"))?;
            if target.folder_pk == r.folder_pk {
                bail!("письмо #{item_pk} уже лежит в папке «{}»", target.name);
            }
            println!("#{item_pk}: из папки {} в «{}» (folder_pk {}) операцией MoveItem", r.folder_pk, target.name, target.folder_pk);
            if !yes_i_know {
                println!("Это запись в настоящий ящик. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let to_ref = FolderRef::Id(FolderId { id: target.server_id.clone(), change_key: None });
            let t = connect(&cli).await?;
            let result = t.move_items(std::slice::from_ref(&r.id), &to_ref).await;
            t.logoff().await.ok();
            let results = result.map_err(|e| anyhow!("MoveItem: {e}"))?;
            match results.first() {
                Some(WriteResult::Ok { id }) => {
                    let new_id = (id.id != r.id.id || id.change_key != r.id.change_key).then_some(id);
                    store.move_local(*item_pk, target.folder_pk, new_id)?;
                    println!("MoveItem: Ok; новый ItemId в ответе {}; локально письмо в папке {}", if new_id.is_some() { "есть" } else { "нет (остался прежний)" }, target.folder_pk);
                }
                Some(WriteResult::Failed { code, message, .. }) => println!("MoveItem: Failed {code}: {message}"),
                None => println!("MoveItem: пустой ответ"),
            }
        }
        Cmd::Delete { item_pk, permanent, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let r = store.item_ref(*item_pk)?.ok_or_else(|| anyhow!("письмо #{item_pk} не найдено в базе или удалено"))?;
            let deleted_items = store.folder_by_distinguished(account, "deleteditems")?;
            let in_deleted = deleted_items.as_ref().map(|(pk, _)| *pk == r.folder_pk).unwrap_or(false);
            let mode = if *permanent { DeleteMode::Hard } else { DeleteMode::ToDeletedItems };
            if in_deleted && !permanent {
                bail!("письмо #{item_pk} уже в «Удалённых»: используйте --permanent для удаления навсегда");
            }
            println!("#{item_pk}: папка {}, DeleteItem {}", r.folder_pk, if *permanent { "HardDelete (навсегда)" } else { "MoveToDeletedItems (в «Удалённые»)" });
            if !yes_i_know {
                println!("Это запись в настоящий ящик. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let result = t.delete_items(std::slice::from_ref(&r.id), mode).await;
            t.logoff().await.ok();
            let results = result.map_err(|e| anyhow!("DeleteItem: {e}"))?;
            match results.first() {
                Some(WriteResult::Ok { id }) => {
                    let new_id = (id.id != r.id.id).then_some(id);
                    match (mode, deleted_items) {
                        (DeleteMode::Hard, _) | (DeleteMode::ToDeletedItems, None) => {
                            let convs = store.mark_deleted_local(&[*item_pk])?;
                            println!("DeleteItem: Ok; локально deleted = 1, бесед затронуто {}", convs.len());
                        }
                        (DeleteMode::ToDeletedItems, Some((trash_pk, _))) => {
                            store.move_local(*item_pk, trash_pk, new_id)?;
                            println!("DeleteItem: Ok; новый ItemId в ответе {}; локально письмо в «Удалённых» (folder_pk {trash_pk})", if new_id.is_some() { "есть" } else { "нет" });
                        }
                    }
                }
                Some(WriteResult::Failed { code, message, .. }) => println!("DeleteItem: Failed {code}: {message}"),
                None => println!("DeleteItem: пустой ответ"),
            }
        }
        Cmd::Sql { query } => {
            let store = Store::open(&db_path)?;
            for line in store.debug_query(query)? {
                println!("{line}");
            }
        }
        Cmd::SetPassword | Cmd::ForgetPassword => unreachable!(),
    }
    Ok(())
}

fn aiowa_extract_limit() -> usize {
    SyncOptions::default().attachment_text_chars
}

/// Как изменился `ChangeKey` после операции записи (сами значения не печатаются).
fn describe_change_key(before: &aiowa_core::ItemId, after: &aiowa_core::ItemId) -> &'static str {
    match (&before.change_key, &after.change_key) {
        (_, None) => "в ответе отсутствует",
        (Some(b), Some(a)) if b == a => "тот же",
        _ => "новый",
    }
}

fn print_folder_tree(changes: &[FolderChange]) {
    let folders: Vec<&aiowa_core::Folder> = changes
        .iter()
        .filter_map(|c| match c {
            FolderChange::Create(f) | FolderChange::Update(f) => Some(f),
            FolderChange::Delete(_) => None,
        })
        .collect();
    let roots: Vec<&aiowa_core::Folder> = {
        let ids: std::collections::HashSet<&str> = folders.iter().map(|f| f.id.id.as_str()).collect();
        folders.iter().copied().filter(|f| f.parent_id.as_ref().map(|p| !ids.contains(p.id.as_str())).unwrap_or(true)).collect()
    };
    fn walk(f: &aiowa_core::Folder, all: &[&aiowa_core::Folder], depth: usize) {
        let marker = if f.is_mail() { "" } else { " ·" };
        println!(
            "{}{}{}  [{} / {} непроч.]{}",
            "  ".repeat(depth),
            f.display_name,
            f.distinguished.as_deref().map(|d| format!(" ({d})")).unwrap_or_default(),
            f.total_count,
            f.unread_count,
            marker
        );
        let mut children: Vec<&&aiowa_core::Folder> = all.iter().filter(|c| c.parent_id.as_ref().map(|p| p.id == f.id.id).unwrap_or(false)).collect();
        children.sort_by_key(|c| c.display_name.to_lowercase());
        for c in children {
            walk(c, all, depth + 1);
        }
    }
    let mut roots = roots;
    roots.sort_by_key(|f| f.display_name.to_lowercase());
    for r in roots {
        walk(r, &folders, 0);
    }
    println!("\nВсего папок: {} (· = не почтовая)", folders.len());
    let _ = SyncState(String::new());
}
