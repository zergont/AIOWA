//! Вид дерева папок, который переживает перезапуск: свёрнутые ветки. Хранятся в базе учётной
//! записи (`sync_state`, ключ `desktop.collapsed_folders`), а не в `localStorage` окна: база своя
//! у каждого ящика, а профиль WebView2 общий. В базе — серверные идентификаторы папок
//! (`Store::folder_list_state`), окну отдаются и от окна принимаются локальные ключи `folder_pk`;
//! удалённые на сервере папки из списка выбрасываются.

use crate::{err, AppState, CmdResult};
use tauri::State;
use tracing::debug;

/// Ключ в `sync_state`: JSON-массив серверных идентификаторов свёрнутых папок.
pub(crate) const COLLAPSED_FOLDERS_KEY: &str = "desktop.collapsed_folders";

/// Свёрнутые папки (локальные ключи) — окно читает их при старте, до первой отрисовки дерева.
#[tauri::command]
pub(crate) fn folders_collapsed_get(state: State<'_, AppState>) -> CmdResult<Vec<i64>> {
    let pks = state.store.folder_list_state(state.account, COLLAPSED_FOLDERS_KEY).map_err(err)?;
    debug!(count = pks.len(), "свёрнутые папки прочитаны");
    Ok(pks)
}

/// Сохранить свёрнутые папки целиком (окно зовёт с небольшой задержкой после сворачивания или
/// разворачивания). Неизвестные и удалённые папки пропускаются.
#[tauri::command]
pub(crate) fn folders_collapsed_set(folder_pks: Vec<i64>, state: State<'_, AppState>) -> CmdResult<()> {
    let saved = state.store.set_folder_list_state(state.account, COLLAPSED_FOLDERS_KEY, &folder_pks).map_err(|e| format!("Не удалось сохранить вид дерева папок: {e}"))?;
    debug!(requested = folder_pks.len(), saved, "свёрнутые папки сохранены");
    Ok(())
}
