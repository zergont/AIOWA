//! Фоновая очередь распознавания сканов (`docs/mcp-attachments.md`): пока окно открыто, AIOWA сама
//! распознаёт сканы в скачанных вложениях недавних писем, и письмо находится поиском по словам из
//! скана — у агента (`search_mail`) и в окне — без того, чтобы агент сначала открыл вложение.
//!
//! Берутся только файлы, которые уже лежат на диске: ради распознавания ничего не скачивается.
//! Распознавание то же, что по запросу агента (`pages::paged_text`), локальное. Что очередь
//! разобрала, отмечено в базе (`attachments.ocr_at`): к файлу, в котором распознавать нечего, она
//! не возвращается. Очередь живёт в процессе окна, берёт по одному вложению и после каждого ждёт не
//! меньше, чем работала, — занимает не больше половины одного ядра.
//!
//! Работает она, пока открыто то, ради чего нужна: чтение вложений агентами с распознаванием
//! сканов (вкладка «ИИ»). Выключатель и срок — там же.

use crate::mcp;
use crate::pages::{paged_text, OcrMode, OCR_PAGES_PER_CALL};
use crate::{now_ms, AppState};
use aiowa_core::AccountId;
use aiowa_ocr::SourceKind;
use aiowa_store::{AttachmentRow, Store};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Распознавать ли сканы заранее (`0` — нет); по умолчанию да.
pub(crate) const BACKGROUND_KEY: &str = "ocr.background";
/// За сколько последних месяцев брать письма; ключа нет — `MONTHS`.
pub(crate) const MONTHS_KEY: &str = "ocr.background_months";
pub(crate) const MONTHS: u32 = 6;
pub(crate) const MONTHS_MAX: u32 = 120;

/// Пауза перед первым проходом: сначала окно запускается и синхронизируется.
const START_DELAY: Duration = Duration::from_secs(45);
/// Пауза между проходами, когда очередь пуста или выключена.
const IDLE: Duration = Duration::from_secs(120);
/// Пауза, когда распознавать нечем: в Windows нет языков распознавания.
const UNAVAILABLE: Duration = Duration::from_secs(15 * 60);
/// После вложения очередь ждёт столько же, сколько работала, в этих пределах.
const PAUSE_MIN: Duration = Duration::from_millis(200);
const PAUSE_MAX: Duration = Duration::from_secs(5);
/// Работа короче этого паузы не требует (вложение пропущено без чтения файла).
const BUSY: Duration = Duration::from_millis(20);
/// Файл больше этого очередь не читает: агент по запросу прочитает его сам.
const MAX_BYTES: u64 = 50 * 1024 * 1024;
/// Картинка меньше этого — значок или подпись, а не скан документа.
const IMAGE_MIN_BYTES: u64 = 20 * 1024;
/// Сколько страниц одного файла очередь распознаёт; остальные — по запросу агента.
const PAGES_PER_FILE: usize = 100;

/// Настройки очереди: включена ли и за сколько месяцев берёт письма.
pub(crate) fn settings(store: &Store, account: AccountId) -> (bool, u32) {
    let get = |key: &str| store.get_state(account, key).ok().flatten();
    let on = get(BACKGROUND_KEY).as_deref() != Some("0");
    let months = get(MONTHS_KEY).and_then(|v| v.trim().parse::<u32>().ok()).filter(|n| (1..=MONTHS_MAX).contains(n)).unwrap_or(MONTHS);
    (on, months)
}

/// С какого времени письма попадают в очередь (месяц считается за 31 день).
pub(crate) fn since_ms(months: u32, now: i64) -> i64 {
    now - i64::from(months) * 31 * 24 * 3600 * 1000
}

/// Работает ли очередь сейчас; если да — с какого времени брать письма и порог «на странице нет
/// текста». Очередь включена сама, включено распознавание сканов и агентам открыты вложения.
pub(crate) fn active(store: &Store, account: AccountId) -> Option<(i64, usize)> {
    let (on, months) = settings(store, account);
    let (ocr, min_chars) = mcp::ocr_settings(store, account);
    (on && ocr && mcp::attachments_open(store, account)).then(|| (since_ms(months, now_ms()), min_chars))
}

/// Чем кончился разбор одного вложения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Распознано столько страниц.
    Recognized(usize),
    /// Распознавать нечего: текст есть на каждой странице или на картинке его нет.
    Nothing,
    /// Не PDF и не картинка, значок или слишком большой файл.
    Skipped,
    /// Файла нет на диске: отметка не ставится, очередь заглянет в следующий проход.
    NoFile,
    /// Файл не открылся или не разобрался.
    Failed,
    /// Распознавать на этом компьютере нечем: отметка не ставится, очередь ждёт.
    Unavailable,
}

