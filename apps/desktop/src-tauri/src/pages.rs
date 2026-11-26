//! Текст вложения по страницам — для агента (`mcp_tools::get_attachment`). У PDF берётся текстовый
//! слой каждой страницы, а где его нет (скан) — страница распознаётся; картинка (фотография
//! документа) распознаётся целиком. Распознавание локальное (`aiowa-ocr`, средства Windows).
//!
//! Что уже извлечено и распознано, лежит в базе (`attachment_pages`): повторный вызов распознавание
//! не запускает. После распознавания текст вложения целиком пересобирается из страниц, и письмо
//! начинает находиться поиском по словам из скана.

use aiowa_ocr::{OcrError, SourceKind};
use aiowa_store::{AttachmentPage, AttachmentRow, Store, PAGE_EXTRACTED, PAGE_OCR};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Сколько страниц распознаётся за один вызов; остальные агент запрашивает параметром `pages`.
pub(crate) const OCR_PAGES_PER_CALL: usize = 20;
/// На странице меньше стольких символов текстового слоя — это скан, её нужно распознавать.
pub(crate) const PAGE_MIN_CHARS: usize = 50;
/// Сколько времени один вызов может распознавать: дальше — частичный ответ с пояснением.
const OCR_TIME_BUDGET: Duration = Duration::from_secs(120);
/// Предел текста одной страницы, символов.
const PAGE_CHARS: usize = 200_000;

/// Параметр `ocr` инструмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OcrMode {
    /// Распознавать только страницы без текстового слоя.
    Auto,
    /// Не распознавать и распознанное раньше не отдавать: только текст из файла.
    Off,
    /// Распознать все запрошенные страницы, даже с текстовым слоем.
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageSource {
    Extracted,
    Ocr,
    /// Текста у страницы нет.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageOut {
    pub page: u32,
    pub source: PageSource,
    pub text: String,
}

/// Текст запрошенных страниц вложения.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Paged {
    pub pages_total: u32,
    pub pages: Vec<PageOut>,
    /// Пояснение: распознан не весь файл, распознавание недоступно или не удалось.
    pub note: Option<String>,
    /// Страницы, до которых распознавание в этом вызове не дошло: предел страниц или времени.
    pub left: Vec<u32>,
    /// Сколько страниц распознано этим вызовом.
    pub recognized_now: usize,
    /// Файл для распознавания не открылся: распознавать на этом компьютере нечем.
    pub unavailable: bool,
}

impl Paged {
    /// `extracted`, `ocr`, `mixed` или `none` — откуда взят текст отданных страниц.
    pub(crate) fn text_source(&self) -> &'static str {
        let has = |s: PageSource| self.pages.iter().any(|p| p.source == s);
        match (has(PageSource::Extracted), has(PageSource::Ocr)) {
            (true, true) => "mixed",
            (true, false) => "extracted",
            (false, true) => "ocr",
            (false, false) => "none",
        }
    }

    pub(crate) fn ocr_pages(&self) -> Vec<u32> {
        self.pages.iter().filter(|p| p.source == PageSource::Ocr).map(|p| p.page).collect()
    }

    /// Текст со строкой `--- стр. N ---` перед каждой страницей; пусто — текста нет ни на одной.
    pub(crate) fn text(&self) -> String {
        if self.pages.iter().all(|p| p.source == PageSource::None) {
            return String::new();
        }
        self.pages.iter().map(|p| if p.text.is_empty() { format!("--- стр. {} ---\n(текста нет)", p.page) } else { format!("--- стр. {} ---\n{}", p.page, p.text) }).collect::<Vec<_>>().join("\n\n")
    }
}

/// Отказ: неверный запрос агента или сбой при чтении файла.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PagesError {
    Invalid(String),
    Failed(String),
}

