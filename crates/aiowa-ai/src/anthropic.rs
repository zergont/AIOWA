//! Messages API Anthropic: запрос и разбор потока. Официального SDK для Rust нет, поэтому HTTP
//! напрямую: `POST /v1/messages` со `stream: true`, заголовки `x-api-key` и `anthropic-version`.
//!
//! Поток — события `message_start` (модель, токены запроса), `content_block_start` /
//! `content_block_delta` / `content_block_stop` (текст — `text_delta`; блоки размышлений модели
//! пропускаются), `message_delta` (причина остановки, токены ответа), `message_stop`; `ping` —
//! пустые; `error` — отказ посреди потока (например, перегрузка).

use crate::{AiError, ChatOutcome, ChatRequest, Effort, ModelInfo, Role, StopReason};
use crate::sse::SseEvent;
use serde_json::{json, Value};

/// Версия API в заголовке `anthropic-version`. С этой версией параметры выборки (`temperature`,
/// `top_p`, `top_k`) не принимаются: «ровность» ответа задаётся промптом и `effort`.
pub const API_VERSION: &str = "2026-10-21";

pub fn request_body(model: &str, req: &ChatRequest) -> Value {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| json!({ "role": match m.role { Role::User => "user", Role::Assistant => "assistant" }, "content": m.text }))
        .collect();
    let mut body = json!({
        "model": model,
        "max_tokens": req.max_tokens,
        "stream": true,
        "messages": messages,
    });
    if !req.system.is_empty() {
        body["system"] = json!(req.system);
    }
    if let Some(e) = req.effort {
        body["output_config"] = json!({ "effort": match e { Effort::Low => "low", Effort::Medium => "medium", Effort::High => "high" } });
    }
    if let Some(s) = &req.json_schema {
        body["output_config"]["format"] = json!({ "type": "json_schema", "schema": s.schema });
    }
    body
}

/// Состояние разбора одного ответа.
#[derive(Debug, Default)]
pub struct Stream {
    text: String,
    model: Option<String>,
    stop: Option<StopReason>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    done: bool,
}

impl Stream {
    /// Событие потока; кусок текста — в `on_text`. `Ok(true)` — ответ закончен.
    pub fn on_event(&mut self, ev: &SseEvent, on_text: &mut (dyn FnMut(&str) + Send)) -> Result<bool, AiError> {
        let kind = ev.event.as_deref().unwrap_or("");
        if kind == "ping" {
            return Ok(false);
        }
        let v: Value = serde_json::from_str(&ev.data).map_err(|_| AiError::Protocol("непонятный кусок потока Anthropic".into()))?;
        let kind = if kind.is_empty() { v["type"].as_str().unwrap_or("") } else { kind };
        match kind {
            "message_start" => {
                let m = &v["message"];
                self.model = m["model"].as_str().map(str::to_string);
                let u = &m["usage"];
                // Кэшированная часть запроса тоже вход: считаем все токены запроса.
                let total = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"].iter().filter_map(|k| u[*k].as_u64()).sum::<u64>();
                self.input_tokens = Some(total);
            }
            "content_block_delta" => {
                if v["delta"]["type"] == "text_delta"
                    && let Some(t) = v["delta"]["text"].as_str()
                {
                    self.text.push_str(t);
                    on_text(t);
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(stop_reason(s));
                }
                if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = Some(n);
                }
            }
            "message_stop" => {
                self.done = true;
                return Ok(true);
            }
            "error" => return Err(stream_error(&v["error"])),
            _ => {}
        }
        Ok(false)
    }

    pub fn finish(self) -> Result<ChatOutcome, AiError> {
        if !self.done && self.stop.is_none() {
            return Err(AiError::Network("Соединение оборвалось посреди ответа модели".into()));
        }
        Ok(ChatOutcome { text: self.text, stop: self.stop.unwrap_or(StopReason::End), input_tokens: self.input_tokens, output_tokens: self.output_tokens, model: self.model })
    }
}

fn stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" | "stop_sequence" => StopReason::End,
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}

/// Ошибка внутри потока (`event: error`): перегрузка — повторить позже, остальное — отказ.
fn stream_error(e: &Value) -> AiError {
    let kind = e["type"].as_str().unwrap_or("");
    let message = e["message"].as_str().unwrap_or("").to_string();
    match kind {
        "overloaded_error" | "rate_limit_error" => AiError::RateLimited { retry_after_s: None },
        "authentication_error" | "permission_error" => AiError::Auth,
        _ => AiError::Server { status: 0, message },
    }
}

