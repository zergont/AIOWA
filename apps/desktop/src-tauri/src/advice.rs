//! «Совет» при ответе (`docs/ai-layer-design.md` § 4, шаг И2): контекст из локальной базы,
//! обращение к модели через шлюз с потоком в окно, проверка фактов черновика.
//!
//! Контекст собирается здесь, детерминированно: письма беседы (кроме черновиков) от старых к
//! новым — дата, отправитель и получатели с ролью «автор» или «собеседник», тема, видимый текст
//! (скрытые элементы и символы нулевой ширины вырезаны, `aiowa_ai::mailtext`), имена вложений;
//! тела последних писем, которых нет в базе, догружаются с сервера; мои прошлые письма главному
//! адресату — образец стиля. Промпт и бюджет — `aiowa_ai::advice`. Ответ модели идёт в окно
//! кусками (`Channel`), в конце — ссылки на письма и непроверенные числа и адреса.

use crate::ai::{key_get, AiConfig, AiState};
use crate::compose::own_addresses_of;
use crate::ondemand::{ensure_body, OnDemand};
use crate::sync::SyncState;
use crate::{err, AppState, CmdResult};
use aiowa_ai::advice::{self, AdviceInput, AdviceKind, CtxMessage, Person, Tone};
use aiowa_ai::mailtext::{strip_hidden_html, strip_invisible};
use aiowa_ai::{ChatRequest, Effort, Message, Place, ProviderKind, StopReason};
use aiowa_core::BodyType;
use aiowa_store::ui::MessageHead;
use aiowa_store::Store;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tauri::ipc::Channel;
use tauri::State;
use tracing::{info, warn};

/// Писем беседы самое большее (свежие); старше — не нужны и дорого собирать.
const THREAD_MAX: usize = 40;
/// Свежих писем, тела которых догружаются с сервера.
const FETCH_RECENT: usize = 5;
/// Примеров моего стиля.
const STYLE_EXAMPLES: usize = 3;
/// Бюджет сообщения с данными, символов: облако — до ~30 тыс. токенов, свои модели — ~13 тыс.
const BUDGET_CLOUD: usize = 90_000;
const BUDGET_OWN: usize = 40_000;
/// Предел длины ответа модели, токенов (суть и черновик; у моделей с размышлением — с запасом).
const MAX_TOKENS: u32 = 8192;

/// Просьба окна.
#[derive(Debug, Deserialize)]
pub(crate) struct AdviceRequest {
    /// Номер просьбы (окно создаёт сам): по нему — отмена.
    request_id: String,
    /// `reply` | `reply_all` | `forward`.
    kind: String,
    reference_item_pk: i64,
    #[serde(default)]
    direction: String,
    #[serde(default)]
    tones: Vec<String>,
    /// Текст, уже набранный в письме.
    #[serde(default)]
    typed_text: String,
}

/// Письмо, на которое модель может сослаться: номер и подпись для чипа.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CtxRef {
    id: i64,
    label: String,
}

/// События «Совета» в окно.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AdviceEvent {
    /// Контекст собран, обращение к модели пошло.
    Context { messages: Vec<CtxRef>, truncated: bool, chars: usize },
    /// Кусок ответа модели.
    Text { text: String },
    /// Ответ закончен.
    Done { stop: String, cited: Vec<i64>, unverified: Vec<String>, input_tokens: Option<u64>, output_tokens: Option<u64>, ms: u64 },
}

/// Доступен ли «Совет» и какая модель его даёт (подпись в панели).
#[derive(Debug, Serialize)]
pub(crate) struct AdviceStatusDto {
    available: bool,
    reason: Option<String>,
    profile_name: Option<String>,
    model: Option<String>,
    place: Option<Place>,
}

#[tauri::command(async)]
pub(crate) fn ai_advice_status(state: State<'_, AppState>) -> CmdResult<AdviceStatusDto> {
    let cfg = AiConfig::load(&state.store, state.account);
    let profile = cfg.advice.as_deref().and_then(|id| cfg.profile(id).ok());
    let reason = if !cfg.enabled {
        Some("ИИ-функции выключены: Настройки → ИИ")
    } else if profile.is_none() {
        Some("Не назначена модель для «Совета»: Настройки → ИИ")
    } else if profile.is_some_and(|p| p.model.trim().is_empty()) {
        Some("У подключения для «Совета» не выбрана модель")
    } else {
        None
    };
    Ok(AdviceStatusDto {
        available: reason.is_none(),
        reason: reason.map(str::to_string),
        profile_name: profile.map(|p| p.name.clone()),
        model: profile.map(|p| p.model.clone()),
        place: profile.map(|p| p.place()),
    })
}

