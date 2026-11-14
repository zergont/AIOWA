//! Поиск в окне (`docs/ai-layer-design.md` § 5, шаг И3): запрос на обычном языке разбирает
//! хранилище без модели (`aiowa_store::nlquery`) и вместе с письмами отдаёт, как он понят, — окно
//! показывает это чипами. Модель нужна двум необязательным вещам, и обе идут через шлюз с записью
//! в журнал обращений:
//!
//! - **доводка запроса** (назначение «Разбор запросов», функция журнала «разбор»): окно зовёт её
//!   по Enter, когда разбор чего-то не понял или ничего не нашлось. Модель возвращает условия
//!   поиска по JSON-схеме, имена людей — словами; адреса по ним подбирает код, адресная книга в
//!   модель не уходит. Итог — тот же запрос операторами и выдача по нему;
//! - **«Кратко по найденному»** (назначение «Совет», функция «кратко»): модель читает до 20
//!   верхних писем выдачи — контекст собирается как для «Совета» (видимый текст без скрытых
//!   блоков, недостающие тела догружаются) — и отвечает на запрос потоком со ссылками на письма.
//!
//! При выключенных ИИ-функциях работает только разбор без модели; к моделям обращений нет.

use crate::advice::{context_message, ru_today, CtxRef};
use crate::ai::{key_get, AiConfig, AiState};
use crate::compose::own_addresses_of;
use crate::ondemand::{ensure_body, OnDemand};
use crate::sync::SyncState;
use crate::{err, ms, AppState, CmdResult};
use aiowa_ai::digest::{self, DigestInput};
use aiowa_ai::{advice as ai_advice, nlsearch, ChatRequest, Effort, JsonSchema, Message, Place, Profile, ProviderKind, StopReason};
use aiowa_store::nldate::Period;
use aiowa_store::nlquery::ModelFields;
use aiowa_store::query::AttachmentKind;
use aiowa_store::{SearchHit, SearchOutcome};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tauri::ipc::Channel;
use tauri::State;
use tracing::{info, warn};

/// Писем, которые читает модель в «Кратко по найденному» (верхние письма выдачи).
const DIGEST_MAX: usize = 20;
/// Бюджет сообщения с письмами, символов: облако и свои модели — как у «Совета».
const BUDGET_CLOUD: usize = 90_000;
const BUDGET_OWN: usize = 40_000;
/// Предел длины ответа модели, токенов (у моделей с размышлением — с запасом).
const DIGEST_MAX_TOKENS: u32 = 4096;
const REFINE_MAX_TOKENS: u32 = 2048;

// --- поиск ------------------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct SearchHitDto {
    item_pk: i64,
    conversation_pk: Option<i64>,
    subject: Option<String>,
    from_name: Option<String>,
    from_email: Option<String>,
    received_ms: Option<i64>,
    folder_name: Option<String>,
    snippet: String,
    attachment_name: Option<String>,
    /// Начало текста письма: строка результата показывает его, когда фрагмента нет.
    preview: Option<String>,
    /// У письма есть вложения: строка результата рисует скрепку (п. 29).
    has_attachments: bool,
}

#[derive(Serialize)]
struct ChipOptionDto {
    label: String,
    operator: String,
}

