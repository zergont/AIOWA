//! Отдельные окна Windows рядом с главным: просмотр вложений (docs/obkatka.md п. 31), письмо и
//! черновик (п. 32). Окно можно двигать, растягивать, разворачивать и уносить на другой монитор,
//! а в главном окне тем временем работать.
//!
//! Страница — та же, что у главного окна, с `?view=…` (`main.ts`): `attachments` (вложения письма),
//! `message` (беседа), `compose` (письмо в работе). Права окон — `capabilities/windows.json`.
//! Окна закрываются вместе с главным (`lib.rs`), размер и место последнего закрытого окна каждого
//! вида запоминаются (`remember_geometry`) и при открытии вписываются в свой монитор.
//!
//! Имена окон: `att-<письмо>-<номер>`, `msg-<беседа>-<номер>`, `cmp-<номер>-<локальный черновик>`.
//! Проверка навигации помнит окна по имени и повторную загрузку страницы в то же имя отменяет,
//! поэтому новое окно — всегда новое имя, а уже открытое находится по началу имени и поднимается.

use crate::{err, prepare_attachments, AppState, AttachmentDto, CmdResult};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tracing::{info, warn};

/// Событие окну просмотра: показать вложение (`attachment_pk`).
const SHOW_EVENT: &str = "attachment-show";
const MIN_SIZE: (f64, f64) = (480.0, 360.0);

static NEXT: AtomicU64 = AtomicU64::new(1);

/// Окна черновиков с сервера, открытые сейчас: письмо (`item_pk`) → имя окна. Повторное открытие
/// того же черновика поднимает окно, а не создаёт второе.
static OPEN_DRAFTS: Mutex<Option<HashMap<i64, String>>> = Mutex::new(None);

/// Вид отдельного окна: начало имени, ключ запомненного размера в `sync_state`, размер по умолчанию.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Attachments,
    Message,
    Compose,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Attachments, Kind::Message, Kind::Compose];

    fn prefix(self) -> &'static str {
        match self {
            Kind::Attachments => "att-",
            Kind::Message => "msg-",
            Kind::Compose => "cmp-",
        }
    }

    /// Где и какого размера было последнее закрытое окно этого вида: `x,y,ширина,высота,развёрнуто`
    /// в логических пикселях.
    fn geometry_key(self) -> &'static str {
        match self {
            Kind::Attachments => "attachment_window_geometry",
            Kind::Message => "message_window_geometry",
            Kind::Compose => "compose_window_geometry",
        }
    }

    fn default_size(self) -> (f64, f64) {
        match self {
            Kind::Attachments => (1000.0, 760.0),
            Kind::Message => (900.0, 820.0),
            Kind::Compose => (860.0, 760.0),
        }
    }

    fn of_label(label: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| label.starts_with(k.prefix()))
    }
}

/// Размер и место окна.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Geometry {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    maximized: bool,
}

impl Geometry {
    fn parse(s: &str) -> Option<Self> {
        let v: Vec<&str> = s.split(',').collect();
        let [x, y, w, h, m] = v.as_slice() else { return None };
        let g = Self { x: x.parse().ok()?, y: y.parse().ok()?, w: w.parse().ok()?, h: h.parse().ok()?, maximized: *m == "1" };
        (g.w >= MIN_SIZE.0 && g.h >= MIN_SIZE.1 && g.x.is_finite() && g.y.is_finite()).then_some(g)
    }

    fn format(&self) -> String {
        format!("{:.0},{:.0},{:.0},{:.0},{}", self.x, self.y, self.w, self.h, u8::from(self.maximized))
    }
}

/// Запомненное окно на мониторе, где лежит его полоса заголовка: размер не больше монитора, низ не
/// за краем. Полоса ни на одном мониторе (монитор отключили) — `None`, окно откроется по центру.
fn on_screen(app: &AppHandle, g: Geometry) -> Option<Geometry> {
    let monitors = app.available_monitors().ok()?;
    let rects = monitors.iter().map(|m| {
        let scale = m.scale_factor();
        (m.position().x as f64 / scale, m.position().y as f64 / scale, m.size().width as f64 / scale, m.size().height as f64 / scale)
    });
    let found = rects.into_iter().find(|&(mx, my, mw, mh)| {
        let (px, py) = (g.x + 80.0, g.y + 16.0);
        px >= mx && px < mx + mw && py >= my && py < my + mh
    })?;
    Some(fit(g, found))
}