/// `"3"`, `"1-5"`, `"1-3, 7"` → номера страниц по возрастанию без повторов.
pub(crate) fn parse_pages(raw: &str) -> Result<Vec<u32>, String> {
    const WRONG: &str = "Параметр pages — номер страницы или диапазон: \"3\", \"1-5\", \"1-3,7\"";
    let mut out = Vec::new();
    for part in raw.split([',', ';']).map(str::trim).filter(|p| !p.is_empty()) {
        let number = |s: &str| s.trim().parse::<u32>().ok().filter(|n| *n >= 1).ok_or_else(|| WRONG.to_string());
        let (from, to) = match part.split_once(['-', '–', '—']) {
            Some((a, b)) => (number(a)?, number(b)?),
            None => (number(part)?, number(part)?),
        };
        if to < from || to - from >= 10_000 {
            return Err(WRONG.into());
        }
        out.extend(from..=to);
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() { Err(WRONG.into()) } else { Ok(out) }
}

/// Номера страниц коротко: `21–40, 45`.
pub(crate) fn ranges(pages: &[u32]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < pages.len() {
        let mut j = i;
        while j + 1 < pages.len() && pages[j + 1] == pages[j] + 1 {
            j += 1;
        }
        out.push(if j > i { format!("{}–{}", pages[i], pages[j]) } else { pages[i].to_string() });
        i = j + 1;
    }
    out.join(", ")
}

/// Что известно о странице: текстовый слой и распознанный текст.
#[derive(Debug, Clone, Default)]
struct Known {
    extracted: Option<String>,
    ocr: Option<String>,
}

fn chars(s: &Option<String>) -> usize {
    s.as_deref().map_or(0, |t| t.chars().count())
}

/// Нужно ли распознавать страницу при таком режиме.
fn needs_ocr(known: Option<&Known>, mode: OcrMode, min_chars: usize) -> bool {
    let (extracted, recognized) = known.map_or((0, false), |k| (chars(&k.extracted), k.ocr.is_some()));
    match mode {
        OcrMode::Off => false,
        OcrMode::Auto => !recognized && extracted < min_chars,
        OcrMode::Force => !recognized,
    }
}

/// Какой текст страницы отдавать при таком режиме.
fn choose(known: &Known, mode: OcrMode, min_chars: usize) -> (PageSource, &str) {
    let extracted = known.extracted.as_deref().unwrap_or("");
    let recognized = known.ocr.as_deref().filter(|t| !t.trim().is_empty());
    let from_file = || if extracted.is_empty() { (PageSource::None, "") } else { (PageSource::Extracted, extracted) };
    match (mode, recognized) {
        (OcrMode::Force, Some(text)) => (PageSource::Ocr, text),
        // Страница-скан: распознанное полнее обрывка текстового слоя (номер страницы, колонтитул).
        (OcrMode::Auto, Some(text)) if extracted.chars().count() < min_chars => (PageSource::Ocr, text),
        _ => from_file(),
    }
}

fn clip_page(text: String) -> String {
    match text.char_indices().nth(PAGE_CHARS) {
        Some((at, _)) => text[..at].to_string(),
        None => text,
    }
}

/// Текст страниц вложения. `Ok(None)` — у такого файла страниц нет (Word, Excel, текст) или их не
/// удалось посчитать: вызывающий отдаёт текст вложения по-старому. `file` — файл в кэше, если он
/// есть на диске; без него отдаётся только то, что уже лежит в базе. Работа блокирующая.
pub(crate) fn paged_text(store: &Store, row: &AttachmentRow, file: Option<&Path>, mode: OcrMode, wanted: Option<&[u32]>, min_chars: usize) -> Result<Option<Paged>, PagesError> {
    let Some(kind) = aiowa_ocr::source_kind(row.name.as_deref(), row.content_type.as_deref()) else { return Ok(None) };
    let db = |e: aiowa_store::StoreError| PagesError::Failed(format!("Ошибка локальной базы: {e}"));
    let pk = row.attachment_pk;
    let (cached_total, cached) = store.attachment_pages(pk).map_err(db)?;
    let mut known: BTreeMap<u32, Known> = BTreeMap::new();
    for p in cached {
        let entry = known.entry(p.page.max(0) as u32).or_default();
        match p.source {
            PAGE_OCR => entry.ocr = Some(p.text),
            _ => entry.extracted = Some(p.text),
        }
    }
    let mut total = cached_total.filter(|n| *n > 0).map(|n| n as u32);
    let mut fresh: Vec<AttachmentPage> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut bytes: Option<Vec<u8>> = None;
    let mut document: Option<aiowa_ocr::Document> = None;
    let read = |path: &Path| std::fs::read(path).map_err(|e| PagesError::Failed(format!("Файл вложения не прочитан: {e}")));

    // 1. Число страниц и текстовый слой — один раз на вложение.
    if total.is_none() {
        let Some(path) = file else { return Ok(None) };
        let data = read(path)?;
        match kind {
            SourceKind::Pdf => match aiowa_extract::pdf_pages(&data, PAGE_CHARS) {
                Ok(pages) if !pages.is_empty() => {
                    total = Some(pages.len() as u32);
                    for (i, text) in pages.into_iter().enumerate().filter(|(_, t)| !t.is_empty()) {
                        known.entry(i as u32 + 1).or_default().extracted = Some(text.clone());
                        fresh.push(AttachmentPage { page: i as i64 + 1, source: PAGE_EXTRACTED, text });
                    }
                }
                // Текстовый слой не разобрался: страницы всё равно можно посчитать и распознать.
                _ => match aiowa_ocr::pdf_page_count(&data) {
                    Ok(n) if n > 0 => total = Some(n),
                    _ => return Ok(None),
                },
            },
            SourceKind::Image if mode == OcrMode::Off => total = Some(1),
            SourceKind::Image => match aiowa_ocr::Document::open(kind, &data) {
                Ok(doc) => {
                    total = Some(doc.page_count().max(1));
                    document = Some(doc);
                }
                Err(OcrError::Unavailable(why)) => {
                    notes.push(format!("Распознавание текста недоступно: {why}."));
                    return Ok(Some(Paged { pages_total: 1, pages: vec![PageOut { page: 1, source: PageSource::None, text: String::new() }], note: Some(notes.join(" ")), unavailable: true, ..Default::default() }));
                }
                Err(e) => return Err(PagesError::Failed(format!("Картинка не открылась: {e}"))),
            },
        }
        bytes = Some(data);
    }
    let total = total.unwrap_or(1);

    // 2. Какие страницы нужны.
    let wanted: Vec<u32> = match wanted {
        Some(list) => list.iter().copied().filter(|p| (1..=total).contains(p)).collect(),
        None => (1..=total).collect(),
    };
    if wanted.is_empty() {
        return Err(PagesError::Invalid(format!("Таких страниц нет: в файле всего {total} стр.")));
    }

    // 3. Распознавание страниц без текста: не больше предела за вызов, остальное — с пояснением.
    let todo: Vec<u32> = wanted.iter().copied().filter(|p| needs_ocr(known.get(p), mode, min_chars)).collect();
    let (mut left_pages, mut recognized_now, mut unavailable) = (Vec::new(), 0, false);
    if !todo.is_empty() {
        match file {
            None => notes.push(format!("Файл не скачан с сервера: страницы {} не распознаны.", ranges(&todo))),
            Some(path) => {
                if document.is_none() {
                    let data = match bytes.take() {
                        Some(data) => data,
                        None => read(path)?,
                    };
                    match aiowa_ocr::Document::open(kind, &data) {
                        Ok(doc) => document = Some(doc),
                        Err(OcrError::Unavailable(why)) => {
                            unavailable = true;
                            notes.push(format!("Распознавание текста недоступно: {why}."));
                        }
                        Err(e) => notes.push(format!("Файл не открылся для распознавания: {e}.")),
                    }
                }
                if let Some(doc) = &document {
                    let started = Instant::now();
                    let (mut done, mut failed, mut left) = (0usize, Vec::new(), Vec::new());
                    for (n, page) in todo.iter().copied().enumerate() {
                        if n >= OCR_PAGES_PER_CALL || started.elapsed() > OCR_TIME_BUDGET {
                            left = todo[n..].to_vec();
                            break;
                        }
                        match doc.recognize(page) {
                            Ok(text) => {
                                let text = clip_page(text);
                                known.entry(page).or_default().ocr = Some(text.clone());
                                fresh.push(AttachmentPage { page: i64::from(page), source: PAGE_OCR, text });
                                done += 1;
                            }
                            Err(e) => {
                                warn!(attachment_pk = pk, page, error = %e, "страница вложения не распознана");
                                failed.push(page);
                            }
                        }
                    }
                    info!(attachment_pk = pk, pages = done, failed = failed.len(), left = left.len(), language = doc.language(), ms = started.elapsed().as_millis() as u64, "страницы вложения распознаны");
                    if !failed.is_empty() {
                        notes.push(format!("Не удалось распознать страницы {}.", ranges(&failed)));
                    }
                    if !left.is_empty() {
                        notes.push(format!("За один вызов распознаётся не больше {OCR_PAGES_PER_CALL} страниц. Не распознаны страницы {}: запросите их параметром pages.", ranges(&left)));
                    }
                    (left_pages, recognized_now) = (left, done);
                }
            }
        }
    }

    // 4. В базу: новые страницы и, если что-то читается только распознаванием, текст целиком для поиска.
    if !fresh.is_empty() || cached_total != Some(i64::from(total)) {
        let by_ocr = known.values().any(|k| choose(k, OcrMode::Auto, min_chars).0 == PageSource::Ocr);
        let whole = by_ocr.then(|| (1..=total).filter_map(|p| known.get(&p)).map(|k| choose(k, OcrMode::Auto, min_chars).1).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n"));
        store.save_attachment_pages(pk, i64::from(total), &fresh, whole.as_deref()).map_err(db)?;
    }

    // 5. Ответ: текст запрошенных страниц.
    let pages = wanted
        .iter()
        .map(|page| {
            let (source, text) = known.get(page).map_or((PageSource::None, ""), |k| choose(k, mode, min_chars));
            PageOut { page: *page, source, text: text.to_string() }
        })
        .collect();
    Ok(Some(Paged { pages_total: total, pages, note: (!notes.is_empty()).then(|| notes.join(" ")), left: left_pages, recognized_now, unavailable }))
}

/// Файлы для тестов распознавания.
#[cfg(test)]
pub(crate) mod testing {
    /// PDF с надписями встроенным шрифтом, по одной на страницу: у страницы есть текстовый слой,
    /// а нарисованная, она читается распознаванием — как скан.
    pub(crate) fn text_pdf(pages: &[&str]) -> Vec<u8> {
        let mut objs: Vec<String> = vec!["<< /Type /Catalog /Pages 2 0 R >>".into()];
        let kids: String = (0..pages.len()).map(|i| format!("{} 0 R ", 4 + i * 2)).collect();
        objs.push(format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()));
        objs.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into());
        for (i, text) in pages.iter().enumerate() {
            objs.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", 5 + i * 2));
            let content = format!("BT /F1 22 Tf 40 700 Td ({text}) Tj ET");
            objs.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
        }
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
        }
        let xref = out.len();
        out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
        for off in offsets {
            out.push_str(&format!("{off:010} 00000 n \n"));
        }
        out.push_str(&format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1));
        out.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_lists_are_parsed() {
        assert_eq!(parse_pages("3").unwrap(), [3]);
        assert_eq!(parse_pages(" 1-5 ").unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(parse_pages("7, 1–3; 2").unwrap(), [1, 2, 3, 7]);
        for bad in ["", "0", "5-1", "a", "1-", "-3", "1-2-3", "1-99999"] {
            assert!(parse_pages(bad).is_err(), "{bad}");
        }
        assert_eq!(ranges(&[21, 22, 23, 40, 42, 43]), "21–23, 40, 42–43");
        assert_eq!(ranges(&[]), "");
    }

    #[test]
    fn page_text_is_chosen_by_mode() {
        let page = |extracted: Option<&str>, ocr: Option<&str>| Known { extracted: extracted.map(String::from), ocr: ocr.map(String::from) };
        let long = "Текст страницы длиннее порога в пятьдесят символов, это не скан.";
        // Страница с текстовым слоем: распознавать не нужно, отдаётся текст из файла.
        let text = page(Some(long), None);
        assert!(!needs_ocr(Some(&text), OcrMode::Auto, 50) && needs_ocr(Some(&text), OcrMode::Force, 50) && !needs_ocr(Some(&text), OcrMode::Off, 50));
        assert_eq!(choose(&text, OcrMode::Auto, 50), (PageSource::Extracted, long));
        // Скан: нужно распознать; распознанное отдаётся и в `auto`, и в `force`, но не в `off`.
        assert!(needs_ocr(None, OcrMode::Auto, 50) && !needs_ocr(None, OcrMode::Off, 50));
        let scan = page(Some("стр. 1"), Some("Уведомление о проверке"));
        assert!(!needs_ocr(Some(&scan), OcrMode::Auto, 50), "распознанное второй раз не распознаётся");
        assert_eq!(choose(&scan, OcrMode::Auto, 50), (PageSource::Ocr, "Уведомление о проверке"));
        assert_eq!(choose(&scan, OcrMode::Force, 50), (PageSource::Ocr, "Уведомление о проверке"));
        assert_eq!(choose(&scan, OcrMode::Off, 50), (PageSource::Extracted, "стр. 1"));
        // Распознано, но пусто (чистый лист): остаётся обрывок из файла или ничего.
        assert_eq!(choose(&page(Some("стр. 2"), Some(" ")), OcrMode::Auto, 50), (PageSource::Extracted, "стр. 2"));
        assert_eq!(choose(&page(None, Some("")), OcrMode::Auto, 50), (PageSource::None, ""));
        // Страница с текстом, распознанная принудительно: в `auto` всё равно текст из файла.
        let both = page(Some(long), Some("Распознано"));
        assert_eq!(choose(&both, OcrMode::Auto, 50), (PageSource::Extracted, long));
        assert_eq!(choose(&both, OcrMode::Force, 50), (PageSource::Ocr, "Распознано"));
    }

    #[test]
    fn answer_names_the_source_and_marks_pages() {
        let out = |pages: &[(u32, PageSource, &str)]| Paged { pages_total: 9, pages: pages.iter().map(|(page, source, text)| PageOut { page: *page, source: *source, text: (*text).into() }).collect(), note: None, ..Default::default() };
        let mixed = out(&[(1, PageSource::Extracted, "Титул"), (2, PageSource::Ocr, "Скан"), (3, PageSource::None, "")]);
        assert_eq!((mixed.text_source(), mixed.ocr_pages()), ("mixed", vec![2]));
        assert_eq!(mixed.text(), "--- стр. 1 ---\nТитул\n\n--- стр. 2 ---\nСкан\n\n--- стр. 3 ---\n(текста нет)");
        assert_eq!(out(&[(1, PageSource::Ocr, "Скан")]).text_source(), "ocr");
        assert_eq!(out(&[(4, PageSource::Extracted, "Текст")]).text_source(), "extracted");
        let empty = out(&[(1, PageSource::None, ""), (2, PageSource::None, "")]);
        assert_eq!((empty.text_source(), empty.text().as_str()), ("none", ""));
    }
}