/// Условие, понятое из слов запроса: подпись, место в запросе (индексы строки окна) и оператор,
/// которым окно заменяет этот текст при правке.
#[derive(Serialize)]
struct ChipDto {
    kind: &'static str,
    label: String,
    start: usize,
    end: usize,
    operator: String,
    options: Vec<ChipOptionDto>,
    note: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct SearchDto {
    hits: Vec<SearchHitDto>,
    chips: Vec<ChipDto>,
    /// Запрос операторами: во что превратилась фраза.
    canonical: String,
    /// Слова, которые разбор не понял: повод позвать модель.
    unknown: Vec<String>,
    /// Столько первых писем найдено по самому запросу; остальные — по ослабленному условию.
    strict: usize,
    relaxed_note: Option<String>,
}

fn hit_dto(h: SearchHit) -> SearchHitDto {
    SearchHitDto {
        item_pk: h.item_pk,
        conversation_pk: h.conversation_pk,
        subject: h.subject,
        from_name: h.from_name,
        from_email: h.from_email,
        received_ms: ms(h.received_at),
        folder_name: h.folder_name,
        snippet: h.snippet,
        attachment_name: h.attachment_name,
        preview: h.preview,
        has_attachments: h.has_attachments,
    }
}

fn search_dto(o: SearchOutcome) -> SearchDto {
    SearchDto {
        hits: o.hits.into_iter().map(hit_dto).collect(),
        chips: o
            .understood
            .chips
            .into_iter()
            .map(|c| ChipDto { kind: c.kind.code(), label: c.label, start: c.start, end: c.end, operator: c.operator, options: c.options.into_iter().map(|o| ChipOptionDto { label: o.label, operator: o.operator }).collect(), note: c.note })
            .collect(),
        canonical: o.understood.canonical,
        unknown: o.understood.unknown,
        strict: o.strict,
        relaxed_note: o.relaxed_note,
    }
}

fn limit_of(limit: Option<usize>) -> usize {
    limit.unwrap_or(50).clamp(1, 200)
}

/// Поиск по локальной базе: письма и то, как понят запрос. Модель здесь не участвует.
#[tauri::command(async)]
pub(crate) fn search(state: State<'_, AppState>, query: String, limit: Option<usize>) -> CmdResult<SearchDto> {
    let started = Instant::now();
    let outcome = state.store.search_understood(state.account, &query, limit_of(limit)).map_err(err)?;
    info!(hits = outcome.hits.len(), chips = outcome.understood.chips.len(), unknown = outcome.understood.unknown.len(), relaxed = outcome.relaxed_note.is_some(), ms = started.elapsed().as_millis(), "поиск");
    Ok(search_dto(outcome))
}

// --- что из ИИ доступно поиску -------------------------------------------------------------------

/// Доступна ли функция и какая модель её выполняет (подпись «ИИ · место · подключение (модель)»).
#[derive(Debug, Serialize)]
struct AiUseDto {
    available: bool,
    reason: Option<String>,
    profile_name: Option<String>,
    model: Option<String>,
    place: Option<Place>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SearchAiStatusDto {
    /// Доводка запроса быстрой моделью (назначение «Разбор запросов»).
    refine: AiUseDto,
    /// «Кратко по найденному» (назначение «Совет»).
    digest: AiUseDto,
}

fn ai_use(cfg: &AiConfig, assigned: Option<&str>, what: &str) -> AiUseDto {
    let profile = assigned.and_then(|id| cfg.profile(id).ok());
    let reason = if !cfg.enabled {
        Some("ИИ-функции выключены: Настройки → ИИ".to_string())
    } else if profile.is_none() {
        Some(format!("Не назначена модель для {what}: Настройки → ИИ"))
    } else if profile.is_some_and(|p| p.model.trim().is_empty()) {
        Some(format!("У подключения для {what} не выбрана модель"))
    } else {
        None
    };
    AiUseDto { available: reason.is_none(), reason, profile_name: profile.map(|p| p.name.clone()), model: profile.map(|p| p.model.clone()), place: profile.map(|p| p.place()) }
}

/// Подключение функции или причина, почему его нет (текстом для окна).
fn ai_profile(cfg: &AiConfig, assigned: Option<&str>, what: &str) -> CmdResult<Profile> {
    let u = ai_use(cfg, assigned, what);
    match (u.reason, assigned.and_then(|id| cfg.profile(id).ok())) {
        (None, Some(p)) => Ok(p.clone()),
        (reason, _) => Err(reason.unwrap_or_else(|| "Подключение не найдено".into())),
    }
}

#[tauri::command(async)]
pub(crate) fn search_ai_status(state: State<'_, AppState>) -> CmdResult<SearchAiStatusDto> {
    let cfg = AiConfig::load(&state.store, state.account);
    Ok(SearchAiStatusDto { refine: ai_use(&cfg, cfg.parse.as_deref(), "разбора запросов"), digest: ai_use(&cfg, cfg.advice.as_deref(), "«Совета»") })
}

// --- доводка запроса моделью ---------------------------------------------------------------------

/// Запрос, доведённый моделью: операторами, подписями условий и выдачей по нему.
#[derive(Serialize)]
pub(crate) struct RefinedDto {
    operators: String,
    labels: Vec<String>,
    result: SearchDto,
    ms: u64,
    /// Разбор взят из памяти (этот запрос сегодня уже разбирался): к модели обращения не было.
    cached: bool,
}

/// Условия модели в виде, который понимает хранилище: даты — датами, папки — служебными именами.
fn model_fields(p: &nlsearch::Parsed) -> ModelFields {
    let date = |s: &Option<String>| s.as_deref().and_then(|s| s.parse::<jiff::civil::Date>().ok());
    let (start, last) = (date(&p.date_from), date(&p.date_to));
    ModelFields {
        from: p.from.clone(),
        to: p.to.clone(),
        with: p.with.clone(),
        from_me: p.from_me,
        // У модели конец периода включительно, у поиска — не включительно.
        period: (start.is_some() || last.is_some()).then(|| Period { start, end: last.and_then(|d| d.tomorrow().ok()) }),
        has_attachment: p.has_attachment,
        attachment_kind: p.attachment_kind.as_deref().and_then(|k| match k {
            "pdf" => Some(AttachmentKind::Pdf),
            "sheet" => Some(AttachmentKind::Sheet),
            "doc" => Some(AttachmentKind::Doc),
            "image" => Some(AttachmentKind::Image),
            "archive" => Some(AttachmentKind::Archive),
            "slides" => Some(AttachmentKind::Slides),
            _ => None,
        }),
        folder: p.folder.as_deref().and_then(|f| match f {
            "inbox" => Some("inbox"),
            "sent" => Some("sentitems"),
            "drafts" => Some("drafts"),
            "deleted" => Some("deleteditems"),
            "junk" => Some("junkemail"),
            _ => None,
        })
        .map(str::to_string),
        unread: p.unread,
        flagged: p.flagged,
        words: p.words.clone(),
        phrases: p.phrases.clone(),
    }
}

/// Довести запрос быстрой моделью. `None` — модель ничего не добавила к разбору без неё (те же
/// письма) или запрос ей не отдаётся (пустой, слишком длинный). Отказ — текстом: ИИ выключен,
/// модель не назначена, сервер не ответил.
#[tauri::command]
pub(crate) async fn search_refine(query: String, limit: Option<usize>, state: State<'_, AppState>, ai: State<'_, AiState>) -> CmdResult<Option<RefinedDto>> {
    let q = query.trim().to_string();
    if q.is_empty() || q.chars().count() > nlsearch::QUERY_MAX {
        return Ok(None);
    }
    let cfg = AiConfig::load(&state.store, state.account);
    let profile = ai_profile(&cfg, cfg.parse.as_deref(), "разбора запросов")?;
    let limit = limit_of(limit);
    let tz = TimeZone::system();
    let today = Timestamp::now().to_zoned(tz.clone()).date();
    let started = Instant::now();

    // Относительные даты считаются от сегодня, поэтому разбор помнится в пределах дня.
    let cache_key = format!("{today}|{}|{}|{}", profile.id, profile.model, q.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" "));
    let remembered = ai.refine_cached(&cache_key);
    let cached = remembered.is_some();
    let parsed = match remembered {
        Some(p) => p,
        None => {
            let cancel = ai.start_refine();
            let key = key_get(&profile.id);
            let request = ChatRequest {
                feature: "разбор",
                system: nlsearch::SYSTEM.into(),
                messages: vec![Message::user(nlsearch::user_message(&q, &format!("{} ({today})", ru_today(&tz))))],
                max_tokens: REFINE_MAX_TOKENS,
                effort: (profile.kind == ProviderKind::Anthropic).then_some(Effort::Low),
                json_schema: Some(JsonSchema { name: "search_query", schema: nlsearch::schema() }),
            };
            let (value, _) = ai.gateway.chat_json(&profile, key.as_ref(), cfg.mode, &request, &cancel).await.map_err(|e| e.to_string())?;
            let parsed = nlsearch::parse(&value)?;
            ai.refine_remember(cache_key, parsed.clone());
            parsed
        }
    };
    if parsed.is_empty() {
        return Ok(None);
    }
    let store = &state.store;
    let composed = store.compose_query(state.account, &model_fields(&parsed), today, tz.clone()).map_err(err)?;
    if composed.operators.trim().is_empty() {
        return Ok(None);
    }
    let refined = store.search_at(state.account, &composed.operators, limit, today, tz.clone()).map_err(err)?;
    // Те же письма, что нашёл разбор без модели, — предлагать нечего.
    let plain = store.search_at(state.account, &q, limit, today, tz).map_err(err)?;
    let ids = |o: &SearchOutcome| o.hits.iter().map(|h| h.item_pk).collect::<Vec<_>>();
    let same = ids(&plain) == ids(&refined);
    info!(cached, same, labels = composed.labels.len(), hits = refined.hits.len(), plain_hits = plain.hits.len(), ms = started.elapsed().as_millis(), "доводка запроса моделью");
    if same {
        return Ok(None);
    }
    Ok(Some(RefinedDto { operators: composed.operators, labels: composed.labels, result: search_dto(refined), ms: started.elapsed().as_millis() as u64, cached }))
}

/// Запрос в строке поиска изменился: идущая доводка больше не нужна.
#[tauri::command]
pub(crate) fn search_refine_cancel(ai: State<'_, AiState>) {
    ai.cancel_refine();
}

// --- «Кратко по найденному» ------------------------------------------------------------------------

/// Просьба окна: запрос из строки поиска и верхние письма выдачи.
#[derive(Debug, Deserialize)]
pub(crate) struct DigestRequest {
    /// Номер просьбы (окно создаёт сам): по нему — отмена.
    request_id: String,
    query: String,
    /// Письма выдачи в порядке показа; читаются первые [`DIGEST_MAX`].
    item_pks: Vec<i64>,
    /// Сколько писем нашёл поиск всего.
    total: usize,
}

/// События «Кратко» в окно.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum DigestEvent {
    /// Догружаются тексты писем, которых нет в базе.
    Progress { done: usize, total: usize },
    /// Контекст собран, обращение к модели пошло. Счётчики считает код, не модель.
    Context { messages: Vec<CtxRef>, truncated: bool, chars: usize, first_ms: Option<i64>, last_ms: Option<i64>, senders: usize },
    /// Кусок ответа модели.
    Text { text: String },
    /// Ответ закончен.
    Done { stop: String, cited: Vec<i64>, input_tokens: Option<u64>, output_tokens: Option<u64>, ms: u64 },
}