/// Окно в пределах монитора `(x, y, ширина, высота)`: не больше его и не ниже его нижнего края.
fn fit(g: Geometry, (mx, my, mw, mh): (f64, f64, f64, f64)) -> Geometry {
    let w = g.w.min(mw).max(MIN_SIZE.0);
    let h = g.h.min(mh).max(MIN_SIZE.1);
    let x = g.x.min(mx + mw - w).max(mx);
    let y = g.y.min(my + mh - h).max(my);
    Geometry { x, y, w, h, ..g }
}

/// Уже открытое окно, чьё имя начинается с `prefix`: поднять; его имя — если нашлось.
fn raise(app: &AppHandle, prefix: &str) -> Option<String> {
    raise_where(app, |l| l.starts_with(prefix))
}

fn raise_where(app: &AppHandle, pred: impl Fn(&str) -> bool) -> Option<String> {
    let (label, window) = app.webview_windows().into_iter().find(|(l, _)| pred(l))?;
    let _ = window.unminimize();
    let _ = window.set_focus();
    Some(label)
}

/// Создать окно вида `kind` с именем `label` на странице `query` (`index.html?…`), размер и место —
/// запомненные для этого вида.
fn open(app: &AppHandle, state: &AppState, kind: Kind, label: &str, query: &str, title: &str) -> CmdResult<()> {
    let url = WebviewUrl::App(format!("index.html?{query}").into());
    let saved = state.store.get_state(state.account, kind.geometry_key()).ok().flatten().as_deref().and_then(Geometry::parse).and_then(|g| on_screen(app, g));
    let (w, h) = saved.map(|g| (g.w, g.h)).unwrap_or(kind.default_size());
    let mut builder = WebviewWindowBuilder::new(app, label, url).title(title).decorations(false).inner_size(w, h).min_inner_size(MIN_SIZE.0, MIN_SIZE.1).disable_drag_drop_handler();
    builder = match saved {
        Some(g) => builder.position(g.x, g.y),
        None => builder.center(),
    };
    let window = builder.build().map_err(|e| {
        warn!(error = %e, ?kind, "отдельное окно не создано");
        "Не удалось открыть отдельное окно".to_string()
    })?;
    if saved.is_some_and(|g| g.maximized) {
        let _ = window.maximize();
    }
    Ok(())
}

/// Открыть (или поднять уже открытое) окно просмотра вложений письма `item_pk` на вложении `attachment_pk`.
#[tauri::command]
pub(crate) async fn attachment_window(item_pk: i64, attachment_pk: i64, app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    let row = state.store.attachment_of_account(state.account, attachment_pk).map_err(err)?.ok_or_else(|| "Вложение не найдено".to_string())?;
    if row.item_pk != item_pk {
        return Err("Вложение не из этого письма".into());
    }
    let prefix = format!("{}{item_pk}-", Kind::Attachments.prefix());
    if let Some(label) = raise(&app, &prefix) {
        return app.emit_to(label.as_str(), SHOW_EVENT, attachment_pk).map_err(err);
    }
    let label = format!("{prefix}{}", NEXT.fetch_add(1, Ordering::Relaxed));
    open(&app, &state, Kind::Attachments, &label, &format!("view=attachments&item={item_pk}&att={attachment_pk}"), row.name.as_deref().unwrap_or("Вложение"))?;
    info!(item_pk, attachment_pk, "окно просмотра вложения открыто");
    Ok(())
}

/// Открыть (или поднять уже открытое) окно беседы `conversation_pk` (п. 32): кнопка в области чтения
/// и двойной щелчок по беседе.
#[tauri::command]
pub(crate) async fn message_window(conversation_pk: i64, title: Option<String>, app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    let prefix = format!("{}{conversation_pk}-", Kind::Message.prefix());
    if raise(&app, &prefix).is_some() {
        return Ok(());
    }
    let label = format!("{prefix}{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let title = title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Письмо".into());
    open(&app, &state, Kind::Message, &label, &format!("view=message&conv={conversation_pk}"), &title)?;
    info!(conversation_pk, "окно беседы открыто");
    Ok(())
}

