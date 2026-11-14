//! Шлюз моделей против фейковых серверов Anthropic и OpenAI-совместимого: минимальный HTTP/1.1 на
//! `127.0.0.1:0` (каждый ответ с `Connection: close`), поток SSE кусками с паузами — как у
//! настоящих серверов. Проверяется: ключ в заголовках, тело запроса, текст по кускам, токены,
//! отказы по кодам, таймаут, отмена и запись журнала.

use aiowa_ai::{AiError, CallRecord, Cancel, ChatRequest, Effort, Gateway, Message, Place, PrivacyMode, Profile, ProviderKind, StopReason};
use secrecy::SecretString;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Запрос, как его увидел сервер.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// Ответ сервера: заголовок и куски тела с паузой между ними; `hang` — молчать после заголовков.
struct Reply {
    status: &'static str,
    content_type: &'static str,
    chunks: Vec<String>,
    pause_ms: u64,
    hang: bool,
}

fn json_reply(status: &'static str, body: &str) -> Reply {
    Reply { status, content_type: "application/json", chunks: vec![body.to_string()], pause_ms: 0, hang: false }
}

fn sse_reply(chunks: Vec<String>) -> Reply {
    Reply { status: "200 OK", content_type: "text/event-stream", chunks, pause_ms: 20, hang: false }
}

type Handler = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

struct Server {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

async fn serve(handler: Handler) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { return };
            let handler = handler.clone();
            let seen = seen2.clone();
            tokio::spawn(async move { handle(sock, handler, seen).await });
        }
    });
    Server { base: format!("http://{addr}"), seen }
}

async fn handle(mut sock: TcpStream, handler: Handler, seen: Arc<Mutex<Vec<Seen>>>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect();
    let len = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = sock.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let req = Seen { method, path, headers, body };
    seen.lock().unwrap().push(req.clone());
    let reply = handler(&req);
    let head = format!("HTTP/1.1 {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n", reply.status, reply.content_type);
    if sock.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    if reply.hang {
        tokio::time::sleep(Duration::from_secs(30)).await;
        return;
    }
    for c in reply.chunks {
        if sock.write_all(c.as_bytes()).await.is_err() {
            return;
        }
        let _ = sock.flush().await;
        if reply.pause_ms > 0 {
            tokio::time::sleep(Duration::from_millis(reply.pause_ms)).await;
        }
    }
    let _ = sock.shutdown().await;
}

fn gateway() -> (Gateway, Arc<Mutex<Vec<CallRecord>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    let gw = Gateway::new("AIOWA-test", Arc::new(move |r| log2.lock().unwrap().push(r))).unwrap();
    (gw, log)
}

fn profile(kind: ProviderKind, base: &str, model: &str) -> Profile {
    Profile { id: "p1".into(), name: "Тест".into(), kind, base_url: base.into(), model: model.into(), timeout_s: 5, own_server: false, extra: String::new() }
}

fn req() -> ChatRequest {
    ChatRequest { feature: "проверка", system: "Отвечай одним словом".into(), messages: vec![Message::user("Готов?")], max_tokens: 64, effort: Some(Effort::Low), json_schema: None }
}

fn key(s: &str) -> SecretString {
    SecretString::from(s.to_string())
}

fn anthropic_events(text_parts: &[&str], stop: &str) -> Vec<String> {
    let mut out = vec![
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":11}}}\n\n".to_string(),
        "event: ping\ndata: {\"type\":\"ping\"}\n\n".to_string(),
    ];
    for t in text_parts {
        out.push(format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{t}\"}}}}\n\n"));
    }
    out.push(format!("event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\"}},\"usage\":{{\"output_tokens\":4}}}}\n\n"));
    out.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string());
    out
}

