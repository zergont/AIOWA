//! Живое окно во время синхронизации. Движок сообщает о каждой записи в базу по ходу прогона
//! (дерево папок, свежая страница, пачка писем с изменениями), `sync.rs` передаёт эти события
//! сюда, а окну они уходят с прореживанием — не чаще раза в `MIN_GAP` на тип события:
//!
//! - `folders-changed` — дерево папок записано (окно перечитывает дерево и счётчики);
//! - `sync-live { folders, folder_pks }` — что изменилось с прошлого события: дерево и папки,
//!   в которых записаны письма (окно мягко перечитывает открытый список).
//!
//! Пока идёт пауза, изменения копятся, и последнее не теряется: отложенная отправка в конце паузы
//! уносит всё накопленное. По концу прогона накопленное сбрасывается — окно и так перечитывает всё.

use serde::Serialize;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tracing::warn;

/// Наименьший промежуток между двумя событиями одного типа.
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

/// Прореживатель одного типа событий. Время передаётся снаружи: так логику проще проверить.
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

/// Тип события окна.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Folders,
    Items,
}

#[derive(Debug, Default)]
struct Inner {
    folders_gate: Gate,
    items_gate: Gate,
    /// Дерево записано, `folders-changed` ещё не ушло.
    folders_pending: bool,
    /// Накоплено для `sync-live`.
    tree: bool,
    folder_pks: BTreeSet<i64>,
}

impl Inner {
    fn gate(&mut self, kind: Kind) -> &mut Gate {
        match kind {
            Kind::Folders => &mut self.folders_gate,
            Kind::Items => &mut self.items_gate,
        }
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

    /// Дерево папок записано в базу.
    pub(crate) fn folders_applied(self: &Arc<Self>, app: &AppHandle) {
        let now = Instant::now();
        let (folders, items) = {
            let mut g = self.lock();
            g.folders_pending = true;
            g.tree = true;
            (g.folders_gate.offer(now, MIN_GAP), g.items_gate.offer(now, MIN_GAP))
        };
        self.dispatch(app, Kind::Folders, folders);
        self.dispatch(app, Kind::Items, items);
    }

    /// Письма папки записаны в базу.
    pub(crate) fn items_applied(self: &Arc<Self>, app: &AppHandle, folder_pk: i64) {
        let fire = {
            let mut g = self.lock();
            g.folder_pks.insert(folder_pk);
            g.items_gate.offer(Instant::now(), MIN_GAP)
        };
        self.dispatch(app, Kind::Items, fire);
    }

    /// Прогон закончился: окно по его концу перечитывает всё само, накопленное больше не нужно
    /// (отложенная отправка, если назначена, ничего не найдёт).
    pub(crate) fn finish(&self) {
        let mut g = self.lock();
        g.folders_pending = false;
        g.tree = false;
        g.folder_pks.clear();
    }

    fn dispatch(self: &Arc<Self>, app: &AppHandle, kind: Kind, fire: Fire) {
        match fire {
            Fire::Now => self.emit(app, kind),
            Fire::Queued => {}
            Fire::Later(delay) => {
                let this = self.clone();
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(delay).await;
                    this.lock().gate(kind).fired(Instant::now());
                    this.emit(&app, kind);
                });
            }
        }
    }

    fn emit(&self, app: &AppHandle, kind: Kind) {
        let result = match kind {
            Kind::Folders => {
                if !std::mem::take(&mut self.lock().folders_pending) {
                    return;
                }
                app.emit("folders-changed", ())
            }
            Kind::Items => {
                let Some(dto) = self.lock().take_live() else { return };
                app.emit("sync-live", dto)
            }
        };
        if let Err(e) = result {
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
}