/// Собранный промпт без отправки: «Что уйдёт в модель».
#[derive(Debug, Serialize)]
pub(crate) struct DigestPreviewDto {
    system: String,
    user: String,
    chars: usize,
    truncated: bool,
    /// Сколько писем попало в промпт.
    read: usize,
}

struct Collected {
    input: DigestInput,
    refs: Vec<CtxRef>,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
    senders: usize,
}

/// Письма для модели: первые [`DIGEST_MAX`] из выдачи, от старых к новым; тела, которых нет в базе,
/// догружаются с сервера (первая ошибка связи останавливает догрузку — дальше по началу письма).
async fn collect(req: &DigestRequest, state: &AppState, sync: &SyncState, od: &OnDemand, place: Place, progress: &(dyn Fn(usize, usize) + Send + Sync)) -> CmdResult<Collected> {
    let store = state.store.clone();
    let mut seen = HashSet::new();
    let pks: Vec<i64> = req.item_pks.iter().copied().filter(|pk| seen.insert(*pk)).take(DIGEST_MAX).collect();
    let mut heads = Vec::with_capacity(pks.len());
    for pk in &pks {
        if let Some(h) = store.message_head(*pk).map_err(err)? {
            heads.push(h);
        }
    }
    if heads.is_empty() {
        return Err("Найденных писем уже нет в базе: повторите поиск".into());
    }
    let missing: Vec<i64> = heads.iter().filter(|h| h.body_state == 0).map(|h| h.item_pk).collect();
    for (i, pk) in missing.iter().enumerate() {
        progress(i, missing.len());
        if let Err(e) = ensure_body(state, sync, od, *pk).await {
            warn!(error = %e, "«Кратко»: текст письма не догружен, остальные — по тому, что есть");
            break;
        }
    }
    if !missing.is_empty() {
        // После догрузки у писем появились тела и получатели: шапки перечитываются.
        heads.clear();
        for pk in &pks {
            if let Some(h) = store.message_head(*pk).map_err(err)? {
                heads.push(h);
            }
        }
    }
    heads.sort_by_key(|h| (h.received_at.or(h.sent_at).map(|t| t.as_millisecond()).unwrap_or(0), h.item_pk));

    let own = own_addresses_of(&store, state.account, &state.user)?;
    let tz = TimeZone::system();
    let mut messages = Vec::with_capacity(heads.len());
    let mut refs = Vec::with_capacity(heads.len());
    let mut senders = HashSet::new();
    for h in &heads {
        let (m, r) = context_message(&store, h, &own, &tz)?;
        senders.insert(h.from_email.as_deref().or(h.from_name.as_deref()).unwrap_or("").to_lowercase());
        messages.push(m);
        refs.push(r);
    }
    let when = |h: &aiowa_store::ui::MessageHead| h.received_at.or(h.sent_at).map(|t| t.as_millisecond());
    let input = DigestInput { query: req.query.trim().to_string(), today: ru_today(&tz), total: req.total.max(messages.len()), messages, budget_chars: if place == Place::Cloud { BUDGET_CLOUD } else { BUDGET_OWN } };
    Ok(Collected { input, refs, first_ms: heads.first().and_then(when), last_ms: heads.last().and_then(when), senders: senders.len() })
}

