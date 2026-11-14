//! Разбор поискового запроса в духе OWA: операторы `от:`/`from:`, `кому:`/`to:`,
//! `переписка:`/`with:`, `тема:`/`subject:`, `вложение:`/`has:attachment`, `папка:`/`in:`,
//! `после:`/`до:`/`дата:` по датам, `прочитано:`, `флаг:`; остальной текст ищется по словам как
//! префиксы. Значения в кавычках: `от:"Иван Петров"`. Несколько адресов через запятую
//! (`от:a@x.ru,b@x.ru`) — любой из них. Запрос на обычном языке разбирает `nlquery` и сводит к
//! этим же условиям.

use crate::text::{fts_norm, norm_lower};
use jiff::civil::Date;
use jiff::tz::TimeZone;
use std::collections::BTreeMap;

/// Слово или фраза свободного текста.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    /// В кавычках: искать как фразу целиком, без префиксов.
    pub phrase: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub terms: Vec<Term>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    /// `от:имя@домен.зона` — точное равенство адреса отправителя (без учёта регистра), не поиск по словам:
    /// так работает «Найти письма» из карточки и выбор человека в подсказках поиска (п. 9 обкатки).
    pub from_exact: Vec<String>,
    /// `кому:имя@домен.зона` — адрес среди получателей письма (Кому, Копия, Скрытая); каждый
    /// адрес обязателен, как и остальные условия.
    pub to_exact: Vec<String>,
    pub subject: Vec<String>,
    pub has_attachment: bool,
    pub folder: Option<String>,
    /// Начало окна по дате получения, мс UTC, включительно.
    pub after_ms: Option<i64>,
    /// Конец окна по дате получения, мс UTC, не включительно.
    pub before_ms: Option<i64>,
    /// `кому:a@x.ru,b@x.ru` — любой адрес группы среди получателей; каждая группа обязательна.
    pub to_any: Vec<Vec<String>>,
    /// `переписка:адрес` — адрес группы отправителем или среди получателей; каждая группа обязательна.
    pub with_any: Vec<Vec<String>>,
    /// `переписка:слово` — слово в отправителе или получателях.
    pub with: Vec<String>,
    /// `прочитано:нет` — только непрочитанные (`Some(true)`), `прочитано:да` — только прочитанные.
    pub unread: Option<bool>,
    /// `флаг:да` — только с флагом, `флаг:нет` — без него.
    pub flagged: Option<bool>,
    /// `вложение:pdf` — вид вложения по расширению имени файла (любой из названных).
    pub attachment_kinds: Vec<AttachmentKind>,
    /// Другие формы слов запроса (`morph::forms`): слово в нижнем регистре → точные токены,
    /// которые ищутся вместе с ним. Заполняет `nlquery`; у слов в кавычках форм нет.
    pub forms: BTreeMap<String, Vec<String>>,
}

/// Вид вложения в запросе: «с таблицей», `вложение:pdf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Pdf,
    Sheet,
    Doc,
    Image,
    Archive,
    Slides,
}

