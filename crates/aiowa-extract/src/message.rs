//! Вложенные письма `.eml` (docs/obkatka.md п. 30): письмо, пересланное «как вложение», сервер
//! отдаёт целиком в MIME. Здесь — разбор для просмотра в окне (`parse_message`), текст для
//! поискового индекса (`message_text`) и узнавание письма по содержимому (`looks_like_message`):
//! у вложений, скачанных до п. 30, нет ни расширения, ни типа.

use mail_parser::{AddressList, MessageParser};

/// Адрес из шапки письма.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagePerson {
    pub name: Option<String>,
    pub email: Option<String>,
}

/// Вложение внутри вложенного письма.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageAttachment {
    pub name: Option<String>,
    pub content_type: Option<String>,
    /// `Content-ID` без угловых скобок.
    pub content_id: Option<String>,
    /// Картинка, на которую ссылается текст письма (`cid:`): её показывают в тексте, а не списком.
    pub inline: bool,
    pub bytes: Vec<u8>,
}

/// Разобранное письмо.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedMessage {
    pub subject: Option<String>,
    pub from: Vec<MessagePerson>,
    pub to: Vec<MessagePerson>,
    pub cc: Vec<MessagePerson>,
    /// Дата из шапки, мс эпохи.
    pub date_ms: Option<i64>,
    /// Тело HTML (у письма только с текстом — текст, переведённый в HTML). Не очищено.
    pub html: Option<String>,
    /// Тело текстом (у письма только с HTML — HTML, переведённый в текст).
    pub text: Option<String>,
    pub attachments: Vec<MessageAttachment>,
}

fn people(list: Option<AddressList<'_>>) -> Vec<MessagePerson> {
    list.map(|l| l.mailboxes().map(|m| MessagePerson { name: m.name().map(String::from), email: m.address().map(String::from) }).collect()).unwrap_or_default()
}

/// Разобрать письмо RFC 822; `None` — это не письмо (нет шапки).
pub fn parse_message(bytes: &[u8]) -> Option<ParsedMessage> {
    let m = MessageParser::default().parse(bytes)?;
    let html = m.body_html(0).map(|h| h.into_owned());
    let referenced = |cid: &str| html.as_deref().is_some_and(|h| h.contains(&format!("cid:{cid}")));
    // Вложения письма и картинки его текста: по RFC 8621 картинка, на которую ссылается HTML, лежит
    // в списке тела, а не вложений.
    let mut seen = std::collections::HashSet::new();
    let attachments = m
        .attachments()
        .chain(m.html_body().filter(|p| !p.is_text() && p.content_id().is_some()))
        .filter(|p| seen.insert(p.id()))
        .map(|p| {
            let content_type = p.content_type().map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub).to_ascii_lowercase(),
                None => ct.ctype().to_ascii_lowercase(),
            });
            let content_id = p.content_id().map(|c| c.trim().trim_start_matches('<').trim_end_matches('>').to_string()).filter(|c| !c.is_empty());
            // Вложенное в письмо письмо — тоже `.eml`, по его теме, если имени нет.
            let name = p.attachment_name().map(String::from).or_else(|| p.nested().and_then(|n| n.subject().map(|s| format!("{s}.eml"))));
            MessageAttachment { inline: content_id.as_deref().is_some_and(referenced), name, content_type, content_id, bytes: p.decoded().into_owned() }
        })
        .collect();
    Some(ParsedMessage {
        subject: m.subject().map(String::from),
        from: people(m.from()),
        to: people(m.to()),
        cc: people(m.cc()),
        date_ms: m.date().map(|d| d.to_timestamp() * 1000),
        text: m.body_text(0).map(|t| t.into_owned()),
        html,
        attachments,
    })
}

