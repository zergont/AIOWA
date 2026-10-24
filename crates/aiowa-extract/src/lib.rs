//! Извлечение текста из вложений для поискового индекса. Чистые функции над байтами,
//! без ввода-вывода и без OCR: PDF с текстовым слоем, DOCX, XLSX/XLS, TXT/CSV.
//! Остальные типы (картинки, архивы, сканы) считаются неподдерживаемыми и пропускаются.

use std::io::{Cursor, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Тип вложения по имени файла и MIME-типу.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    Docx,
    /// XLSX, XLSM, XLS, ODS: всё, что читает calamine.
    Spreadsheet,
    /// TXT, CSV, MD, LOG и прочий простой текст.
    PlainText,
    Unsupported,
}

/// Итог извлечения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extraction {
    /// Извлечённый текст (может быть пустым, если в файле только картинки).
    Text(String),
    /// Тип не поддерживается: текста нет и не будет.
    Unsupported,
    /// Файл поддерживаемого типа, но разобрать не удалось.
    Failed(String),
}

/// Предел текста по умолчанию, символов: больше в индекс не нужно.
pub const DEFAULT_MAX_CHARS: usize = 2_000_000;

pub fn kind_of(name: Option<&str>, content_type: Option<&str>) -> Kind {
    let ext = name
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let ct = content_type.map(|c| c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "pdf" => return Kind::Pdf,
        "docx" | "docm" | "dotx" => return Kind::Docx,
        "xlsx" | "xlsm" | "xls" | "xlsb" | "ods" => return Kind::Spreadsheet,
        "txt" | "csv" | "tsv" | "md" | "log" | "json" | "xml" | "ini" | "cfg" => return Kind::PlainText,
        _ => {}
    }
    match ct.as_str() {
        "application/pdf" => Kind::Pdf,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Kind::Docx,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" | "application/vnd.ms-excel" | "application/vnd.oasis.opendocument.spreadsheet" => Kind::Spreadsheet,
        "text/plain" | "text/csv" | "text/markdown" => Kind::PlainText,
        _ => Kind::Unsupported,
    }
}

/// Извлекает текст по типу. Панику библиотек разбора превращает в `Failed`.
pub fn extract(kind: Kind, bytes: &[u8], max_chars: usize) -> Extraction {
    let result = catch_unwind(AssertUnwindSafe(|| match kind {
        Kind::Pdf => pdf(bytes),
        Kind::Docx => docx(bytes),
        Kind::Spreadsheet => spreadsheet(bytes),
        Kind::PlainText => Ok(plain_text(bytes)),
        Kind::Unsupported => return Extraction::Unsupported,
    }
    .map(|t| Extraction::Text(limit_chars(normalize_ws(&t), max_chars)))
    .unwrap_or_else(Extraction::Failed)));
    match result {
        Ok(e) => e,
        Err(_) => Extraction::Failed("паника в библиотеке разбора".into()),
    }
}

/// Удобная обёртка: тип определяется по имени и MIME.
pub fn extract_text(name: Option<&str>, content_type: Option<&str>, bytes: &[u8], max_chars: usize) -> Extraction {
    extract(kind_of(name, content_type), bytes, max_chars)
}

fn limit_chars(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    s.chars().take(max).collect()
}

/// Схлопывает пробелы внутри строк, убирает пустые строки.
pub fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&collapsed);
    }
    out
}

// ---------------------------------------------------------------------------

fn pdf(bytes: &[u8]) -> Result<String, String> {
    pdf_extract::extract_text_from_mem(bytes).map_err(|e| format!("pdf: {e}"))
}

fn docx(bytes: &[u8]) -> Result<String, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("docx zip: {e}"))?;
    let mut xml = Vec::new();
    zip.by_name("word/document.xml").map_err(|e| format!("docx: нет word/document.xml ({e})"))?.read_to_end(&mut xml).map_err(|e| format!("docx: {e}"))?;
    Ok(wordprocessing_text(&xml))
}