#[tokio::test]
async fn anthropic_stream_models_and_journal() {
    let server = serve(Arc::new(|r: &Seen| match (r.method.as_str(), r.path.as_str()) {
        ("GET", p) if p.starts_with("/v1/models") => json_reply("200 OK", r#"{"data":[{"id":"claude-test","display_name":"Claude Test","max_input_tokens":200000}],"has_more":false}"#),
        ("POST", "/v1/messages") => sse_reply(anthropic_events(&["Го", "тов"], "end_turn")),
        _ => json_reply("404 Not Found", r#"{"type":"error","error":{"type":"not_found_error","message":"nope"}}"#),
    }))
    .await;
    let (gw, log) = gateway();
    let p = profile(ProviderKind::Anthropic, &server.base, "claude-test");
    let k = key("sk-test-1");

    let models = gw.list_models(&p, Some(&k), PrivacyMode::Any, &Cancel::new()).await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].context, Some(200_000));

    let mut pieces = Vec::new();
    let out = gw.chat(&p, Some(&k), PrivacyMode::Any, &req(), &mut |t: &str| pieces.push(t.to_string()), &Cancel::new()).await.unwrap();
    assert_eq!(pieces, ["Го", "тов"], "текст приходит кусками");
    assert_eq!(out.text, "Готов");
    assert_eq!(out.stop, StopReason::End);
    assert_eq!((out.input_tokens, out.output_tokens), (Some(11), Some(4)));

    let seen = server.seen.lock().unwrap().clone();
    let post = seen.iter().find(|s| s.method == "POST").unwrap();
    assert_eq!(post.header("x-api-key"), Some("sk-test-1"));
    assert_eq!(post.header("anthropic-version"), Some("2026-10-21"));
    let body = post.json();
    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["system"], "Отвечай одним словом");
    assert_eq!(body["output_config"]["effort"], "low");

    let log = log.lock().unwrap().clone();
    assert_eq!(log.len(), 1, "журнал — только обращения к модели, не список моделей");
    let r = &log[0];
    assert_eq!((r.feature.as_str(), r.outcome.as_str(), r.model.as_str(), r.host.as_str()), ("проверка", "ok", "claude-test", "127.0.0.1"));
    assert_eq!(r.place, Place::Cloud);
    assert_eq!(r.request_sha256.len(), 64);
    assert_eq!((r.input_tokens, r.output_tokens), (Some(11), Some(4)));
}

#[tokio::test]
async fn openai_compatible_stream_without_key() {
    let server = serve(Arc::new(|r: &Seen| match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/v1/models") => json_reply("200 OK", r#"{"object":"list","data":[{"id":"t-pro","max_model_len":32768}]}"#),
        ("POST", "/v1/chat/completions") => sse_reply(vec![
            "data: {\"model\":\"t-pro\",\"choices\":[{\"delta\":{\"content\":\"При\"}}]}\n\n".into(),
            "data: {\"choices\":[{\"delta\":{\"content\":\"вет\"},\"finish_reason\":\"stop\"}]}\n\n".into(),
            "data: [DONE]\n\n".into(),
        ]),
        _ => json_reply("404 Not Found", r#"{"error":"not found"}"#),
    }))
    .await;
    let (gw, log) = gateway();
    // Адрес без /v1 — шлюз добавит сам; сервер на петлевом адресе — «этот компьютер».
    let mut p = profile(ProviderKind::OpenAi, &server.base, "t-pro");
    p.extra = r#"{"priority":"interactive","notes":false}"#.into();
    assert_eq!(p.place(), Place::Local);
    let models = gw.list_models(&p, None, PrivacyMode::OwnOnly, &Cancel::new()).await.unwrap();
    assert_eq!(models[0].id, "t-pro");
    let mut text = String::new();
    let out = gw.chat(&p, None, PrivacyMode::OwnOnly, &req(), &mut |t: &str| text.push_str(t), &Cancel::new()).await.unwrap();
    assert_eq!((text.as_str(), out.text.as_str()), ("Привет", "Привет"));
    let seen = server.seen.lock().unwrap().clone();
    let post = seen.iter().find(|s| s.method == "POST").unwrap();
    assert!(post.header("authorization").is_none(), "без ключа — без заголовка");
    let body = post.json();
    assert_eq!(body["max_tokens"], 64);
    assert!(body.get("output_config").is_none(), "effort только у Anthropic");
    assert_eq!((body["priority"].as_str(), body["notes"].as_bool()), (Some("interactive"), Some(false)), "дополнительные поля профиля");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(log.lock().unwrap()[0].place, Place::Local);
}

