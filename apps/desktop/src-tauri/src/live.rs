//! Живое окно во время синхронизации. Движок сообщает о каждой записи в базу по ходу прогона
//! (дерево папок, свежая страница, пачка писем с изменениями), `sync.rs` передаёт эти события
//! сюда, а окну они уходят одним событием с прореживанием — не чаще раза в `MIN_GAP`:
//!
//! - `sync-live { folders, folder_pks }` — что изменилось с прошлого события: дерево папок и
//!   счётчики (окно перечитывает дерево) и папки, в которых записаны письма (окно мягко
//!   перечитывает открытый список).
//!
//! `folders-changed` отсюда не шлётся: дерево по ходу прогона окно перечитывает по
//! `sync-live { folders: true }`, а `folders-changed` остаётся за командами папок (`folders.rs`) и
//! концом прогона «только дерево» (`sync.rs`). Иначе такой прогон давал окну два `folders-changed`
//! подряд: одно по записи дерева, другое по концу.
//!
//! Пока идёт пауза, изменения копятся, и последнее не теряется: отложенная отправка в конце паузы
//! уносит всё накопленное. По концу прогона накопленное сбрасывается — окно и так перечитывает всё.

use serde::Serialize;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tracing::warn;

/// Наименьший промежуток между двумя событиями `sync-live`.
pub(crate) const MIN_GAP: Duration = Duration::from_millis(2500);

/// Событие `sync-live`: что перечитать окну.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SyncLiveDto {
    /// Дерево папок и счётчики записаны с прошлого события.
    pub folders: bool,
    /// Папки, в которых записаны письма (ключи локальной базы, по возрастанию).
    pub folder_pks: Vec<i64>,
}

/// Решение прореживателя на новое событие.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fire {
    /// Отправить сейчас.
    Now,
    /// Пауза ещё идёт: назначить отложенную отправку через столько.
    Later(Duration),
    /// Отложенная отправка уже назначена, событие уйдёт с ней.
    Queued,
}

/// Прореживатель событий. Время передаётся снаружи: так логику проще проверить.
#[derive(Debug, Default)]
struct Gate {
    last: Option<Instant>,
    scheduled: bool,
}

impl Gate {
    fn offer(&mut self, now: Instant, gap: Duration) -> Fire {
        if self.scheduled {
            return Fire::Queued;
        }
        match self.last.map(|t| now.saturating_duration_since(t)) {
            Some(since) if since < gap => {
                self.scheduled = true;
                Fire::Later(gap - since)
            }
            _ => {
                self.last = Some(now);
                Fire::Now
            }
        }
    }

    /// Отложенная отправка сработала.
    fn fired(&mut self, now: Instant) {
        self.scheduled = false;
        self.last = Some(now);
    }
}

#[derive(Debug, Default)]
struct Inner {
    gate: Gate,
    /// Накоплено для `sync-live`.
    tree: bool,
    folder_pks: BTreeSet<i64>,
}

impl Inner {
    /// Дерево записано: отметить и спросить прореживатель.
    fn tree_applied(&mut self, now: Instant) -> Fire {
        self.tree = true;
        self.gate.offer(now, MIN_GAP)
    }

    /// Письма папки записаны: отметить и спросить прореживатель.
    fn items_applied(&mut self, folder_pk: i64, now: Instant) -> Fire {
        self.folder_pks.insert(folder_pk);
        self.gate.offer(now, MIN_GAP)
    }

    /// Накопленное для `sync-live`; `None` — отправлять нечего.
    fn take_live(&mut self) -> Option<SyncLiveDto> {
        if !self.tree && self.folder_pks.is_empty() {
            return None;
        }
        let dto = SyncLiveDto { folders: self.tree, folder_pks: std::mem::take(&mut self.folder_pks).into_iter().collect() };
        self.tree = false;
        Some(dto)
    }
}

/// События «по ходу прогона» для окна; один на процесс, живёт в `SyncState`.
#[derive(Default)]
pub(crate) struct LiveEvents {
    inner: Mutex<Inner>,
}