/// Собранный промпт без отправки: «Что уйдёт в модель».
#[derive(Debug, Serialize)]
pub(crate) struct AdvicePreviewDto {
    system: String,
    user: String,
    chars: usize,
    truncated: bool,
}

#[tauri::command]
pub(crate) async fn ai_advice_preview(req: AdviceRequest, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>) -> CmdResult<AdvicePreviewDto> {
    let cfg = AiConfig::load(&state.store, state.account);
    let place = cfg.advice.as_deref().and_then(|id| cfg.profile(id).ok()).map(|p| p.place()).unwrap_or(Place::Cloud);
    let ctx = collect(&req, &state, &sync, &od, place).await?;
    let p = advice::build(&ctx.input);
    Ok(AdvicePreviewDto { chars: p.user.chars().count(), system: p.system, user: p.user, truncated: p.truncated })
}

/// «Посоветовать»: контекст → модель (поток в окно) → итог со ссылками и проверкой фактов.
#[tauri::command]
pub(crate) async fn ai_advice(req: AdviceRequest, on_event: Channel<AdviceEvent>, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, od: State<'_, OnDemand>, ai: State<'_, AiState>) -> CmdResult<()> {
    let cfg = AiConfig::load(&state.store, state.account);
    if !cfg.enabled {
        return Err("ИИ-функции выключены: Настройки → ИИ".into());
    }
    let profile = cfg.advice.as_deref().and_then(|id| cfg.profile(id).ok()).cloned().ok_or_else(|| "Не назначена модель для «Совета»: Настройки → ИИ".to_string())?;
    let key = key_get(&profile.id);
    let cancel = ai.start_advice(&req.request_id);
    let result = async {
        let ctx = collect(&req, &state, &sync, &od, profile.place()).await?;
        if cancel.is_cancelled() {
            return Err(aiowa_ai::AiError::Cancelled.to_string());
        }
        let prompt = advice::build(&ctx.input);
        let _ = on_event.send(AdviceEvent::Context { messages: ctx.refs.iter().filter(|r| prompt.message_ids.contains(&r.id)).cloned().collect(), truncated: prompt.truncated, chars: prompt.user.chars().count() });
        let chat = ChatRequest {
            feature: "совет",
            system: prompt.system.clone(),
            messages: vec![Message::user(prompt.user.clone())],
            max_tokens: MAX_TOKENS,
            effort: (profile.kind == ProviderKind::Anthropic).then_some(Effort::Medium),
        };
        let started = Instant::now();
        let channel = on_event.clone();
        let mut on_text = move |t: &str| {
            let _ = channel.send(AdviceEvent::Text { text: t.to_string() });
        };
        let out = ai.gateway.chat(&profile, key.as_ref(), cfg.mode, &chat, &mut on_text, &cancel).await.map_err(|e| e.to_string())?;
        let (summary, draft) = advice::split_output(&out.text);
        let stop = match out.stop {
            StopReason::End => "end",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
            StopReason::Other(_) => "other",
        };
        let _ = on_event.send(AdviceEvent::Done {
            stop: stop.into(),
            cited: advice::cited(&summary, &prompt.message_ids),
            unverified: advice::unverified(&draft, &ctx.facts),
            input_tokens: out.input_tokens,
            output_tokens: out.output_tokens,
            ms: started.elapsed().as_millis() as u64,
        });
        info!(messages = prompt.message_ids.len(), truncated = prompt.truncated, stop, "«Совет» готов");
        Ok(())
    }
    .await;
    ai.finish_advice(&req.request_id);
    result
}

#[tauri::command]
pub(crate) fn ai_advice_cancel(request_id: String, ai: State<'_, AiState>) {
    ai.cancel_advice(&request_id);
}

// --- контекст -------------------------------------------------------------------------------

struct Collected {
    input: AdviceInput,
    /// Подписи писем для чипов.
    refs: Vec<CtxRef>,
    /// Всё, с чем сверяются числа и адреса черновика: тексты писем, их шапки, слова автора.
    facts: String,
}