/// Разобрать одно вложение: распознать страницы без текста и отметить, что очередь его видела.
/// Работа блокирующая.
pub(crate) fn process(store: &Store, row: &AttachmentRow, min_chars: usize) -> Outcome {
    let pk = row.attachment_pk;
    let done = |outcome: Outcome| {
        if let Err(e) = store.mark_ocr_done(pk) {
            warn!(attachment_pk = pk, error = %e, "фоновое распознавание: отметка не записана");
        }
        outcome
    };
    let Some(kind) = aiowa_ocr::source_kind(row.name.as_deref(), row.content_type.as_deref()) else { return done(Outcome::Skipped) };
    let Some(path) = row.local_path.as_deref().filter(|p| p.is_file()) else { return Outcome::NoFile };
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size == 0 || size > MAX_BYTES || (kind == SourceKind::Image && size < IMAGE_MIN_BYTES) {
        return done(Outcome::Skipped);
    }
    // PDF, который по страницам ещё никто не читал. Текст есть на каждой странице — распознавать
    // нечего, и страницы в базу не пишутся: текст такого вложения лежит в ней целиком.
    if kind == SourceKind::Pdf && matches!(store.attachment_pages(pk), Ok((None, _))) {
        let Ok(data) = std::fs::read(path) else { return Outcome::NoFile };
        if aiowa_extract::pdf_pages(&data, min_chars).is_ok_and(|pages| !pages.is_empty() && pages.iter().all(|t| t.chars().count() >= min_chars)) {
            return done(Outcome::Nothing);
        }
    }
    let mut recognized = 0;
    for _ in 0..PAGES_PER_FILE.div_ceil(OCR_PAGES_PER_CALL) {
        match paged_text(store, row, Some(path), OcrMode::Auto, None, min_chars) {
            // Движок есть, а этот файл для распознавания не открылся — дело в файле.
            Ok(Some(paged)) if paged.unavailable => return if aiowa_ocr::language().is_ok() { done(Outcome::Failed) } else { Outcome::Unavailable },
            Ok(Some(paged)) => {
                recognized += paged.recognized_now;
                if paged.left.is_empty() {
                    break;
                }
            }
            Ok(None) => return done(Outcome::Failed),
            Err(e) => {
                debug!(attachment_pk = pk, error = ?e, "фоновое распознавание: вложение не разобрано");
                return done(Outcome::Failed);
            }
        }
    }
    done(if recognized > 0 { Outcome::Recognized(recognized) } else { Outcome::Nothing })
}

/// Итог прохода для журнала: только счётчики.
#[derive(Debug, Default)]
struct Tally {
    files: usize,
    recognized: usize,
    pages: usize,
    nothing: usize,
    skipped: usize,
    no_file: usize,
    failed: usize,
}

impl Tally {
    fn add(&mut self, outcome: Outcome) {
        self.files += 1;
        match outcome {
            Outcome::Recognized(pages) => {
                self.recognized += 1;
                self.pages += pages;
            }
            Outcome::Nothing => self.nothing += 1,
            Outcome::Skipped => self.skipped += 1,
            Outcome::NoFile => self.no_file += 1,
            Outcome::Failed => self.failed += 1,
            Outcome::Unavailable => {}
        }
    }
}

