//! OpenAI-совместимый протокол: `POST {base}/chat/completions` со `stream: true` и
//! `GET {base}/models`. Так говорят OpenAI, vLLM, Ollama (`/v1`), LM Studio, llama.cpp server.
//!
//! Поток — строки `data: {…}` с `choices[0].delta.content` и `finish_reason`, конец —
//! `data: [DONE]`. Рассуждения моделей (`reasoning_content`, `reasoning`) в текст ответа не идут.
//! Токены — из `usage`, если сервер его присылает (OpenAI — по `stream_options`).

use crate::sse::SseEvent;
use crate::{AiError, ChatOutcome, ChatRequest, ModelInfo, Role, StopReason};
use serde_json::{json, Map, Value};

/// Облако OpenAI: новые модели принимают только `max_completion_tokens`, токены в потоке —
/// только по `stream_options`. Свои серверы понимают `max_tokens`, а незнакомые поля некоторые
/// из них отвергают, поэтому им — минимальный запрос.
pub fn request_body(model: &str, req: &ChatRequest, openai_cloud: bool, extra: Option<&Map<String, Value>>) -> Value {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.is_empty() {
        messages.push(json!({ "role": "system", "content": req.system }));
    }
    for m in &req.messages {
        messages.push(json!({ "role": match m.role { Role::User => "user", Role::Assistant => "assistant" }, "content": m.text }));
    }
    let mut body = json!({ "model": model, "messages": messages, "stream": true });
    if openai_cloud {
        body["max_completion_tokens"] = json!(req.max_tokens);
        body["stream_options"] = json!({ "include_usage": true });
    } else {
        body["max_tokens"] = json!(req.max_tokens);
    }
    // Дополнительные поля профиля (расширения своих шлюзов) не меняют того, что задано выше.
    if let (Some(extra), Some(obj)) = (extra, body.as_object_mut()) {
        for (k, v) in extra {
            obj.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    body
}

/// Ответ целиком (сервер не стал отдавать поток): `choices[0].message.content`, причина, токены.
pub fn parse_completion(v: &Value) -> Result<ChatOutcome, AiError> {
    let choice = v["choices"].get(0).ok_or_else(|| AiError::Protocol("в ответе нет choices".into()))?;
    let text = choice["message"]["content"].as_str().unwrap_or("").to_string();
    let stop = match choice["finish_reason"].as_str() {
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        Some("stop") | None => StopReason::End,
        Some(other) => StopReason::Other(other.to_string()),
    };
    Ok(ChatOutcome { text, stop, input_tokens: v["usage"]["prompt_tokens"].as_u64(), output_tokens: v["usage"]["completion_tokens"].as_u64(), model: v["model"].as_str().map(str::to_string) })
}

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
    /// Кусок потока; текст — в `on_text`. `Ok(true)` — `[DONE]`.
    pub fn on_event(&mut self, ev: &SseEvent, on_text: &mut (dyn FnMut(&str) + Send)) -> Result<bool, AiError> {
        let data = ev.data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(true);
        }
        let v: Value = serde_json::from_str(data).map_err(|_| AiError::Protocol("непонятный кусок потока OpenAI-совместимого сервера".into()))?;
        if v.get("error").is_some_and(|e| !e.is_null()) {
            let message = v["error"]["message"].as_str().or_else(|| v["error"].as_str()).unwrap_or("").to_string();
            return Err(AiError::Server { status: 0, message });
        }
        if self.model.is_none() {
            self.model = v["model"].as_str().map(str::to_string);
        }
        if let Some(choice) = v["choices"].get(0) {
            if let Some(t) = choice["delta"]["content"].as_str()
                && !t.is_empty()
            {
                self.text.push_str(t);
                on_text(t);
            }
            if let Some(r) = choice["finish_reason"].as_str() {
                self.stop = Some(match r {
                    "stop" => StopReason::End,
                    "length" => StopReason::MaxTokens,
                    "content_filter" => StopReason::Refusal,
                    other => StopReason::Other(other.to_string()),
                });
            }
        }
        let u = &v["usage"];
        if let Some(n) = u["prompt_tokens"].as_u64() {
            self.input_tokens = Some(n);
        }
        if let Some(n) = u["completion_tokens"].as_u64() {
            self.output_tokens = Some(n);
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

/// Список моделей: `data[].id`; окно контекста — если сервер его называет (vLLM `max_model_len`,
/// LM Studio и другие — `context_length`).
pub fn parse_models(v: &Value) -> Vec<ModelInfo> {
    let mut list: Vec<ModelInfo> = v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let id = m["id"].as_str()?.to_string();
                    let context = m["max_model_len"].as_u64().or_else(|| m["context_length"].as_u64()).or_else(|| m["ctx"].as_u64());
                    Some(ModelInfo { id, name: None, context })
                })
                .collect()
        })
        .unwrap_or_default();
    list.sort_by(|a, b| a.id.cmp(&b.id));
    list
}