async fn collect(req: &AdviceRequest, state: &AppState, sync: &SyncState, od: &OnDemand, place: Place) -> CmdResult<Collected> {
    let kind = match req.kind.as_str() {
        "reply" => AdviceKind::Reply,
        "reply_all" => AdviceKind::ReplyAll,
        "forward" => AdviceKind::Forward,
        _ => return Err("«Совет» — только для ответа и пересылки".into()),
    };
    let store = state.store.clone();
    let account = state.account;
    let reference = req.reference_item_pk;
    let item = store.item_ref(reference).map_err(err)?.ok_or_else(|| "Исходное письмо удалено".to_string())?;

    // Письма беседы от старых к новым, без черновиков; исходное — всегда.
    let mut heads: Vec<MessageHead> = match item.conversation_pk {
        Some(c) => store.conversation_messages(account, c).map_err(err)?,
        None => store.message_head(reference).map_err(err)?.into_iter().collect(),
    };
    heads.retain(|h| !h.is_draft || h.item_pk == reference);
    if !heads.iter().any(|h| h.item_pk == reference)
        && let Some(h) = store.message_head(reference).map_err(err)?
    {
        heads.push(h);
    }
    heads.sort_by_key(|h| (h.received_at.or(h.sent_at).map(|t| t.as_millisecond()).unwrap_or(0), h.item_pk));
    if heads.len() > THREAD_MAX {
        let cut = heads.len() - THREAD_MAX;
        let keep_reference = heads[..cut].iter().position(|h| h.item_pk == reference).map(|i| heads[i].clone());
        heads.drain(..cut);
        if let Some(r) = keep_reference {
            heads.insert(0, r);
        }
    }

    // Тела свежих писем и исходного — с сервера, если их нет; первая ошибка связи останавливает догрузку.
    let n = heads.len();
    for (i, h) in heads.iter().enumerate() {
        if h.body_state != 0 || !(i + FETCH_RECENT >= n || h.item_pk == reference) {
            continue;
        }
        if let Err(e) = ensure_body(state, sync, od, h.item_pk).await {
            warn!(error = %e, "«Совет»: тело письма не догружено, остальные — по тому, что есть");
            break;
        }
    }

    let own = own_addresses_of(&store, account, &state.user)?;
    let tz = TimeZone::system();
    let mut messages = Vec::with_capacity(heads.len());
    let mut refs = Vec::with_capacity(heads.len());
    let mut facts = String::new();
    for h in &heads {
        let text = visible_text(&store, h)?;
        let recipients = store.recipients(h.item_pk).map_err(err)?;
        let person = |r: &aiowa_store::ui::RecipientRow| Person { name: r.name.clone(), email: r.email.clone() };
        let to: Vec<Person> = recipients.iter().filter(|r| r.kind == 1).map(person).collect();
        let cc: Vec<Person> = recipients.iter().filter(|r| r.kind == 2).map(person).collect();
        let attachments: Vec<String> = store.attachments_of_item(h.item_pk).map_err(err)?.into_iter().filter(|a| !a.is_inline).filter_map(|a| a.name).collect();
        let mine = h.from_email.as_deref().is_some_and(|e| own.iter().any(|o| o.eq_ignore_ascii_case(e)));
        let when = h.received_at.or(h.sent_at);
        let from = Person { name: h.from_name.clone(), email: h.from_email.clone() };
        refs.push(CtxRef { id: h.item_pk, label: chip_label(&from, when, &tz) });
        for p in std::iter::once(&from).chain(&to).chain(&cc) {
            facts.push_str(&format!("{} {}\n", p.name.as_deref().unwrap_or(""), p.email.as_deref().unwrap_or("")));
        }
        facts.push_str(&text);
        facts.push('\n');
        facts.push_str(h.subject.as_deref().unwrap_or(""));
        facts.push('\n');
        messages.push(CtxMessage { id: h.item_pk, date: when.map(|t| ru_datetime(t, &tz)).unwrap_or_default(), from, to, cc, subject: h.subject.clone().unwrap_or_default(), text, attachments, mine });
    }

    // Образец стиля: мои письма главному адресату (отправитель исходного, а если это я — первый получатель).
    let target = messages.iter().find(|m| m.id == reference).and_then(|m| if m.mine { m.to.first().and_then(|p| p.email.clone()) } else { m.from.email.clone() });
    let mut style = Vec::new();
    if let Some(email) = target {
        let thread: Vec<i64> = messages.iter().map(|m| m.id).collect();
        for pk in store.sent_to(account, &email, STYLE_EXAMPLES + 3).map_err(err)? {
            if thread.contains(&pk) || style.len() >= STYLE_EXAMPLES {
                continue;
            }
            if let Some(h) = store.message_head(pk).map_err(err)? {
                let t = visible_text(&store, &h)?;
                if !t.trim().is_empty() {
                    style.push(t);
                }
            }
        }
    }

    facts.push_str(&req.direction);
    facts.push('\n');
    facts.push_str(&req.typed_text);
    let input = AdviceInput {
        kind: Some(kind),
        reference_id: reference,
        me_name: store.own_display_name(account).map_err(err)?,
        me_addresses: own,
        today: ru_today(&tz),
        direction: req.direction.clone(),
        tones: req.tones.iter().filter_map(|t| Tone::parse(t)).collect(),
        typed: req.typed_text.clone(),
        messages,
        style,
        budget_chars: if place == Place::Cloud { BUDGET_CLOUD } else { BUDGET_OWN },
    };
    Ok(Collected { input, refs, facts })
}

