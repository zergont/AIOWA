//! Текстовые утилиты хранилища: выжимка текста из HTML для индекса, нормализация
//! для поиска (ё→е), тема беседы без префиксов ответов и пересылок.
//! Это не рендер писем (им займётся интерфейс), а минимально честная очистка для поиска.

/// Убирает теги, скрипты и стили, раскрывает частые сущности, схлопывает пробелы.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut chars = html.char_indices().peekable();
    let lower = html.to_ascii_lowercase();
    let mut skip_until: Option<usize> = None;

    while let Some((i, c)) = chars.next() {
        if let Some(end) = skip_until {
            if i < end {
                continue;
            }
            skip_until = None;
        }
        if c == '<' {
            let rest = &lower[i..];
            for (open, close) in [("<script", "</script>"), ("<style", "</style>"), ("<head", "</head>"), ("<!--", "-->")] {
                if rest.starts_with(open) {
                    if let Some(off) = rest.find(close) {
                        skip_until = Some(i + off + close.len());
                    } else {
                        skip_until = Some(html.len());
                    }
                    break;
                }
            }
            if skip_until.is_some() {
                continue;
            }
            // Блочные теги превращаем в перевод строки, ячейки таблиц в пробел, строчные теги не оставляют следа.
            let block = ["<p", "<div", "<br", "<li", "<tr", "<h1", "<h2", "<h3", "<h4", "<h5", "<h6", "<blockquote", "<table", "<pre", "</p", "</div", "</li", "</tr", "</h", "</pre"];
            let cell = ["<td", "<th", "</td", "</th", "<img", "<hr"];
            if block.iter().any(|b| rest.starts_with(b)) {
                out.push('\n');
            } else if cell.iter().any(|b| rest.starts_with(b)) {
                out.push(' ');
            }
            // пропускаем до '>'
            if let Some(off) = rest.find('>') {
                skip_until = Some(i + off + 1);
            } else {
                skip_until = Some(html.len());
            }
            continue;
        }
        if c == '&' {
            let rest = &html[i..];
            let semi = rest.find(';').filter(|p| *p <= 10);
            if let Some(p) = semi {
                let ent = &rest[1..p];
                let decoded = match ent {
                    "nbsp" => Some(' '),
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" | "#39" => Some('\''),
                    "laquo" => Some('«'),
                    "raquo" => Some('»'),
                    "mdash" => Some('—'),
                    "ndash" => Some('–'),
                    "hellip" => Some('…'),
                    _ => ent.strip_prefix('#').and_then(|n| {
                        if let Some(h) = n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
                            u32::from_str_radix(h, 16).ok()
                        } else {
                            n.parse::<u32>().ok()
                        }
                        .and_then(char::from_u32)
                    }),
                };
                if let Some(d) = decoded {
                    out.push(d);
                    skip_until = Some(i + p + 1);
                    continue;
                }
            }
        }
        out.push(c);
    }
    normalize_ws(&out)
}

/// Схлопывает пробелы внутри строк и убирает пустые строки.
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

/// Нормализация текста для индекса: ё→е, Ё→Е. Регистр не трогаем (его сворачивает
/// токенизатор), чтобы сниппеты читались как оригинал. Та же функция применяется к
/// запросу, поэтому «счёт» и «счет» находят друг друга.
pub fn fts_norm(s: &str) -> String {
    if !s.contains(['ё', 'Ё']) {
        return s.to_string();
    }
    s.chars().map(|c| match c {
        'ё' => 'е',
        'Ё' => 'Е',
        c => c,
    })
    .collect()
}

/// Нормализация для сравнений: нижний регистр Юникода плюс ё→е.
pub fn norm_lower(s: &str) -> String {
    fts_norm(&s.to_lowercase())
}

/// Тема беседы: тема письма без префиксов ответов и пересылок (`RE:`, `FW:`, `Отв:`, `RE[2]:`).
pub fn topic_of(subject: &str) -> String {
    const PREFIXES: &[&str] = &["re:", "fw:", "fwd:", "tr:", "aw:", "wg:", "sv:", "vs:", "отв:", "ответ:", "пер:", "re :", "fw :", "fwd :"];
    let mut s = subject.trim();
    loop {
        let mut stripped = false;
        for p in PREFIXES {
            if s.get(..p.len()).map(|h| h.to_lowercase() == *p).unwrap_or(false) {
                s = s[p.len()..].trim_start();
                stripped = true;
                break;
            }
        }
        if !stripped {
            // Нумерованные: RE[2]:, Re(3):, FW[12]:
            let lower2: String = s.chars().take(2).collect::<String>().to_lowercase();
            if matches!(lower2.as_str(), "re" | "fw") {
                let rest = &s[lower2.len()..];
                let open = rest.chars().next();
                if matches!(open, Some('[') | Some('(')) {
                    let close = if open == Some('[') { ']' } else { ')' };
                    if let Some(end) = rest.find(close) {
                        let digits = &rest[1..end];
                        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) && rest[end + 1..].starts_with(':') {
                            s = rest[end + 2..].trim_start();
                            stripped = true;
                        }
                    }
                }
            }
        }
        if !stripped {
            break;
        }
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html() {
        let t = html_to_text("<html><head><style>p{}</style></head><body><p>Привет,&nbsp;<b>мир</b>!</p><script>x()</script><div>Строка&nbsp;2 &laquo;да&raquo;</div></body></html>");
        assert_eq!(t, "Привет, мир!\nСтрока 2 «да»");
    }

    #[test]
    fn normalizes_yo() {
        assert_eq!(fts_norm("Счёт и ЁЛКА"), "Счет и ЕЛКА");
        assert_eq!(fts_norm("plain"), "plain");
        assert_eq!(norm_lower("Счёт №25"), "счет №25");
    }

    #[test]
    fn topics() {
        assert_eq!(topic_of("RE: FW: Счет"), "Счет");
        assert_eq!(topic_of("Отв: тема"), "тема");
        assert_eq!(topic_of("RE[2]: Re(3): тема"), "тема");
        assert_eq!(topic_of("Regular"), "Regular");
        assert_eq!(topic_of("  "), "");
    }
}
