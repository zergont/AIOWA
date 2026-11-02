//! Просмотр вложений отдельным окном Windows (docs/obkatka.md п. 31): картинки, PDF, текст и
//! вложенные письма открываются не слоем поверх главного окна, а своим окном — его можно двигать,
//! растягивать, разворачивать и уносить на другой монитор, а рядом читать почту.
//!
//! Окно одно на письмо: клик по другому вложению того же письма переключает и поднимает уже
//! открытое (`attachment-show`). Имя окна — `att-<письмо>-<номер>`: проверка навигации помнит окна
//! по имени и повторную загрузку страницы в то же имя отменяет, поэтому новое окно — новое имя.
//! Страница — та же, что у главного окна, с `?view=attachments` (`main.ts`); права окна —
//! `capabilities/attachments.json`. Окна просмотра закрываются вместе с главным (`lib.rs`),
//! размер и место последнего закрытого запоминаются (`remember_geometry`).

use crate::{err, prepare_attachments, AppState, AttachmentDto, CmdResult};
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tracing::{info, warn};

/// Префикс имён окон просмотра (`capabilities/attachments.json`: `att-*`).
pub(crate) const LABEL_PREFIX: &str = "att-";
/// Событие окну просмотра: показать вложение (`attachment_pk`).
const SHOW_EVENT: &str = "attachment-show";
/// Где и какого размера было последнее закрытое окно просмотра: `x,y,ширина,высота,развёрнуто`
/// в логических пикселях (`sync_state`).
const GEOMETRY_KEY: &str = "attachment_window_geometry";
const DEFAULT_SIZE: (f64, f64) = (1000.0, 760.0);
const MIN_SIZE: (f64, f64) = (480.0, 360.0);

static NEXT: AtomicU64 = AtomicU64::new(1);

/// Размер и место окна просмотра.
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

/// Открыть (или поднять уже открытое) окно просмотра вложений письма `item_pk` на вложении `attachment_pk`.
#[tauri::command]
pub(crate) async fn attachment_window(item_pk: i64, attachment_pk: i64, app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    let row = state.store.attachment_of_account(state.account, attachment_pk).map_err(err)?.ok_or_else(|| "Вложение не найдено".to_string())?;
    if row.item_pk != item_pk {
        return Err("Вложение не из этого письма".into());
    }
    let prefix = format!("{LABEL_PREFIX}{item_pk}-");
    if let Some((label, window)) = app.webview_windows().into_iter().find(|(l, _)| l.starts_with(&prefix)) {
        app.emit_to(label.as_str(), SHOW_EVENT, attachment_pk).map_err(err)?;
        let _ = window.unminimize();
        let _ = window.set_focus();
        return Ok(());
    }
    let label = format!("{prefix}{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let url = WebviewUrl::App(format!("index.html?view=attachments&item={item_pk}&att={attachment_pk}").into());
    let title = row.name.clone().unwrap_or_else(|| "Вложение".into());
    let saved = state.store.get_state(state.account, GEOMETRY_KEY).ok().flatten().as_deref().and_then(Geometry::parse).and_then(|g| on_screen(&app, g));
    let (w, h) = saved.map(|g| (g.w, g.h)).unwrap_or(DEFAULT_SIZE);
    let mut builder = WebviewWindowBuilder::new(&app, &label, url).title(&title).decorations(false).inner_size(w, h).min_inner_size(MIN_SIZE.0, MIN_SIZE.1).disable_drag_drop_handler();
    builder = match saved {
        Some(g) => builder.position(g.x, g.y),
        None => builder.center(),
    };
    let window = builder.build().map_err(|e| {
        warn!(error = %e, "окно просмотра вложения не создано");
        "Не удалось открыть окно просмотра".to_string()
    })?;
    if saved.is_some_and(|g| g.maximized) {
        let _ = window.maximize();
    }
    info!(item_pk, attachment_pk, "окно просмотра вложения открыто");
    Ok(())
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

/// Окно просмотра закрывается: запомнить его размер и место для следующего (в логических пикселях,
/// развёрнутое — флагом, размер тогда прежний).
pub(crate) fn remember_geometry(window: &tauri::Window) {
    let Some(state) = window.try_state::<AppState>() else { return };
    let maximized = window.is_maximized().unwrap_or(false);
    let previous = state.store.get_state(state.account, GEOMETRY_KEY).ok().flatten().as_deref().and_then(Geometry::parse);
    let g = if maximized {
        match previous {
            Some(p) => Geometry { maximized: true, ..p },
            None => return,
        }
    } else {
        let (Ok(scale), Ok(pos), Ok(size)) = (window.scale_factor(), window.outer_position(), window.inner_size()) else { return };
        Geometry { x: pos.x as f64 / scale, y: pos.y as f64 / scale, w: size.width as f64 / scale, h: size.height as f64 / scale, maximized: false }
    };
    if let Err(e) = state.store.set_state(state.account, GEOMETRY_KEY, Some(&g.format())) {
        warn!(error = %e, "размер окна просмотра не записан");
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
}