/// Один проход по очереди; возвращает, сколько ждать до следующего.
pub(crate) async fn pass(store: Arc<Store>, account: AccountId) -> Duration {
    let plan = {
        let store = store.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let (since, min_chars) = active(&store, account)?;
            match store.ocr_queue(account, since) {
                Ok(queue) => Some((min_chars, queue)),
                Err(e) => {
                    warn!(error = %e, "фоновое распознавание: очередь не прочитана");
                    None
                }
            }
        })
        .await
    };
    let Ok(Some((min_chars, queue))) = plan else { return IDLE };
    if queue.is_empty() {
        return IDLE;
    }
    let started = Instant::now();
    let mut tally = Tally::default();
    for pk in queue {
        let step = Instant::now();
        let outcome = {
            let store = store.clone();
            tauri::async_runtime::spawn_blocking(move || {
                // Очередь могли выключить посреди прохода.
                active(&store, account)?;
                Some(match store.attachment(pk) {
                    Ok(Some(row)) => process(&store, &row, min_chars),
                    _ => Outcome::NoFile,
                })
            })
            .await
        };
        let outcome = match outcome {
            Ok(Some(outcome)) => outcome,
            Ok(None) => break,
            Err(e) => {
                // Разбор файла упал: отметить, чтобы очередь не падала на нём каждый проход.
                warn!(attachment_pk = pk, error = %e, "фоновое распознавание: разбор вложения прерван");
                let store = store.clone();
                let _ = tauri::async_runtime::spawn_blocking(move || store.mark_ocr_done(pk)).await;
                Outcome::Failed
            }
        };
        tally.add(outcome);
        if outcome == Outcome::Unavailable {
            info!("фоновое распознавание: в Windows нет языков распознавания, очередь ждёт");
            return UNAVAILABLE;
        }
        let worked = step.elapsed();
        if worked > BUSY {
            tokio::time::sleep(worked.clamp(PAUSE_MIN, PAUSE_MAX)).await;
        }
    }
    info!(files = tally.files, recognized = tally.recognized, pages = tally.pages, nothing = tally.nothing, skipped = tally.skipped, no_file = tally.no_file, failed = tally.failed, ms = started.elapsed().as_millis() as u64, "фоновое распознавание: проход закончен");
    IDLE
}