/// Ответ целиком (без потока): текстовые блоки `content`, причина, токены.
pub fn parse_message(v: &Value) -> Result<ChatOutcome, AiError> {
    let blocks = v["content"].as_array().ok_or_else(|| AiError::Protocol("в ответе нет content".into()))?;
    let text: String = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    let u = &v["usage"];
    let input = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"].iter().filter_map(|k| u[*k].as_u64()).sum::<u64>();
    Ok(ChatOutcome {
        text,
        stop: v["stop_reason"].as_str().map(stop_reason).unwrap_or(StopReason::End),
        input_tokens: Some(input),
        output_tokens: u["output_tokens"].as_u64(),
        model: v["model"].as_str().map(str::to_string),
    })
}

/// Список моделей (`GET /v1/models`): id, имя для людей, окно контекста.
pub fn parse_models(v: &Value) -> (Vec<ModelInfo>, Option<String>) {
    let list = v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let id = m["id"].as_str()?.to_string();
                    Some(ModelInfo { id, name: m["display_name"].as_str().map(str::to_string), context: m["max_input_tokens"].as_u64() })
                })
                .collect()
        })
        .unwrap_or_default();
    let next = (v["has_more"] == true).then(|| v["last_id"].as_str().map(str::to_string)).flatten();
    (list, next)
}

/// Текст ошибки из тела отказа (`{"type":"error","error":{"type","message"}}`).
pub fn error_message(v: &Value) -> Option<String> {
    v["error"]["message"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent { event: Some(event.into()), data: data.into() }
    }

    #[test]
    fn body_has_model_system_messages_and_effort() {
        let req = ChatRequest { feature: "проверка", system: "Ты помощник".into(), messages: vec![Message::user("Привет")], max_tokens: 512, effort: Some(Effort::Low), json_schema: None };
        let with_schema = ChatRequest { json_schema: Some(crate::JsonSchema { name: "search", schema: json!({ "type": "object" }) }), ..req.clone() };
        assert_eq!(request_body("m", &with_schema)["output_config"], json!({ "effort": "low", "format": { "type": "json_schema", "schema": { "type": "object" } } }));
        let no_effort = ChatRequest { effort: None, ..with_schema.clone() };
        assert_eq!(request_body("m", &no_effort)["output_config"], json!({ "format": { "type": "json_schema", "schema": { "type": "object" } } }));
        let b = request_body("claude-opus-5-5", &req);
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["stream"], true);
        assert_eq!(b["system"], "Ты помощник");
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["output_config"]["effort"], "low");
        assert!(b.get("temperature").is_none(), "параметры выборки этой версией API не принимаются");
        let plain = request_body("m", &ChatRequest { feature: "x", system: String::new(), messages: vec![Message::user("a")], max_tokens: 10, effort: None, json_schema: None });
        assert!(plain.get("system").is_none() && plain.get("output_config").is_none());
    }

    #[test]
    fn stream_collects_text_and_usage_skipping_thinking() {
        let mut s = Stream::default();
        let mut got = String::new();
        let mut sink = |t: &str| got.push_str(t);
        let events = [
            ev("message_start", r#"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":20,"cache_read_input_tokens":100}}}"#),
            ev("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#),
            ev("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"размышляю"}}"#),
            ev("ping", r#"{"type":"ping"}"#),
            ev("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Гото"}}"#),
            ev("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"в"}}"#),
            ev("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#),
        ];
        for e in &events {
            assert!(!s.on_event(e, &mut sink).unwrap());
        }
        assert!(s.on_event(&ev("message_stop", r#"{"type":"message_stop"}"#), &mut sink).unwrap());
        let out = s.finish().unwrap();
        assert_eq!(got, "Готов");
        assert_eq!(out.text, "Готов");
        assert_eq!(out.stop, StopReason::End);
        assert_eq!((out.input_tokens, out.output_tokens), (Some(120), Some(7)));
        assert_eq!(out.model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn refusal_overload_and_cut_stream() {
        let mut s = Stream::default();
        let mut sink = |_: &str| {};
        s.on_event(&ev("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":0}}"#), &mut sink).unwrap();
        assert_eq!(s.finish().unwrap().stop, StopReason::Refusal);

        let mut s = Stream::default();
        let err = s.on_event(&ev("error", r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#), &mut sink).unwrap_err();
        assert!(matches!(err, AiError::RateLimited { .. }));

        let mut s = Stream::default();
        s.on_event(&ev("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"пол"}}"#), &mut sink).unwrap();
        assert!(matches!(s.finish(), Err(AiError::Network(_))), "без конца ответа — обрыв");
    }

    #[test]
    fn models_page() {
        let v: Value = serde_json::from_str(r#"{"data":[{"id":"claude-opus-5-5","display_name":"Claude Opus 5.5","max_input_tokens":1000000},{"id":"claude-haiku-5-5"}],"has_more":true,"last_id":"claude-haiku-5-5"}"#).unwrap();
        let (list, next) = parse_models(&v);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name.as_deref(), Some("Claude Opus 5.5"));
        assert_eq!(list[0].context, Some(1_000_000));
        assert_eq!(next.as_deref(), Some("claude-haiku-5-5"));
    }
}