/// Текст из `word/document.xml`: `w:t` → текст, `w:p`/`w:br`/`w:tab` → разделители.
fn wordprocessing_text(xml: &[u8]) -> String {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let local = name.as_ref();
                match local {
                    "w:t" => in_text = true,
                    "w:tab" => out.push(' '),
                    "w:br" | "w:cr" => out.push('\n'),
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                "w:tab" => out.push(' '),
                "w:br" | "w:cr" => out.push('\n'),
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                "w:t" => in_text = false,
                "w:p" | "w:tr" => out.push('\n'),
                "w:tc" => out.push(' '),
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => out.push_str(t.as_ref()),
            Ok(Event::GeneralRef(r)) if in_text => {
                // Ссылки на сущности приходят отдельными событиями: &amp;, &#1057; и т. п.
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    out.push(c);
                } else if let Some(s) = quick_xml::escape::resolve_predefined_entity(r.as_ref()) {
                    out.push_str(s);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

fn spreadsheet(bytes: &[u8]) -> Result<String, String> {
    use calamine::{open_workbook_auto_from_rs, Data, Reader};
    let mut wb = open_workbook_auto_from_rs(Cursor::new(bytes)).map_err(|e| format!("таблица: {e}"))?;
    let names: Vec<String> = wb.sheet_names().to_vec();
    let mut out = String::new();
    for name in names {
        let range = match wb.worksheet_range(&name) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "лист не прочитан");
                continue;
            }
        };
        if range.is_empty() {
            continue;
        }
        out.push_str(&name);
        out.push('\n');
        for row in range.rows() {
            let mut line = String::new();
            for cell in row {
                let s = match cell {
                    Data::Empty => continue,
                    Data::String(s) => s.clone(),
                    Data::Float(f) => {
                        if f.fract() == 0.0 && f.abs() < 1e15 { format!("{}", *f as i64) } else { f.to_string() }
                    }
                    Data::Int(i) => i.to_string(),
                    Data::Bool(b) => b.to_string(),
                    Data::DateTime(d) => d.to_string(),
                    Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
                    Data::Error(_) => continue,
                };
                if s.trim().is_empty() {
                    continue;
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(s.trim());
            }
            if !line.is_empty() {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    Ok(out)
}

/// Простой текст: BOM UTF-8/UTF-16, иначе UTF-8, иначе windows-1251 (русская почта).
fn plain_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        let (cow, _, _) = if bytes.starts_with(&[0xFF, 0xFE]) { encoding_rs::UTF_16LE.decode(&bytes[2..]) } else { encoding_rs::UTF_16BE.decode(&bytes[2..]) };
        return cow.into_owned();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (cow, _, _) = encoding_rs::WINDOWS_1251.decode(bytes);
    cow.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn kinds() {
        assert_eq!(kind_of(Some("Счёт №25.pdf"), None), Kind::Pdf);
        assert_eq!(kind_of(Some("x.XLSX"), None), Kind::Spreadsheet);
        assert_eq!(kind_of(Some("x.bin"), Some("application/pdf")), Kind::Pdf);
        assert_eq!(kind_of(Some("photo.jpg"), Some("image/jpeg")), Kind::Unsupported);
        assert_eq!(kind_of(None, Some("text/plain; charset=utf-8")), Kind::PlainText);
    }

    #[test]
    fn plain_text_encodings() {
        assert_eq!(extract(Kind::PlainText, "привет  мир\n\n".as_bytes(), 100), Extraction::Text("привет мир".into()));
        let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode("счёт оплачен");
        assert_eq!(extract(Kind::PlainText, &cp1251, 100), Extraction::Text("счёт оплачен".into()));
        let mut utf16 = vec![0xFF, 0xFE];
        for u in "да".encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(extract(Kind::PlainText, &utf16, 100), Extraction::Text("да".into()));
    }

    #[test]
    fn docx_minimal() {
        let doc = r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
            <w:p><w:r><w:t>Договор</w:t></w:r><w:r><w:t xml:space="preserve"> поставки</w:t></w:r></w:p>
            <w:p><w:r><w:t>Сумма &amp; срок</w:t></w:r></w:p></w:body></w:document>"#;
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            z.start_file("word/document.xml", opts).unwrap();
            z.write_all(doc.as_bytes()).unwrap();
            z.finish().unwrap();
        }
        assert_eq!(extract(Kind::Docx, buf.get_ref(), 1000), Extraction::Text("Договор поставки\nСумма & срок".into()));
    }

    #[test]
    fn unsupported_and_garbage() {
        assert_eq!(extract(Kind::Unsupported, b"xx", 10), Extraction::Unsupported);
        assert!(matches!(extract(Kind::Pdf, b"not a pdf", 10), Extraction::Failed(_)));
        assert!(matches!(extract(Kind::Docx, b"not a zip", 10), Extraction::Failed(_)));
        assert!(matches!(extract(Kind::Spreadsheet, b"not xlsx", 10), Extraction::Failed(_)));
    }

    #[test]
    fn limits_text() {
        let long = "а".repeat(50);
        assert_eq!(extract(Kind::PlainText, long.as_bytes(), 10), Extraction::Text("а".repeat(10)));
    }
}