impl AttachmentKind {
    /// Значение оператора `вложение:`.
    pub fn code(self) -> &'static str {
        match self {
            AttachmentKind::Pdf => "pdf",
            AttachmentKind::Sheet => "таблица",
            AttachmentKind::Doc => "документ",
            AttachmentKind::Image => "картинка",
            AttachmentKind::Archive => "архив",
            AttachmentKind::Slides => "презентация",
        }
    }

    /// Подпись чипа.
    pub fn label(self) -> &'static str {
        match self {
            AttachmentKind::Pdf => "PDF",
            AttachmentKind::Sheet => "таблица",
            AttachmentKind::Doc => "документ Word",
            AttachmentKind::Image => "картинка",
            AttachmentKind::Archive => "архив",
            AttachmentKind::Slides => "презентация",
        }
    }

    /// Расширения имён файлов этого вида, в нижнем регистре, с точкой.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            AttachmentKind::Pdf => &[".pdf"],
            AttachmentKind::Sheet => &[".xlsx", ".xls", ".xlsm", ".xlsb", ".csv", ".ods"],
            AttachmentKind::Doc => &[".docx", ".doc", ".rtf", ".odt"],
            AttachmentKind::Image => &[".jpg", ".jpeg", ".png", ".gif", ".bmp", ".tif", ".tiff", ".heic", ".webp"],
            AttachmentKind::Archive => &[".zip", ".rar", ".7z"],
            AttachmentKind::Slides => &[".pptx", ".ppt", ".odp"],
        }
    }

    /// Вид по значению оператора: `pdf`, `таблица`, `xlsx`, `excel`, `word`, `картинка`, `zip`…
    pub fn parse(value: &str) -> Option<Self> {
        Some(match norm_lower(value.trim()).as_str() {
            "pdf" | "пдф" => AttachmentKind::Pdf,
            "таблица" | "excel" | "эксель" | "xlsx" | "xls" | "csv" => AttachmentKind::Sheet,
            "документ" | "word" | "ворд" | "docx" | "doc" => AttachmentKind::Doc,
            "картинка" | "фото" | "изображение" | "jpg" | "jpeg" | "png" => AttachmentKind::Image,
            "архив" | "zip" | "rar" | "7z" => AttachmentKind::Archive,
            "презентация" | "pptx" | "ppt" => AttachmentKind::Slides,
            _ => return None,
        })
    }
}

/// Кусок запроса между пробелами: `ключ:значение`, слово или фраза в кавычках.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawToken {
    pub(crate) key: Option<String>,
    pub(crate) value: String,
    pub(crate) quoted: bool,
    /// Место в запросе, в символах: начало и конец (не включительно).
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Делит строку на токены по пробелам с учётом кавычек; `ключ:значение` распознаётся
/// по первому двоеточию, если ключ состоит из букв.
pub(crate) fn split_tokens(input: &str) -> Vec<RawToken> {
    let mut out = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let mut text = String::new();
        let mut quoted = false;
        let mut key: Option<String> = None;
        while i < chars.len() && !chars[i].is_whitespace() {
            let c = chars[i];
            if c == '"' || c == '«' || c == '“' {
                let close = match c {
                    '«' => '»',
                    '“' => '”',
                    _ => '"',
                };
                quoted = true;
                i += 1;
                while i < chars.len() && chars[i] != close {
                    text.push(chars[i]);
                    i += 1;
                }
                i += 1; // закрывающая кавычка
                continue;
            }
            if c == ':' && key.is_none() && !text.is_empty() && text.chars().all(char::is_alphabetic) {
                key = Some(text.to_lowercase());
                text.clear();
                i += 1;
                continue;
            }
            text.push(c);
            i += 1;
        }
        out.push(RawToken { key, value: text, quoted, start, end: i.min(chars.len()) });
    }
    out
}

/// Разбирает дату `YYYY-MM-DD`, `DD.MM.YYYY`, `YYYY-MM`, `YYYY`; возвращает начало
/// и конец (не включительно) периода в мс по системному часовому поясу.
fn parse_date_range(s: &str) -> Option<(i64, i64)> {
    let s = s.trim();
    let tz = TimeZone::system();
    let to_ms = |d: Date| -> Option<i64> { d.to_zoned(tz.clone()).ok().map(|z| z.timestamp().as_millisecond()) };
    let parts: Vec<&str> = if s.contains('.') { s.split('.').collect() } else { s.split('-').collect() };
    let nums: Option<Vec<i64>> = parts.iter().map(|p| p.parse::<i64>().ok()).collect();
    let nums = nums?;
    let (y, m, d) = match (s.contains('.'), nums.as_slice()) {
        (true, [d, m, y]) => (*y, Some(*m), Some(*d)),
        (true, [m, y]) => (*y, Some(*m), None),
        (false, [y, m, d]) => (*y, Some(*m), Some(*d)),
        (false, [y, m]) => (*y, Some(*m), None),
        (_, [y]) => (*y, None, None),
        _ => return None,
    };
    let y = if y < 100 { y + 2000 } else { y };
    let y = i16::try_from(y).ok()?;
    match (m, d) {
        (Some(m), Some(d)) => {
            let start = Date::new(y, i8::try_from(m).ok()?, i8::try_from(d).ok()?).ok()?;
            Some((to_ms(start)?, to_ms(start.tomorrow().ok()?)?))
        }
        (Some(m), None) => {
            let start = Date::new(y, i8::try_from(m).ok()?, 1).ok()?;
            let end = if m == 12 { Date::new(y + 1, 1, 1).ok()? } else { Date::new(y, i8::try_from(m + 1).ok()?, 1).ok()? };
            Some((to_ms(start)?, to_ms(end)?))
        }
        (None, _) => {
            let start = Date::new(y, 1, 1).ok()?;
            Some((to_ms(start)?, to_ms(Date::new(y + 1, 1, 1).ok()?)?))
        }
    }
}