#[tauri::command]
pub(crate) async fn search_digest_preview(req: DigestRequest, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>) -> CmdResult<DigestPreviewDto> {
    let cfg = AiConfig::load(&state.store, state.account);
    let place = cfg.advice.as_deref().and_then(|id| cfg.profile(id).ok()).map(|p| p.place()).unwrap_or(Place::Cloud);
    let ctx = collect(&req, &state, &sync, &od, place, &|_, _| {}).await?;
    let p = digest::build(&ctx.input);
    Ok(DigestPreviewDto { chars: p.user.chars().count(), read: p.message_ids.len(), system: p.system, user: p.user, truncated: p.truncated })
}

/// «Кратко по найденному»: письма выдачи → модель (поток в окно) → ссылки на письма.
#[tauri::command]
pub(crate) async fn search_digest(req: DigestRequest, on_event: Channel<DigestEvent>, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>, ai: State<'_, AiState>) -> CmdResult<()> {
    let cfg = AiConfig::load(&state.store, state.account);
    let profile = ai_profile(&cfg, cfg.advice.as_deref(), "«Совета»")?;
    let key = key_get(&profile.id);
    let cancel = ai.start_advice(&req.request_id);
    let result = async {
        let channel = on_event.clone();
        let progress = move |done: usize, total: usize| {
            let _ = channel.send(DigestEvent::Progress { done, total });
        };
        let ctx = collect(&req, &state, &sync, &od, profile.place(), &progress).await?;
        if cancel.is_cancelled() {
            return Err(aiowa_ai::AiError::Cancelled.to_string());
        }
        let prompt = digest::build(&ctx.input);
        let _ = on_event.send(DigestEvent::Context {
            messages: ctx.refs.iter().filter(|r| prompt.message_ids.contains(&r.id)).cloned().collect(),
            truncated: prompt.truncated,
            chars: prompt.user.chars().count(),
            first_ms: ctx.first_ms,
            last_ms: ctx.last_ms,
            senders: ctx.senders,
        });
        let chat = ChatRequest {
            feature: "кратко",
            system: prompt.system.clone(),
            messages: vec![Message::user(prompt.user.clone())],
            max_tokens: DIGEST_MAX_TOKENS,
            effort: (profile.kind == ProviderKind::Anthropic).then_some(Effort::Medium),
            json_schema: None,
        };
        let started = Instant::now();
        let channel = on_event.clone();
        let mut on_text = move |t: &str| {
            let _ = channel.send(DigestEvent::Text { text: t.to_string() });
        };
        let out = ai.gateway.chat(&profile, key.as_ref(), cfg.mode, &chat, &mut on_text, &cancel).await.map_err(|e| e.to_string())?;
        let stop = match out.stop {
            StopReason::End => "end",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
            StopReason::Other(_) => "other",
        };
        let _ = on_event.send(DigestEvent::Done { stop: stop.into(), cited: ai_advice::cited(&out.text, &prompt.message_ids), input_tokens: out.input_tokens, output_tokens: out.output_tokens, ms: started.elapsed().as_millis() as u64 });
        info!(messages = prompt.message_ids.len(), truncated = prompt.truncated, stop, "«Кратко по найденному» готово");
        Ok(())
    }
    .await;
    ai.finish_advice(&req.request_id);
    result
}