/// Текст ошибки из тела отказа (`{"error":{"message"}}` или `{"error":"..."}`, у Ollama — `{"error":"..."}`).
pub fn error_message(v: &Value) -> Option<String> {
    v["error"]["message"].as_str().or_else(|| v["error"].as_str()).or_else(|| v["detail"].as_str()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    fn data(s: &str) -> SseEvent {
        SseEvent { event: None, data: s.into() }
    }

    #[test]
    fn body_differs_for_openai_cloud_and_own_servers() {
        let req = ChatRequest { feature: "проверка", system: "Система".into(), messages: vec![Message::user("Привет")], max_tokens: 256, effort: None };
        let own = request_body("qwen3", &req, false, None);
        assert_eq!(own["messages"][0]["role"], "system");
        assert_eq!(own["messages"][1]["content"], "Привет");
        assert_eq!(own["max_tokens"], 256);
        assert!(own.get("stream_options").is_none() && own.get("max_completion_tokens").is_none());
        let cloud = request_body("gpt", &req, true, None);
        assert_eq!(cloud["max_completion_tokens"], 256);
        assert_eq!(cloud["stream_options"]["include_usage"], true);
        assert!(cloud.get("max_tokens").is_none());
        let extra: Map<String, Value> = serde_json::from_str(r#"{"priority":"interactive","notes":false,"model":"чужая"}"#).unwrap();
        let gw = request_body("fast", &req, false, Some(&extra));
        assert_eq!((gw["priority"].as_str(), gw["notes"].as_bool()), (Some("interactive"), Some(false)));
        assert_eq!(gw["model"], "fast", "дополнительные поля не меняют заданного шлюзом");
    }

    #[test]
    fn whole_completion_without_stream() {
        let v: Value = serde_json::from_str(r#"{"model":"fast","choices":[{"index":0,"message":{"role":"assistant","content":"Готов"},"finish_reason":"stop"}],"usage":{"completion_tokens":3,"prompt_tokens":19}}"#).unwrap();
        let o = parse_completion(&v).unwrap();
        assert_eq!((o.text.as_str(), o.stop, o.input_tokens, o.output_tokens), ("Готов", StopReason::End, Some(19), Some(3)));
    }

    #[test]
    fn stream_text_reasoning_skipped_usage_and_done() {
        let mut s = Stream::default();
        let mut got = String::new();
        let mut sink = |t: &str| got.push_str(t);
        for d in [
            r#"{"model":"qwen3-32b","choices":[{"delta":{"role":"assistant","reasoning_content":"думаю"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Гот"}}]}"#,
            r#"{"choices":[{"delta":{"content":"ов"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3}}"#,
        ] {
            assert!(!s.on_event(&data(d), &mut sink).unwrap());
        }
        assert!(s.on_event(&data("[DONE]"), &mut sink).unwrap());
        let out = s.finish().unwrap();
        assert_eq!((got.as_str(), out.text.as_str()), ("Готов", "Готов"));
        assert_eq!(out.stop, StopReason::End);
        assert_eq!((out.input_tokens, out.output_tokens), (Some(12), Some(3)));
        assert_eq!(out.model.as_deref(), Some("qwen3-32b"));
    }

    #[test]
    fn stream_error_length_and_cut() {
        let mut sink = |_: &str| {};
        let mut s = Stream::default();
        assert!(matches!(s.on_event(&data(r#"{"error":{"message":"model not found"}}"#), &mut sink), Err(AiError::Server { .. })));
        let mut s = Stream::default();
        s.on_event(&data(r#"{"choices":[{"delta":{"content":"a"},"finish_reason":"length"}]}"#), &mut sink).unwrap();
        assert_eq!(s.finish().unwrap().stop, StopReason::MaxTokens);
        let mut s = Stream::default();
        s.on_event(&data(r#"{"choices":[{"delta":{"content":"a"}}]}"#), &mut sink).unwrap();
        assert!(matches!(s.finish(), Err(AiError::Network(_))));
    }

    #[test]
    fn models_sorted_with_context() {
        let v: Value = serde_json::from_str(r#"{"object":"list","data":[{"id":"t-pro","max_model_len":32768},{"id":"qwen3:8b"},{"id":"q27-5090","kind":"head","ctx":131072}]}"#).unwrap();
        let list = parse_models(&v);
        assert_eq!(list.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["q27-5090", "qwen3:8b", "t-pro"]);
        assert_eq!(list[2].context, Some(32768));
        assert_eq!(list[0].context, Some(131072), "окно контекста у шлюза Горыныч — ctx");
        assert_eq!(error_message(&serde_json::json!({"error":"model 'x' not found"})).as_deref(), Some("model 'x' not found"));
    }
}