/// Полный адрес `имя@домен.зона`: только он ищется точным равенством. Часть адреса (`@домен.ru`,
/// `ivan@`, недонабранное `ivan@comp`) остаётся поиском по словам, как раньше.
fn is_full_address(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else { return false };
    !local.is_empty() && !domain.contains('@') && !s.chars().any(char::is_whitespace) && domain.contains('.') && domain.split('.').all(|p| !p.is_empty())
}

/// Список полных адресов через запятую (`a@x.ru,b@x.ru`), в нижнем регистре; `None` — в значении
/// есть что-то кроме полных адресов.
fn address_list(value: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = value.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    (!parts.is_empty() && parts.iter().all(|p| is_full_address(p))).then(|| parts.iter().map(|p| p.to_lowercase()).collect())
}

fn yes_no(value: &str) -> Option<bool> {
    match norm_lower(value.trim()).as_str() {
        "да" | "yes" | "1" | "true" | "есть" => Some(true),
        "нет" | "no" | "0" | "false" => Some(false),
        _ => None,
    }
}

/// Применить один кусок запроса: оператор, слово или фразу.
pub(crate) fn apply_token(q: &mut SearchQuery, t: &RawToken) {
    let value = fts_norm(t.value.trim());
    match t.key.as_deref() {
        Some("от" | "from") if !value.is_empty() => match address_list(&t.value) {
            Some(list) => q.from_exact.extend(list),
            None => q.from.push(value),
        },
        Some("кому" | "to") if !value.is_empty() => match address_list(&t.value) {
            Some(mut list) if list.len() == 1 => q.to_exact.append(&mut list),
            Some(list) => q.to_any.push(list),
            None => q.to.push(value),
        },
        Some("переписка" | "with") if !value.is_empty() => match address_list(&t.value) {
            Some(list) => q.with_any.push(list),
            None => q.with.push(value),
        },
        Some("тема" | "subject") if !value.is_empty() => q.subject.push(value),
        Some("папка" | "in" | "folder") if !value.is_empty() => q.folder = Some(value),
        Some("вложение" | "вложения" | "attachment") => {
            q.has_attachment = !matches!(value.to_lowercase().as_str(), "нет" | "no" | "0" | "false");
            if let Some(kind) = AttachmentKind::parse(&value)
                && !q.attachment_kinds.contains(&kind)
            {
                q.attachment_kinds.push(kind);
            }
        }
        Some("has" | "есть") => {
            if matches!(value.to_lowercase().as_str(), "attachment" | "attachments" | "вложение" | "вложения" | "файл") {
                q.has_attachment = true;
            }
        }
        Some("после" | "after" | "since" | "с") => {
            if let Some((start, _)) = parse_date_range(&value) {
                q.after_ms = Some(start);
            }
        }
        Some("до" | "before" | "по") => {
            if let Some((_, end)) = parse_date_range(&value) {
                q.before_ms = Some(end);
            }
        }
        Some("дата" | "date") => {
            if let Some((start, end)) = parse_date_range(&value) {
                q.after_ms = Some(start);
                q.before_ms = Some(end);
            }
        }
        Some("прочитано" | "isread" | "read") => {
            if let Some(read) = yes_no(&value) {
                q.unread = Some(!read);
            }
        }
        Some("флаг" | "isflagged" | "flag") => q.flagged = Some(yes_no(&value).unwrap_or(true)),
        Some(k) => {
            // Неизвестный оператор: ищем как текст вместе с ключом.
            let text = format!("{k} {value}");
            if !text.trim().is_empty() {
                q.terms.push(Term { text: text.trim().to_string(), phrase: t.quoted });
            }
        }
        None => {
            if !value.is_empty() {
                q.terms.push(Term { text: value, phrase: t.quoted });
            }
        }
    }
}