/// Видимый текст письма без истории цитат; тела нет — начало письма из списка.
fn visible_text(store: &Store, h: &MessageHead) -> CmdResult<String> {
    let body = store.message_body(h.item_pk).map_err(err)?;
    let text = match body {
        Some(b) => match (b.body_type, b.unique.or(b.full)) {
            (BodyType::Html, Some(html)) => aiowa_store::text::html_to_text(&strip_hidden_html(&html)),
            (BodyType::Text, Some(t)) => t,
            (_, None) => b.text.unwrap_or_default(),
        },
        None => h.preview.clone().unwrap_or_default(),
    };
    Ok(tidy(&strip_invisible(&text)))
}

/// Без лишних пустых строк и пробелов по краям строк.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let l = line.trim();
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim().to_string()
}

const MONTHS: [&str; 12] = ["января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября", "октября", "ноября", "декабря"];
const MONTHS_SHORT: [&str; 12] = ["янв", "фев", "мар", "апр", "мая", "июн", "июл", "авг", "сен", "окт", "ноя", "дек"];
const WEEKDAYS: [&str; 7] = ["понедельник", "вторник", "среда", "четверг", "пятница", "суббота", "воскресенье"];

fn ru_datetime(t: Timestamp, tz: &TimeZone) -> String {
    let z = t.to_zoned(tz.clone());
    format!("{} {} {}, {:02}:{:02}", z.day(), MONTHS[(z.month() - 1) as usize], z.year(), z.hour(), z.minute())
}

fn ru_today(tz: &TimeZone) -> String {
    let z = Timestamp::now().to_zoned(tz.clone());
    let wd = z.weekday().to_monday_zero_offset() as usize;
    format!("{} {} {}, {}", z.day(), MONTHS[(z.month() - 1) as usize], z.year(), WEEKDAYS[wd])
}

/// Подпись чипа ссылки на письмо: «Иван Петров, 5 ноя».
fn chip_label(from: &Person, when: Option<Timestamp>, tz: &TimeZone) -> String {
    let who = from.name.as_deref().filter(|n| !n.trim().is_empty()).or(from.email.as_deref()).unwrap_or("письмо").trim().to_string();
    match when {
        Some(t) => {
            let z = t.to_zoned(tz.clone());
            format!("{who}, {} {}", z.day(), MONTHS_SHORT[(z.month() - 1) as usize])
        }
        None => who,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_dates_and_tidy() {
        let tz = TimeZone::fixed(jiff::tz::offset(3));
        let t: Timestamp = "2026-11-06T07:18:00Z".parse().unwrap();
        assert_eq!(ru_datetime(t, &tz), "6 ноября 2026, 10:18");
        assert_eq!(chip_label(&Person { name: Some("Иван Петров".into()), email: Some("i@x.ru".into()) }, Some(t), &tz), "Иван Петров, 6 ноя");
        assert_eq!(tidy("  Привет  \n\n\n\n  как дела \n"), "Привет\n\nкак дела");
    }
}