/// Запустить очередь вместе с окном.
pub(crate) fn start(app: tauri::AppHandle) {
    use tauri::Manager;
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(START_DELAY).await;
        loop {
            let pause = match app.try_state::<AppState>() {
                Some(state) => pass(state.store.clone(), state.account).await,
                None => IDLE,
            };
            tokio::time::sleep(pause).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::testing::text_pdf;
    use aiowa_core::{AttachmentMeta, BodyType, FlagStatus, Folder, FolderId, Importance, ItemId, MessageBody, MessageFull, MessageMeta, Participant};

    const LONG: &str = "This page has a real text layer that is longer than fifty characters";

    /// База с одним свежим письмом: его вложения названы как в `files` и получают их содержимое
    /// (пустое содержимое — файл не скачан).
    fn store_with(tag: &str, files: &[(&str, Vec<u8>)]) -> (Arc<Store>, AccountId, Vec<AttachmentRow>) {
        let dir = std::env::temp_dir().join(format!("aiowa-ocr-queue-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(dir.join("mail.db")).unwrap());
        let acc = store.account("h", "u").unwrap();
        let inbox = Folder { id: FolderId { id: "I".into(), change_key: None }, parent_id: None, display_name: "Входящие".into(), distinguished: Some("inbox".into()), folder_class: Some("IPF.Note".into()), total_count: 1, unread_count: 0, child_folder_count: 0 };
        store.upsert_folders(acc, &[inbox]).unwrap();
        let (folder_pk, _) = store.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let when = jiff::Timestamp::now();
        let message = MessageFull {
            meta: MessageMeta {
                item_id: ItemId { id: "A".into(), change_key: Some("ck".into()) },
                parent_folder_id: None,
                conversation_id: None,
                internet_message_id: None,
                item_class: Some("IPM.Note".into()),
                subject: Some("Уведомление".into()),
                from: Some(Participant { name: Some("Надзор".into()), email: Some("n@x.ru".into()), routing_type: Some("SMTP".into()) }),
                sender: None,
                display_to: None,
                received_at: Some(when),
                sent_at: Some(when),
                last_modified_at: None,
                is_read: true,
                is_draft: false,
                has_attachments: true,
                importance: Importance::Normal,
                flag: FlagStatus::NotFlagged,
                size: Some(1000),
                preview: None,
                last_verb: None,
                last_verb_at: None,
            },
            to: vec![],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            body: Some(MessageBody { body_type: BodyType::Html, unique_body: Some("<p>См. вложение.</p>".into()), full_body: None, is_truncated: false }),
            attachments: files.iter().enumerate().map(|(i, (name, _))| AttachmentMeta { id: format!("att-{i}"), name: Some((*name).into()), content_type: None, size: Some(100), is_inline: false, content_id: None }).collect(),
        };
        store.store_full_messages(acc, folder_pk, &[message]).unwrap();
        let item_pk = store.search(acc, "уведомление", 5).unwrap()[0].item_pk;
        for (row, (_, bytes)) in store.attachments_of_item(item_pk).unwrap().iter().zip(files) {
            if !bytes.is_empty() {
                store.save_attachment(&row.todo(), bytes, None, aiowa_store::AttachmentText::Unsupported).unwrap();
            }
        }
        let rows = store.attachments_of_item(item_pk).unwrap();
        (store, acc, rows)
    }

    #[test]
    fn files_without_scans_are_marked_and_left_alone() {
        let files = [("смета.pdf", text_pdf(&[LONG, LONG])), ("договор.docx", b"docx".to_vec()), ("значок.png", vec![7u8; 500]), ("пропал.pdf", text_pdf(&[LONG])), ("не скачан.pdf", vec![])];
        let (store, acc, rows) = store_with("text", &files);
        let since = since_ms(MONTHS, now_ms());
        // В очереди — скачанные PDF и картинка: документ Word и нескачанный файл в неё не попадают.
        assert_eq!(store.ocr_queue(acc, since).unwrap().len(), 3);
        assert_eq!(process(&store, &rows[0], 50), Outcome::Nothing);
        assert_eq!(store.attachment_pages(rows[0].attachment_pk).unwrap(), (None, vec![]), "страницы PDF с текстом в базу не пишутся");
        assert_eq!(process(&store, &rows[1], 50), Outcome::Skipped, "не PDF и не картинка");
        assert_eq!(process(&store, &rows[2], 50), Outcome::Skipped, "значок, а не скан");
        // Файл пропал с диска: отметки нет, очередь заглянет к нему в следующий проход.
        std::fs::remove_file(rows[3].local_path.as_deref().unwrap()).unwrap();
        assert_eq!(process(&store, &rows[3], 50), Outcome::NoFile);
        assert_eq!(process(&store, &rows[4], 50), Outcome::NoFile);
        assert_eq!(store.ocr_queue(acc, since).unwrap(), [rows[3].attachment_pk]);
        assert_eq!(store.ocr_queue_stats(acc, since).unwrap(), (1, 0));
    }

    #[test]
    fn scan_is_recognized_once_and_found_by_search() {
        let (store, acc, rows) = store_with("scan", &[("скан.pdf", text_pdf(&[LONG, "SCAN PAGE 2025"]))]);
        let pk = rows[0].attachment_pk;
        assert!(store.search(acc, "scan", 5).unwrap().is_empty());
        let outcome = process(&store, &rows[0], 50);
        if aiowa_ocr::language().is_err() {
            // Распознавать нечем: вложение остаётся в очереди до лучших времён.
            assert_eq!((outcome, store.ocr_queue(acc, 0).unwrap()), (Outcome::Unavailable, vec![pk]));
            return;
        }
        assert_eq!(outcome, Outcome::Recognized(1));
        let (total, pages) = store.attachment_pages(pk).unwrap();
        assert_eq!((total, pages.iter().filter(|p| p.source == aiowa_store::PAGE_OCR).map(|p| p.page).collect::<Vec<_>>()), (Some(2), vec![2]));
        assert_eq!(store.search(acc, "scan", 5).unwrap().len(), 1, "письмо находится по слову из скана");
        assert!(store.ocr_queue(acc, 0).unwrap().is_empty());
        assert_eq!(store.ocr_queue_stats(acc, 0).unwrap(), (0, 1));
        // Повторный разбор ничего не распознаёт заново.
        assert_eq!(process(&store, &rows[0], 50), Outcome::Nothing);
    }

    #[test]
    fn long_scan_is_recognized_whole() {
        if aiowa_ocr::language().is_err() {
            eprintln!("распознавание недоступно, тест пропущен");
            return;
        }
        // Скан в 23 страницы: больше, чем распознаётся за один вызов по запросу агента.
        let labels: Vec<String> = (1..=23).map(|n| format!("SHEET {n}")).collect();
        let (store, _, rows) = store_with("long", &[("альбом.pdf", text_pdf(&labels.iter().map(String::as_str).collect::<Vec<_>>()))]);
        assert_eq!(process(&store, &rows[0], 50), Outcome::Recognized(23));
        let (total, pages) = store.attachment_pages(rows[0].attachment_pk).unwrap();
        assert_eq!((total, pages.iter().filter(|p| p.source == aiowa_store::PAGE_OCR).count()), (Some(23), 23));
    }

    #[tokio::test]
    async fn queue_runs_only_while_agents_may_read_scans() {
        let (store, acc, rows) = store_with("pass", &[("смета.pdf", text_pdf(&[LONG])), ("договор.docx", b"docx".to_vec())]);
        let pk = rows[0].attachment_pk;
        assert_eq!(settings(&store, acc), (true, MONTHS));
        // Доступ агентов выключен (так по умолчанию) — очередь стоит.
        assert!(active(&store, acc).is_none());
        assert_eq!((pass(store.clone(), acc).await, store.ocr_queue(acc, 0).unwrap()), (IDLE, vec![pk]));
        // Включён — проход разбирает очередь.
        store.set_state(acc, mcp::ENABLED_KEY, Some("1")).unwrap();
        assert_eq!(active(&store, acc).map(|(_, min_chars)| min_chars), Some(crate::pages::PAGE_MIN_CHARS));
        assert_eq!((pass(store.clone(), acc).await, store.ocr_queue(acc, 0).unwrap()), (IDLE, vec![]));
        // Выключатели: сама очередь, распознавание сканов, режим «Только свои модели».
        for (key, value) in [(BACKGROUND_KEY, "0"), (mcp::OCR_KEY, "0"), (crate::ai::MODE_KEY, "own_only")] {
            store.set_state(acc, key, Some(value)).unwrap();
            assert!(active(&store, acc).is_none(), "{key}");
            store.set_state(acc, key, None).unwrap();
            assert!(active(&store, acc).is_some(), "{key}");
        }
        // Срок: число месяцев от 1 до 120, иначе шесть.
        for (value, months) in [("24", 24), ("0", MONTHS), ("999", MONTHS), ("год", MONTHS)] {
            store.set_state(acc, MONTHS_KEY, Some(value)).unwrap();
            assert_eq!(settings(&store, acc), (true, months), "{value}");
        }
        assert_eq!(since_ms(1, 40 * 24 * 3600 * 1000), 9 * 24 * 3600 * 1000);
    }

    /// Живая проверка на КОПИИ настоящей базы: один проход очереди по скачанным вложениям. Без
    /// переменных окружения тест ничего не делает. Печатаются только счётчики и время — ни имён, ни
    /// текста. `AIOWA_LIVE_DB` — копия `mail.db` в каталоге `live-copy`, `AIOWA_LIVE_HOST` и
    /// `AIOWA_LIVE_USER` — учётная запись, `AIOWA_LIVE_MONTHS` и `AIOWA_LIVE_LIMIT` — срок и
    /// сколько вложений разобрать.
    #[test]
    #[ignore = "живая проверка на копии базы, запускается вручную"]
    fn live_copy_pass() {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let (Some(db), Some(host), Some(user)) = (var("AIOWA_LIVE_DB"), var("AIOWA_LIVE_HOST"), var("AIOWA_LIVE_USER")) else {
            eprintln!("нужны AIOWA_LIVE_DB, AIOWA_LIVE_HOST и AIOWA_LIVE_USER");
            return;
        };
        assert!(db.replace('\\', "/").contains("/live-copy/"), "только копия базы в каталоге live-copy");
        let months: u32 = var("AIOWA_LIVE_MONTHS").and_then(|v| v.parse().ok()).unwrap_or(MONTHS);
        let limit: usize = var("AIOWA_LIVE_LIMIT").and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
        let store = Store::open(&db).unwrap();
        let acc = store.account(&host, &user).unwrap();
        let since = since_ms(months, now_ms());
        let queue = store.ocr_queue(acc, since).unwrap();
        println!("до прохода: в очереди {}, распознано вложений {}, язык {:?}", queue.len(), store.ocr_queue_stats(acc, since).unwrap().1, aiowa_ocr::language().ok());
        let started = Instant::now();
        let (mut pdf, mut image) = (Tally::default(), Tally::default());
        let (mut slowest, mut work) = (Duration::ZERO, Duration::ZERO);
        for pk in queue.into_iter().take(limit) {
            let Some(row) = store.attachment(pk).unwrap() else { continue };
            let step = Instant::now();
            let outcome = process(&store, &row, crate::pages::PAGE_MIN_CHARS);
            slowest = slowest.max(step.elapsed());
            work += step.elapsed();
            match aiowa_ocr::source_kind(row.name.as_deref(), row.content_type.as_deref()) {
                Some(SourceKind::Image) => image.add(outcome),
                _ => pdf.add(outcome),
            }
            if outcome == Outcome::Unavailable {
                println!("распознавать нечем");
                break;
            }
        }
        println!("PDF: {pdf:?}");
        println!("картинки: {image:?}");
        let pages = pdf.pages + image.pages;
        println!("время: всего {} с, работа {} с, самое долгое вложение {} мс, на страницу {} мс", started.elapsed().as_secs(), work.as_secs(), slowest.as_millis(), if pages > 0 { work.as_millis() / pages as u128 } else { 0 });
        let (waiting, recognized) = store.ocr_queue_stats(acc, since).unwrap();
        println!("после прохода: в очереди {waiting}, распознано вложений {recognized}");
    }
}