#[tokio::test]
async fn whole_json_answer_instead_of_stream() {
    let server = serve(Arc::new(|r: &Seen| match r.path.as_str() {
        "/v1/chat/completions" => json_reply("200 OK", r#"{"model":"fast","choices":[{"index":0,"message":{"role":"assistant","content":"Готов"},"finish_reason":"stop"}],"usage":{"prompt_tokens":19,"completion_tokens":3}}"#),
        _ => json_reply("404 Not Found", r#"{"error":"not found"}"#),
    }))
    .await;
    let (gw, _) = gateway();
    let p = profile(ProviderKind::OpenAi, &server.base, "fast");
    let mut got = String::new();
    let out = gw.chat(&p, None, PrivacyMode::Any, &req(), &mut |t: &str| got.push_str(t), &Cancel::new()).await.unwrap();
    assert_eq!((got.as_str(), out.text.as_str(), out.output_tokens), ("Готов", "Готов", Some(3)));
}

#[tokio::test]
async fn errors_policy_timeout_and_cancel() {
    let server = serve(Arc::new(|r: &Seen| match r.path.as_str() {
        "/v1/messages" if r.header("x-api-key") == Some("bad") => json_reply("401 Unauthorized", r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#),
        "/v1/messages" if r.header("x-api-key") == Some("busy") => json_reply("529 Overloaded", r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
        "/v1/messages" if r.header("x-api-key") == Some("hang") => Reply { status: "200 OK", content_type: "text/event-stream", chunks: vec![], pause_ms: 0, hang: true },
        "/v1/messages" if r.header("x-api-key") == Some("slow") => Reply { status: "200 OK", content_type: "text/event-stream", chunks: anthropic_events(&["a", "b", "c", "d", "e", "f"], "end_turn"), pause_ms: 400, hang: false },
        _ => json_reply("400 Bad Request", r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: too large"}}"#),
    }))
    .await;
    let (gw, log) = gateway();
    let p = profile(ProviderKind::Anthropic, &server.base, "claude-test");
    let mut sink = |_: &str| {};

    let e = gw.chat(&p, Some(&key("bad")), PrivacyMode::Any, &req(), &mut sink, &Cancel::new()).await.unwrap_err();
    assert!(matches!(e, AiError::Auth), "{e:?}");
    let e = gw.chat(&p, Some(&key("busy")), PrivacyMode::Any, &req(), &mut sink, &Cancel::new()).await.unwrap_err();
    assert!(matches!(e, AiError::RateLimited { .. }), "{e:?}");
    let e = gw.chat(&p, Some(&key("other")), PrivacyMode::Any, &req(), &mut sink, &Cancel::new()).await.unwrap_err();
    assert_eq!(e.to_string(), "Сервер модели отклонил запрос: max_tokens: too large");

    // Облако в режиме «только свои» до сервера не доходит и в журнал не пишется.
    let before = server.seen.lock().unwrap().len();
    let e = gw.chat(&p, Some(&key("ok")), PrivacyMode::OwnOnly, &req(), &mut sink, &Cancel::new()).await.unwrap_err();
    assert!(matches!(e, AiError::Policy));
    assert_eq!(server.seen.lock().unwrap().len(), before);

    // Сервер принял запрос и молчит: таймаут профиля (1 с).
    let mut quick = p.clone();
    quick.timeout_s = 1;
    let e = gw.chat(&quick, Some(&key("hang")), PrivacyMode::Any, &req(), &mut sink, &Cancel::new()).await.unwrap_err();
    assert!(matches!(e, AiError::Timeout), "{e:?}");

    // Отмена посреди потока: текст пришёл не весь, итог — «Отменено».
    let cancel = Cancel::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(900)).await;
        c2.cancel();
    });
    let mut got = String::new();
    let e = gw.chat(&p, Some(&key("slow")), PrivacyMode::Any, &req(), &mut |t: &str| got.push_str(t), &cancel).await.unwrap_err();
    assert!(matches!(e, AiError::Cancelled), "{e:?}");
    assert!(!got.is_empty() && got.len() < 6, "часть текста до отмены: {got:?}");

    let outcomes: Vec<String> = log.lock().unwrap().iter().map(|r| r.outcome.clone()).collect();
    assert_eq!(outcomes, ["auth", "rate_limited", "bad_request", "timeout", "cancelled"]);
}

/// Ответ по JSON-схеме (доводка разбора запроса, шаг И3): схема уходит в запросе; сервер, который
/// её не понимает, спрашивается ещё раз без неё; ответ не JSON — отказ, а не пустые условия.
#[tokio::test]
async fn json_by_schema_with_fallback() {
    use aiowa_ai::{nlsearch, JsonSchema};
    let server = serve(Arc::new(|r: &Seen| {
        let body = if r.method == "POST" { r.json() } else { Value::Null };
        let content = |text: &str| serde_json::json!({ "model": "fast", "choices": [{ "index": 0, "message": { "role": "assistant", "content": text }, "finish_reason": "stop" }], "usage": { "prompt_tokens": 400, "completion_tokens": 60 } }).to_string();
        match r.path.as_str() {
            // Старый сервер: схему в запросе не понимает.
            "/old/v1/chat/completions" if body.get("response_format").is_some() => json_reply("400 Bad Request", r#"{"error":{"message":"response_format is not supported"}}"#),
            "/old/v1/chat/completions" => json_reply("200 OK", &content("```json\n{\"from\": [\"Иванов\"], \"words\": [\"ремонт\"], \"date_from\": \"2026-10-01\", \"date_to\": \"2026-10-31\"}\n```")),
            "/new/v1/chat/completions" => json_reply("200 OK", &content("{\"from\": [], \"words\": [\"акт\"], \"unread\": true}")),
            "/chatty/v1/chat/completions" => json_reply("200 OK", &content("Конечно! Вот что я думаю о вашем запросе.")),
            _ => json_reply("404 Not Found", r#"{"error":"not found"}"#),
        }
    }))
    .await;
    let (gw, log) = gateway();
    let request = ChatRequest {
        feature: "разбор",
        system: nlsearch::SYSTEM.into(),
        messages: vec![Message::user(nlsearch::user_message("письма от Иванова про ремонт за октябрь", "13 ноября 2026, пятница (2026-11-13)"))],
        max_tokens: 512,
        effort: None,
        json_schema: Some(JsonSchema { name: "search_query", schema: nlsearch::schema() }),
    };

    // Сервер без поддержки схемы: два обращения, второе — без `response_format`.
    let old = profile(ProviderKind::OpenAi, &format!("{}/old/v1", server.base), "fast");
    let (value, out) = gw.chat_json(&old, None, PrivacyMode::Any, &request, &Cancel::new()).await.unwrap();
    let parsed = nlsearch::parse(&value).unwrap();
    assert_eq!((parsed.from.as_slice(), parsed.words.as_slice()), (&["Иванов".to_string()][..], &["ремонт".to_string()][..]));
    assert_eq!((parsed.date_from.as_deref(), parsed.date_to.as_deref()), (Some("2026-10-01"), Some("2026-10-31")));
    assert_eq!(out.output_tokens, Some(60));
    let posts: Vec<Value> = server.seen.lock().unwrap().iter().filter(|s| s.path.starts_with("/old/")).map(Seen::json).collect();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0]["response_format"]["json_schema"]["name"], "search_query");
    assert_eq!(posts[0]["response_format"]["json_schema"]["strict"], true);
    assert!(posts[1].get("response_format").is_none());
    assert!(posts[1]["messages"][1]["content"].as_str().unwrap().contains("Схема ответа (JSON)"), "схема есть и в промпте");

    // Сервер со схемой: одно обращение.
    let new = profile(ProviderKind::OpenAi, &format!("{}/new/v1", server.base), "fast");
    let (value, _) = gw.chat_json(&new, None, PrivacyMode::Any, &request, &Cancel::new()).await.unwrap();
    let parsed = nlsearch::parse(&value).unwrap();
    assert!(parsed.unread && parsed.words == ["акт"]);
    assert_eq!(server.seen.lock().unwrap().iter().filter(|s| s.path.starts_with("/new/")).count(), 1);

    // Модель ответила прозой: отказ с понятным кодом.
    let chatty = profile(ProviderKind::OpenAi, &format!("{}/chatty/v1", server.base), "fast");
    let e = gw.chat_json(&chatty, None, PrivacyMode::Any, &request, &Cancel::new()).await.unwrap_err();
    assert!(matches!(e, AiError::Protocol(_)), "{e:?}");

    // В журнале — все обращения, дошедшие до серверов, функцией «разбор».
    let records = log.lock().unwrap().clone();
    assert_eq!(records.iter().map(|r| r.outcome.as_str()).collect::<Vec<_>>(), ["bad_request", "ok", "ok", "ok"]);
    assert!(records.iter().all(|r| r.feature == "разбор"));
}