pub fn parse(input: &str) -> SearchQuery {
    let mut q = SearchQuery::default();
    for t in split_tokens(input) {
        apply_token(&mut q, &t);
    }
    q
}

/// Слова текста для FTS5: служебные символы отрезаются по краям.
pub(crate) fn plain_words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '*' | '^'))
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
}

fn quoted(word: &str, prefix: bool) -> String {
    if prefix { format!("\"{}\"*", word.replace('"', "\"\"")) } else { format!("\"{}\"", word.replace('"', "\"\"")) }
}

/// Слово как префиксный токен FTS5 в кавычках.
fn fts_words(text: &str, prefix: bool) -> Vec<String> {
    plain_words(text).map(|w| quoted(w, prefix)).collect()
}

impl SearchQuery {
    /// Есть ли хоть одно условие.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.from.is_empty()
            && self.to.is_empty()
            && self.from_exact.is_empty()
            && self.to_exact.is_empty()
            && self.subject.is_empty()
            && !self.has_attachment
            && self.folder.is_none()
            && self.after_ms.is_none()
            && self.before_ms.is_none()
            && self.to_any.is_empty()
            && self.with_any.is_empty()
            && self.with.is_empty()
            && self.unread.is_none()
            && self.flagged.is_none()
    }

    /// Слова текста как префиксы; у слова с известными формами — «слово или любая его форма».
    fn expanded_words(&self, text: &str) -> Vec<String> {
        plain_words(text)
            .map(|w| match self.forms.get(&norm_lower(w)).filter(|f| !f.is_empty()) {
                Some(forms) => format!("({} OR {})", quoted(w, true), forms.iter().map(|f| quoted(f, false)).collect::<Vec<_>>().join(" OR ")),
                None => quoted(w, true),
            })
            .collect()
    }

    /// Выражения FTS5 для слов и фраз свободного текста, по одному на слово или фразу.
    fn term_parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        for t in &self.terms {
            if t.phrase {
                let words = fts_words(&t.text, false);
                if !words.is_empty() {
                    // Фраза: слова подряд. FTS5: "a" + "b".
                    parts.push(words.join(" + "));
                }
            } else {
                parts.extend(self.expanded_words(&t.text));
            }
        }
        parts
    }

    /// Выражение `MATCH`: хотя бы одно слово или фраза свободного текста — в именах или тексте
    /// вложений письма. `None`, если слов нет.
    pub fn attachment_match(&self) -> Option<String> {
        let parts = self.term_parts();
        (!parts.is_empty()).then(|| format!("attachment_text: ({})", parts.join(" OR ")))
    }

    /// Выражение `MATCH` для FTS5 или `None`, если текстовых условий нет (остались только фильтры).
    pub fn fts_match(&self) -> Option<String> {
        let mut parts = self.term_parts();
        for (cols, values) in [("{from_name from_email}", &self.from), ("recipients", &self.to), ("{from_name from_email recipients}", &self.with)] {
            for v in values {
                let words = fts_words(v, true);
                if !words.is_empty() {
                    parts.push(format!("{cols}: ({})", words.join(" ")));
                }
            }
        }
        for v in &self.subject {
            let words = self.expanded_words(v);
            if !words.is_empty() {
                parts.push(format!("subject: ({})", words.join(" ")));
            }
        }
        // Между фразами и выражениями в скобках FTS5 требует явный AND.
        if parts.is_empty() { None } else { Some(parts.join(" AND ")) }
    }
}

