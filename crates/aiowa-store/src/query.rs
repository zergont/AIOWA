//! Разбор поискового запроса в духе OWA: операторы `от:`/`from:`, `кому:`/`to:`,
//! `тема:`/`subject:`, `вложение:`/`has:attachment`, `папка:`/`in:`, `после:`/`до:` по датам;
//! остальной текст ищется по словам как префиксы. Значения в кавычках: `от:"Иван Петров"`.

use crate::text::fts_norm;
use jiff::civil::Date;
use jiff::tz::TimeZone;

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
    /// `от:адрес@…` — точное равенство адреса отправителя (без учёта регистра), не поиск по словам:
    /// так работает «Найти письма» из карточки и выбор человека в подсказках поиска (п. 9 обкатки).
    pub from_exact: Vec<String>,
    /// `кому:адрес@…` — адрес среди получателей письма (Кому, Копия, Скрытая).
    pub to_exact: Vec<String>,
    pub subject: Vec<String>,
    pub has_attachment: bool,
    pub folder: Option<String>,
    /// Начало окна по дате получения, мс UTC, включительно.
    pub after_ms: Option<i64>,
    /// Конец окна по дате получения, мс UTC, не включительно.
    pub before_ms: Option<i64>,
}

struct RawToken {
    key: Option<String>,
    value: String,
    quoted: bool,
}

/// Делит строку на токены по пробелам с учётом кавычек; `ключ:значение` распознаётся
/// по первому двоеточию, если ключ состоит из букв.
fn split_tokens(input: &str) -> Vec<RawToken> {
    let mut out = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
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
        out.push(RawToken { key, value: text, quoted });
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

pub fn parse(input: &str) -> SearchQuery {
    let mut q = SearchQuery::default();
    for t in split_tokens(input) {
        let value = fts_norm(t.value.trim());
        match t.key.as_deref() {
            Some("от" | "from") if !value.is_empty() => {
                if t.value.contains('@') {
                    q.from_exact.push(t.value.trim().to_lowercase());
                } else {
                    q.from.push(value);
                }
            }
            Some("кому" | "to") if !value.is_empty() => {
                if t.value.contains('@') {
                    q.to_exact.push(t.value.trim().to_lowercase());
                } else {
                    q.to.push(value);
                }
            }
            Some("тема" | "subject") if !value.is_empty() => q.subject.push(value),
            Some("папка" | "in" | "folder") if !value.is_empty() => q.folder = Some(value),
            Some("вложение" | "вложения" | "attachment") => q.has_attachment = !matches!(value.to_lowercase().as_str(), "нет" | "no" | "0" | "false"),
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
    q
}

/// Слово как префиксный токен FTS5 в кавычках; служебные символы отрезаются по краям.
fn fts_words(text: &str, prefix: bool) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '*' | '^'))
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(|w| if prefix { format!("\"{}\"*", w.replace('"', "\"\"")) } else { format!("\"{}\"", w.replace('"', "\"\"")) })
        .collect()
}

impl SearchQuery {
    /// Есть ли хоть одно условие.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.from.is_empty() && self.to.is_empty() && self.from_exact.is_empty() && self.to_exact.is_empty() && self.subject.is_empty() && !self.has_attachment && self.folder.is_none() && self.after_ms.is_none() && self.before_ms.is_none()
    }

    /// Выражение `MATCH` для FTS5 или `None`, если текстовых условий нет (остались только фильтры).
    pub fn fts_match(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        for t in &self.terms {
            if t.phrase {
                let words = fts_words(&t.text, false);
                if !words.is_empty() {
                    // Фраза: слова подряд. FTS5: "a" + "b".
                    parts.push(words.join(" + "));
                }
            } else {
                parts.extend(fts_words(&t.text, true));
            }
        }
        for (cols, values) in [("{from_name from_email}", &self.from), ("recipients", &self.to), ("subject", &self.subject)] {
            for v in values {
                let words = fts_words(v, true);
                if !words.is_empty() {
                    parts.push(format!("{cols}: ({})", words.join(" ")));
                }
            }
        }
        // Между фразами и выражениями в скобках FTS5 требует явный AND.
        if parts.is_empty() { None } else { Some(parts.join(" AND ")) }
    }
}

/// Совместимость: запрос без операторов, каждое слово как префикс (все слова обязательны).
pub fn fts_query(user_query: &str) -> String {
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