/// Локальный черновик (`compose_local_save`) годится в имя окна: UUID окна или похожее.
fn valid_local_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Открыть письмо в работе отдельным окном (п. 32): `local_id` — локальный черновик композера,
/// который окно поднимет (кнопка «Редактировать в отдельном окне»), или `draft_item_pk` —
/// черновик из «Черновиков» (двойной щелчок по нему). Окно с тем же письмом уже открыто — поднять.
#[tauri::command]
pub(crate) async fn compose_window(local_id: Option<String>, draft_item_pk: Option<i64>, app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(pk) = draft_item_pk {
        let open_label = OPEN_DRAFTS.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert_default().get(&pk).cloned();
        if let Some(label) = open_label
            && raise(&app, &label).is_some()
        {
            return Ok(());
        }
        let item = state.store.item_ref(pk).map_err(err)?.ok_or_else(|| "Черновик удалён".to_string())?;
        // Тот же черновик уже правится в окне, куда письмо перенесли из композера: его локальный
        // черновик ссылается на этот черновик сервера — поднять то окно, а не открывать второе.
        for id in local_drafts_in_windows(&app) {
            let same = state.store.local_draft(&id).ok().flatten().and_then(|d| d.draft_server_id).is_some_and(|s| s == item.id.id);
            let suffix = format!("-{id}");
            if same && raise_where(&app, |l| l.starts_with(Kind::Compose.prefix()) && l.ends_with(&suffix)).is_some() {
                return Ok(());
            }
        }
    }
    let id = match local_id {
        Some(id) if valid_local_id(&id) => id,
        Some(_) => return Err("Неверный локальный черновик".into()),
        None if draft_item_pk.is_some() => aiowa_transport::new_guid(),
        None => return Err("Не указано письмо для отдельного окна".into()),
    };
    let suffix = format!("-{id}");
    if raise_where(&app, |l| l.starts_with(Kind::Compose.prefix()) && l.ends_with(&suffix)).is_some() {
        return Ok(());
    }
    let label = format!("{}{}-{id}", Kind::Compose.prefix(), NEXT.fetch_add(1, Ordering::Relaxed));
    let query = match draft_item_pk {
        Some(pk) => format!("view=compose&local={id}&draft={pk}"),
        None => format!("view=compose&local={id}"),
    };
    open(&app, &state, Kind::Compose, &label, &query, "Письмо")?;
    if let Some(pk) = draft_item_pk {
        OPEN_DRAFTS.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert_default().insert(pk, label.clone());
    }
    info!(from_server_draft = draft_item_pk.is_some(), "окно письма в работе открыто");
    Ok(())
}

/// Окно черновика (`cmp-…`).
pub(crate) fn is_compose_window(label: &str) -> bool {
    Kind::of_label(label) == Some(Kind::Compose)
}

/// Локальные черновики, открытые сейчас в окнах черновиков (`cmp-<номер>-<черновик>`): полоса
/// «Несохранённое письмо» главного окна их не предлагает.
pub(crate) fn local_drafts_in_windows(app: &AppHandle) -> Vec<String> {
    app.webview_windows()
        .into_keys()
        .filter_map(|l| l.strip_prefix(Kind::Compose.prefix()).and_then(|rest| rest.split_once('-')).map(|(_, id)| id.to_string()))
        .collect()
}

/// Файловые вложения письма для окна просмотра (картинки текста письма — тоже: окно само
/// оставит только файлы, как карточка письма).
#[tauri::command]
pub(crate) fn item_attachments(item_pk: i64, state: State<'_, AppState>) -> CmdResult<Vec<AttachmentDto>> {
    state.store.item_ref(item_pk).map_err(err)?.ok_or_else(|| "Письмо удалено".to_string())?;
    let (rows, _) = prepare_attachments(&state.store, item_pk, None)?;
    Ok(rows
        .into_iter()
        .map(|a| AttachmentDto { attachment_pk: a.attachment_pk, name: a.name, content_type: a.content_type, size: a.size, is_inline: a.is_inline, cached: a.local_path.is_some(), content_id: a.content_id })
        .collect())
}