#[tauri::command]
pub(crate) fn search_digest_cancel(request_id: String, ai: State<'_, AiState>) {
    ai.cancel_advice(&request_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_model_answer_to_store_fields() {
        let p = nlsearch::Parsed {
            from: vec!["Иванов".into()],
            date_from: Some("2026-10-01".into()),
            date_to: Some("2026-10-31".into()),
            attachment_kind: Some("sheet".into()),
            has_attachment: true,
            folder: Some("sent".into()),
            unread: true,
            words: vec!["ремонт".into()],
            ..nlsearch::Parsed::default()
        };
        let f = model_fields(&p);
        assert_eq!(f.from, ["Иванов"]);
        // Конец периода модели включительно: у поиска граница — следующий день.
        assert_eq!(f.period, Some(Period { start: Some(jiff::civil::date(2026, 10, 1)), end: Some(jiff::civil::date(2026, 11, 1)) }));
        assert_eq!((f.attachment_kind, f.folder.as_deref(), f.unread, f.flagged), (Some(AttachmentKind::Sheet), Some("sentitems"), true, false));
        // Несуществующая дата (31 февраля) прошла проверку вида, но не календаря — граница снята.
        let p = nlsearch::Parsed { date_from: Some("2026-02-31".into()), date_to: Some("2026-03-05".into()), ..nlsearch::Parsed::default() };
        assert_eq!(model_fields(&p).period, Some(Period { start: None, end: Some(jiff::civil::date(2026, 3, 6)) }));
        assert_eq!(model_fields(&nlsearch::Parsed::default()), ModelFields::default());
    }

    #[test]
    fn ai_use_reasons() {
        let off = AiConfig::from_lookup(|_| None);
        let u = ai_use(&off, None, "разбора запросов");
        assert!(!u.available && u.reason.as_deref() == Some("ИИ-функции выключены: Настройки → ИИ"));
        assert!(ai_profile(&off, None, "разбора запросов").is_err());
    }
}