impl LiveEvents {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Внутри только флаги и ключи папок: отравленный мьютекс не страшен.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Дерево папок записано в базу: уйдёт в `sync-live` как `folders: true`.
    pub(crate) fn folders_applied(self: &Arc<Self>, app: &AppHandle) {
        let fire = self.lock().tree_applied(Instant::now());
        self.dispatch(app, fire);
    }

    /// Письма папки записаны в базу.
    pub(crate) fn items_applied(self: &Arc<Self>, app: &AppHandle, folder_pk: i64) {
        let fire = self.lock().items_applied(folder_pk, Instant::now());
        self.dispatch(app, fire);
    }

    /// Прогон закончился: окно по его концу перечитывает всё само, накопленное больше не нужно
    /// (отложенная отправка, если назначена, ничего не найдёт).
    pub(crate) fn finish(&self) {
        let mut g = self.lock();
        g.tree = false;
        g.folder_pks.clear();
    }

    fn dispatch(self: &Arc<Self>, app: &AppHandle, fire: Fire) {
        match fire {
            Fire::Now => self.emit(app),
            Fire::Queued => {}
            Fire::Later(delay) => {
                let this = self.clone();
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(delay).await;
                    this.lock().gate.fired(Instant::now());
                    this.emit(&app);
                });
            }
        }
    }

    fn emit(&self, app: &AppHandle) {
        let Some(dto) = self.lock().take_live() else { return };
        if let Err(e) = app.emit("sync-live", dto) {
            warn!(error = %e, "не удалось отправить событие хода синхронизации");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_sends_first_at_once_then_one_trailing_per_gap() {
        let mut g = Gate::default();
        let t0 = Instant::now();
        assert_eq!(g.offer(t0, MIN_GAP), Fire::Now);
        // Пачки чаще паузы: одна отложенная отправка на конец паузы, остальные входят в неё.
        assert_eq!(g.offer(t0 + Duration::from_millis(500), MIN_GAP), Fire::Later(MIN_GAP - Duration::from_millis(500)));
        assert_eq!(g.offer(t0 + Duration::from_millis(900), MIN_GAP), Fire::Queued);
        g.fired(t0 + MIN_GAP);
        // Сразу после отложенной — снова пауза.
        assert_eq!(g.offer(t0 + MIN_GAP + Duration::from_millis(100), MIN_GAP), Fire::Later(MIN_GAP - Duration::from_millis(100)));
        g.fired(t0 + MIN_GAP * 2);
        // Тишина дольше паузы: следующее уходит сразу.
        assert_eq!(g.offer(t0 + MIN_GAP * 5, MIN_GAP), Fire::Now);
    }

    #[test]
    fn live_payload_accumulates_and_is_taken_once() {
        let mut inner = Inner::default();
        assert_eq!(inner.take_live(), None);
        inner.folder_pks.extend([7, 3, 7]);
        inner.tree = true;
        assert_eq!(inner.take_live(), Some(SyncLiveDto { folders: true, folder_pks: vec![3, 7] }));
        assert_eq!(inner.take_live(), None, "накопленное уходит один раз");
        inner.folder_pks.insert(5);
        assert_eq!(inner.take_live(), Some(SyncLiveDto { folders: false, folder_pks: vec![5] }));
    }

    #[test]
    fn tree_and_items_share_one_event() {
        // Дерево и письма идут одним `sync-live` через один прореживатель: запись дерева не даёт
        // отдельного события, а прогон «только дерево» — ровно одно `sync-live { folders: true }`.
        let mut inner = Inner::default();
        let t0 = Instant::now();
        assert_eq!(inner.tree_applied(t0), Fire::Now);
        assert_eq!(inner.take_live(), Some(SyncLiveDto { folders: true, folder_pks: vec![] }));
        // Свежая страница сразу за деревом — в паузе, уйдёт отложенной вместе со следующими.
        assert_eq!(inner.items_applied(4, t0 + Duration::from_millis(300)), Fire::Later(MIN_GAP - Duration::from_millis(300)));
        assert_eq!(inner.tree_applied(t0 + Duration::from_millis(400)), Fire::Queued);
        inner.gate.fired(t0 + MIN_GAP);
        assert_eq!(inner.take_live(), Some(SyncLiveDto { folders: true, folder_pks: vec![4] }));
    }
}
