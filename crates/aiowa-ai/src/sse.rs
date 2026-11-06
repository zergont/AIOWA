//! Разбор потока Server-Sent Events: события разделены пустой строкой, у события поля
//! `event:` и `data:` (несколько `data:` склеиваются переводом строки). Куски сети режут поток
//! где угодно, в том числе посреди символа UTF-8: строка разбирается, только когда пришла целиком.

/// Событие потока.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Имя из `event:` (у Anthropic есть, у OpenAI-совместимых — нет).
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    pending: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

/// Самая длинная строка потока, байт: больше — сервер прислал не то (защита памяти).
const MAX_LINE: usize = 4 * 1024 * 1024;

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавить кусок потока; вернуть события, которые он завершил. Ошибка — строка длиннее предела.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, String> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            self.line(&line, &mut out);
        }
        if self.pending.len() > MAX_LINE {
            return Err("слишком длинная строка в потоке ответа".into());
        }
        Ok(out)
    }

    /// Конец потока: событие без завершающей пустой строки тоже отдаётся.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).trim_end_matches('\r').to_string();
            self.line(&line, &mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return; // комментарий (keep-alive)
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event = None;
            return;
        }
        out.push(SseEvent { event: self.event.take(), data: std::mem::take(&mut self.data).join("\n") });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks_and_utf8() {
        let mut p = SseParser::new();
        let text = "event: content_block_delta\ndata: {\"t\":\"Привет\"}\n\n: ping\n\ndata: [DONE]\n\n";
        let bytes = text.as_bytes();
        let mut all = Vec::new();
        // По одному байту: режет и строки, и кириллицу посередине символа.
        for b in bytes {
            all.extend(p.push(std::slice::from_ref(b)).unwrap());
        }
        all.extend(p.finish());
        assert_eq!(all, vec![SseEvent { event: Some("content_block_delta".into()), data: "{\"t\":\"Привет\"}".into() }, SseEvent { event: None, data: "[DONE]".into() }]);
    }

    #[test]
    fn crlf_multiline_data_and_tail_without_blank_line() {
        let mut p = SseParser::new();
        let mut all = p.push(b"data: a\r\ndata: b\r\n\r\ndata: tail").unwrap();
        all.extend(p.finish());
        assert_eq!(all, vec![SseEvent { event: None, data: "a\nb".into() }, SseEvent { event: None, data: "tail".into() }]);
    }
}