/// Запрос без операторов, каждое слово как префикс (все слова обязательны) — проверка `fts_words`.
#[cfg(test)]
fn fts_query(user_query: &str) -> String {
    fts_words(&fts_norm(user_query), true).join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_query() {
        assert_eq!(fts_query("запчасти Колыма"), "\"запчасти\"* \"Колыма\"*");
        assert_eq!(fts_query("a (b) \"c\""), "\"a\"* \"b\"* \"c\"*");
        assert_eq!(fts_query("счёт №25"), "\"счет\"* \"25\"*");
    }

    #[test]
    fn parses_operators() {
        let q = parse("от:иванов кому:\"Пётр Сидоров\" тема:счёт has:attachment папка:Входящие после:2026-10-01 до:15.10.2026 запчасти");
        assert_eq!(q.from, vec!["иванов"]);
        assert_eq!(q.to, vec!["Петр Сидоров"]);
        assert_eq!(q.subject, vec!["счет"]);
        assert!(q.has_attachment);
        assert_eq!(q.folder.as_deref(), Some("Входящие"));
        assert!(q.after_ms.is_some() && q.before_ms.is_some() && q.after_ms < q.before_ms);
        assert_eq!(q.terms, vec![Term { text: "запчасти".into(), phrase: false }]);
        assert!(q.from_exact.is_empty() && q.to_exact.is_empty());
        let exact = parse("от:Ivan.Petrov@Example.ru кому:anna@x.ru отчёт");
        assert_eq!(exact.from_exact, vec!["ivan.petrov@example.ru"]);
        assert_eq!(exact.to_exact, vec!["anna@x.ru"]);
        assert!(exact.from.is_empty() && exact.to.is_empty(), "адрес не уходит в поиск по словам");
        assert_eq!(exact.fts_match().as_deref(), Some("\"отчет\"*"));
        assert!(!parse("от:ivan@x.ru").is_empty() && parse("от:ivan@x.ru").fts_match().is_none(), "одно точное условие — без FTS");
        let m = q.fts_match().unwrap();
        assert_eq!(m, "\"запчасти\"* AND {from_name from_email}: (\"иванов\"*) AND recipients: (\"Петр\"* \"Сидоров\"*) AND subject: (\"счет\"*)");
    }

    #[test]
    fn filters_only_and_phrases() {
        let q = parse("вложение: после:2026");
        assert!(q.has_attachment && q.after_ms.is_some() && q.fts_match().is_none() && !q.is_empty());
        let q = parse("\"точная фраза\" слово");
        assert_eq!(q.fts_match().unwrap(), "\"точная\" + \"фраза\" AND \"слово\"*");
        assert!(parse("   ").is_empty());
        // Неизвестный оператор не теряется, а ищется как текст.
        let q = parse("http:сайт");
        assert_eq!(q.terms[0].text, "http сайт");
        // Email целиком в одном токене: адрес — точное условие, не поиск по словам (п. 9 обкатки).
        let q = parse("from:ivan@x.ru");
        assert_eq!(q.from_exact, vec!["ivan@x.ru"]);
        assert!(q.fts_match().is_none() && !q.is_empty());
    }

    #[test]
    fn partial_address_is_word_search() {
        // Домен, начало адреса или недонабранный адрес — поиск по словам, как до точных условий.
        let q = parse("от:@company.ru");
        assert!(q.from_exact.is_empty());
        assert_eq!(q.fts_match().as_deref(), Some("{from_name from_email}: (\"company.ru\"*)"));
        let q = parse("от:ivan@comp кому:anna@ кому:@x.ru");
        assert!(q.from_exact.is_empty() && q.to_exact.is_empty());
        assert_eq!(q.from, vec!["ivan@comp"]);
        assert_eq!(q.to, vec!["anna@", "@x.ru"]);
        for partial in ["ivan@x.", "ivan@.ru", "a@b@x.ru", "\"ivan @x.ru\""] {
            assert!(parse(&format!("кому:{partial}")).to_exact.is_empty(), "{partial}");
        }
        // Несколько полных адресов в кому: — каждый отдельным условием (все обязательны).
        let q = parse("кому:a@x.ru кому:B@x.ru");
        assert_eq!(q.to_exact, vec!["a@x.ru", "b@x.ru"]);
        assert!(q.to.is_empty() && q.fts_match().is_none());
    }

    #[test]
    fn address_lists_and_new_operators() {
        // Несколько адресов через запятую — любой из них.
        let q = parse("от:a@x.ru,B@x.ru кому:c@x.ru,d@x.ru кому:e@x.ru переписка:f@x.ru,g@x.ru переписка:иванов");
        assert_eq!(q.from_exact, vec!["a@x.ru", "b@x.ru"]);
        assert_eq!(q.to_any, vec![vec!["c@x.ru".to_string(), "d@x.ru".to_string()]]);
        assert_eq!(q.to_exact, vec!["e@x.ru"]);
        assert_eq!(q.with_any, vec![vec!["f@x.ru".to_string(), "g@x.ru".to_string()]]);
        assert_eq!(q.with, vec!["иванов"]);
        assert_eq!(q.fts_match().as_deref(), Some("{from_name from_email recipients}: (\"иванов\"*)"));
        // Список, где не всё адреса, — поиск по словам, как раньше.
        assert_eq!(parse("от:a@x.ru,иванов").from, vec!["a@x.ru,иванов"]);
        // Состояние письма и вид вложения.
        let q = parse("прочитано:нет флаг:да вложение:pdf вложение:таблица");
        assert_eq!((q.unread, q.flagged, q.has_attachment), (Some(true), Some(true), true));
        assert_eq!(q.attachment_kinds, vec![AttachmentKind::Pdf, AttachmentKind::Sheet]);
        assert!(!q.is_empty() && q.fts_match().is_none());
        assert_eq!(parse("isread:yes").unread, Some(false));
        assert_eq!(parse("флаг:нет").flagged, Some(false));
        assert!(parse("вложение:нет").attachment_kinds.is_empty() && !parse("вложение:нет").has_attachment);
        // `дата:` — обе границы разом.
        let q = parse("дата:2026-10");
        assert_eq!((q.after_ms, q.before_ms), parse_date_range("2026-10").map(|(a, b)| (Some(a), Some(b))).unwrap());
        // Место кусков в запросе — в символах.
        let tokens = split_tokens("от:иванов  «точная фраза» слово");
        assert_eq!(tokens.iter().map(|t| (t.start, t.end)).collect::<Vec<_>>(), vec![(0, 9), (11, 25), (26, 31)]);
    }

    #[test]
    fn word_forms_in_match() {
        let mut q = parse("договора тема:поставки \"точная фраза\"");
        q.forms.insert("договора".into(), vec!["договор".into(), "договору".into()]);
        q.forms.insert("поставки".into(), vec!["поставка".into()]);
        q.forms.insert("точная".into(), vec!["точный".into()]);
        assert_eq!(q.fts_match().unwrap(), "(\"договора\"* OR \"договор\" OR \"договору\") AND \"точная\" + \"фраза\" AND subject: ((\"поставки\"* OR \"поставка\"))");
        assert_eq!(q.attachment_match().unwrap(), "attachment_text: ((\"договора\"* OR \"договор\" OR \"договору\") OR \"точная\" + \"фраза\")");
        assert_eq!(parse("от:иванов вложение:").attachment_match(), None);
    }

    #[test]
    fn date_ranges() {
        // Границы — полночь по системному поясу: в поясе с летним временем октябрь длиннее
        // 31 суток на час, поэтому ожидания считаем в том же поясе, а не умножением суток.
        let tz = TimeZone::system();
        let ms = |y, m, d| Date::new(y, m, d).unwrap().to_zoned(tz.clone()).unwrap().timestamp().as_millisecond();
        assert_eq!(parse_date_range("2026-10"), Some((ms(2026, 10, 1), ms(2026, 11, 1))));
        assert_eq!(parse_date_range("12.2026"), Some((ms(2026, 12, 1), ms(2027, 1, 1))));
        assert_eq!(parse_date_range("2025"), Some((ms(2025, 1, 1), ms(2026, 1, 1))));
        assert_eq!(parse_date_range("31.12.2026"), Some((ms(2026, 12, 31), ms(2027, 1, 1))));
        assert_eq!(parse_date_range("2026-03-29"), Some((ms(2026, 3, 29), ms(2026, 3, 30))));
        assert!(parse_date_range("вчера").is_none());
    }
}