fn person_line(people: &[MessagePerson]) -> String {
    people
        .iter()
        .map(|p| match (p.name.as_deref(), p.email.as_deref()) {
            (Some(n), Some(e)) => format!("{n} <{e}>"),
            (Some(n), None) => n.to_string(),
            (None, Some(e)) => e.to_string(),
            (None, None) => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Текст письма для поиска: тема, отправитель, получатели, текст и имена его вложений.
pub(crate) fn message_text(bytes: &[u8]) -> Result<String, String> {
    let m = parse_message(bytes).ok_or("eml: не письмо")?;
    let mut out = String::new();
    let mut line = |label: &str, value: &str| {
        if !value.trim().is_empty() {
            out.push_str(label);
            out.push_str(value);
            out.push('\n');
        }
    };
    line("Тема: ", m.subject.as_deref().unwrap_or(""));
    line("От: ", &person_line(&m.from));
    line("Кому: ", &person_line(&m.to));
    line("Копия: ", &person_line(&m.cc));
    let names = m.attachments.iter().filter(|a| !a.inline).filter_map(|a| a.name.as_deref()).collect::<Vec<_>>().join(", ");
    line("Вложения: ", &names);
    out.push('\n');
    out.push_str(m.text.as_deref().unwrap_or(""));
    Ok(out)
}

/// Начало файла похоже на письмо RFC 822: до пустой строки идут только строки заголовков (и их
/// продолжения), и среди них есть хотя бы два из характерных для письма. Так узнаются вложенные
/// письма, скачанные до п. 30 без расширения и с типом `application/octet-stream`.
pub fn looks_like_message(bytes: &[u8]) -> bool {
    const KNOWN: [&str; 9] = ["from", "to", "subject", "date", "message-id", "mime-version", "received", "content-type", "return-path"];
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(16 * 1024)]);
    let mut known = 0usize;
    for (n, line) in head.lines().enumerate() {
        if line.trim().is_empty() {
            return n > 0 && known >= 2;
        }
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((name, _)) = line.split_once(':') else { return false };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return false;
        }
        if KNOWN.contains(&name.to_ascii_lowercase().as_str()) {
            known += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const EML: &str = "From: =?UTF-8?B?0J/QtdGC0YDQvtCy?= <petrov@example.ru>\r\n\
To: Ivanov <ivanov@example.ru>\r\n\
Subject: =?windows-1251?B?0fe48iDn4CDu6vL/4fD8?=\r\n\
Date: Mon, 02 Nov 2026 10:15:00 +0300\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Transfer-Encoding: 8bit\r\n\
\r\n\
<p>Счёт во вложении <img src=\"cid:logo@x\"></p>\r\n\
--b1\r\n\
Content-Type: image/png\r\n\
Content-ID: <logo@x>\r\n\
Content-Disposition: inline\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
iVBORw0KGgo=\r\n\
--b1\r\n\
Content-Type: application/pdf; name=\"schet.pdf\"\r\n\
Content-Disposition: attachment; filename=\"schet.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0=\r\n\
--b1--\r\n";

    #[test]
    fn parses_headers_body_and_attachments() {
        let m = parse_message(EML.as_bytes()).unwrap();
        assert_eq!(m.subject.as_deref(), Some("Счёт за октябрь"), "тема в windows-1251");
        assert_eq!(m.from, vec![MessagePerson { name: Some("Петров".into()), email: Some("petrov@example.ru".into()) }]);
        assert_eq!(m.to.len(), 1);
        assert!(m.date_ms.is_some());
        assert!(m.html.as_deref().unwrap().contains("Счёт во вложении"));
        assert_eq!(m.attachments.len(), 2);
        let logo = m.attachments.iter().find(|a| a.content_id.as_deref() == Some("logo@x")).unwrap();
        assert!(logo.inline, "картинка, на которую ссылается текст");
        let pdf = m.attachments.iter().find(|a| a.name.as_deref() == Some("schet.pdf")).unwrap();
        assert!(!pdf.inline);
        assert_eq!(pdf.content_type.as_deref(), Some("application/pdf"));
        assert_eq!(pdf.bytes, b"%PDF-");
    }

    #[test]
    fn text_for_search_and_recognition() {
        let t = message_text(EML.as_bytes()).unwrap();
        assert!(t.contains("Тема: Счёт за октябрь"));
        assert!(t.contains("От: Петров <petrov@example.ru>"));
        assert!(t.contains("Вложения: schet.pdf"), "имена вложений, без картинок текста: {t}");
        assert!(t.contains("Счёт во вложении"));
        assert!(looks_like_message(EML.as_bytes()));
        assert!(!looks_like_message(b"%PDF-1.7\n..."));
        assert!(!looks_like_message(b"Hello: world\n\nbody"), "один заголовок — не письмо");
        assert!(!looks_like_message(b"just text without headers"));
    }
}
