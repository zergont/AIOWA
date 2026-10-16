//! Текстовые утилиты хранилища: выжимка текста из HTML для индекса и сборка FTS-запроса.
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

/// Запрос FTS5: каждое слово как префикс в кавычках, слова объединяются по И.
/// Слова короче двух символов отбрасываются; кавычки внутри экранируются.
pub fn fts_query(user_query: &str) -> String {
    user_query
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '*' | '^'))
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| w.chars().count() >= 2)
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
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
    fn builds_query() {
        assert_eq!(fts_query("запчасти Колыма"), "\"запчасти\"* \"Колыма\"*");
        assert_eq!(fts_query("a (b) \"c\""), "");
        assert_eq!(fts_query("счёт №25"), "\"счёт\"* \"25\"*");
    }
}