/// Отдельное окно закрывается: запомнить его размер и место для следующего окна того же вида (в
/// логических пикселях, развёрнутое — флагом, размер тогда прежний). Окно черновика с сервера
/// снимается с учёта открытых.
pub(crate) fn remember_geometry(window: &tauri::Window) {
    let Some(kind) = Kind::of_label(window.label()) else { return };
    if kind == Kind::Compose {
        let mut drafts = OPEN_DRAFTS.lock().unwrap_or_else(|p| p.into_inner());
        drafts.get_or_insert_default().retain(|_, l| l != window.label());
    }
    let Some(state) = window.try_state::<AppState>() else { return };
    let key = kind.geometry_key();
    let maximized = window.is_maximized().unwrap_or(false);
    let previous = state.store.get_state(state.account, key).ok().flatten().as_deref().and_then(Geometry::parse);
    let g = if maximized {
        match previous {
            Some(p) => Geometry { maximized: true, ..p },
            None => return,
        }
    } else {
        let (Ok(scale), Ok(pos), Ok(size)) = (window.scale_factor(), window.outer_position(), window.inner_size()) else { return };
        Geometry { x: pos.x as f64 / scale, y: pos.y as f64 / scale, w: size.width as f64 / scale, h: size.height as f64 / scale, maximized: false }
    };
    if let Err(e) = state.store.set_state(state.account, key, Some(&g.format())) {
        warn!(error = %e, "размер отдельного окна не записан");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_round_trip_and_rejects_garbage() {
        let g = Geometry { x: -1200.0, y: 40.0, w: 900.0, h: 700.0, maximized: true };
        assert_eq!(Geometry::parse(&g.format()), Some(g));
        assert_eq!(Geometry::parse("10,20,300,200,0"), None, "меньше минимального размера");
        assert_eq!(Geometry::parse("10,20,800"), None);
        assert_eq!(Geometry::parse("a,b,c,d,0"), None);
        assert_eq!(Geometry::parse("NaN,0,800,600,0"), None);
    }

    #[test]
    fn saved_window_is_fitted_to_its_monitor() {
        let monitor = (0.0, 0.0, 1720.0, 1080.0);
        let g = |x, y, w, h| Geometry { x, y, w, h, maximized: false };
        // Высота больше монитора — ужата, окно поднято к верху.
        assert_eq!(fit(g(260.0, 180.0, 880.0, 65526.0), monitor), g(260.0, 0.0, 880.0, 1080.0));
        // Низ за краем — окно поднято, размер прежний.
        assert_eq!(fit(g(100.0, 600.0, 900.0, 700.0), monitor), g(100.0, 380.0, 900.0, 700.0));
        // Помещается — как было; второй монитор слева (отрицательные координаты) — тоже.
        assert_eq!(fit(g(100.0, 100.0, 900.0, 700.0), monitor), g(100.0, 100.0, 900.0, 700.0));
        assert_eq!(fit(g(-1500.0, 50.0, 1000.0, 760.0), (-1560.0, 0.0, 1560.0, 1040.0)), g(-1500.0, 50.0, 1000.0, 760.0));
    }

    #[test]
    fn window_kinds_and_local_ids() {
        assert_eq!(Kind::of_label("att-5-1"), Some(Kind::Attachments));
        assert_eq!(Kind::of_label("msg-77-2"), Some(Kind::Message));
        assert_eq!(Kind::of_label("cmp-3-1b2c-9f"), Some(Kind::Compose));
        assert_eq!(Kind::of_label("main"), None);
        assert!(valid_local_id("8f14e45f-ceea-467a-9575-6e1b1a3bb1c2"));
        assert!(!valid_local_id(""));
        assert!(!valid_local_id("../x"));
        assert!(!valid_local_id("a b"));
    }
}
