//! «Хранилище» (docs/obkatka.md п. 15): сколько места занимает ящик на сервере — квота из
//! `sessiondata.ashx` и размер каждой папки (`FindFolder` всего дерева с `PR_MESSAGE_SIZE_EXTENDED`,
//! docs/protocol-owa-json.md § 5t), в том числе папок, которые приложение не синхронизирует, — и
//! сколько на компьютере: писем в базе и скачанных вложений по папкам, размер базы и кэша.
//! В журнал — только числа.

use crate::sync::SyncState;
use crate::{err, AppState, CmdResult};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::State;
use tracing::info;

/// Таймаут запросов к серверу (квота и дерево с размерами).
const SERVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Папка ящика для «Хранилища»: с сервера (размер, число элементов) и по базе (`folder_pk`, если
/// папка известна приложению, письма и вложения на компьютере).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StorageFolderDto {
    folder_pk: Option<i64>,
    server_id: String,
    parent_id: Option<String>,
    name: String,
    folder_class: Option<String>,
    distinguished: Option<String>,
    total_count: Option<i64>,
    size_bytes: Option<i64>,
    local_items: i64,
    local_cached_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StorageDto {
    /// Квота ящика (как отдаёт сервер; 0 у предупреждения и запрета — ограничений нет).
    quota_used: Option<i64>,
    quota_warning: Option<i64>,
    quota_send: Option<i64>,
    folders: Vec<StorageFolderDto>,
    db_size_bytes: u64,
    cache_size_bytes: u64,
}

/// Квота и размеры папок с сервера плюс локальные счётчики. Сервер не ответил — ошибка.
#[tauri::command]
pub(crate) async fn storage_info(state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>) -> CmdResult<StorageDto> {
    let started = Instant::now();
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let quota = tokio::time::timeout(SERVER_TIMEOUT, transport.mailbox_quota()).await.map_err(|_| "Сервер не ответил за 30 с".to_string())?.map_err(|e| crate::ondemand::describe_transport(&e))?;
    let sizes = tokio::time::timeout(SERVER_TIMEOUT, transport.folder_sizes()).await.map_err(|_| "Сервер не ответил за 30 с".to_string())?.map_err(|e| crate::ondemand::describe_transport(&e))?;
    let (store, account) = (state.store.clone(), state.account);
    let dto = tauri::async_runtime::spawn_blocking(move || -> CmdResult<StorageDto> {
        let known: HashMap<String, (i64, Option<String>)> = store.list_folders(account).map_err(err)?.into_iter().map(|(pk, f)| (f.id.id, (pk, f.distinguished))).collect();
        let local: HashMap<i64, aiowa_store::FolderLocalStats> = store.folder_local_stats(account).map_err(err)?.into_iter().map(|s| (s.folder_pk, s)).collect();
        let folders = sizes
            .into_iter()
            .map(|f| {
                let (folder_pk, distinguished) = known.get(&f.id).cloned().map(|(pk, d)| (Some(pk), d)).unwrap_or((None, None));
                let l = folder_pk.and_then(|pk| local.get(&pk).copied());
                StorageFolderDto {
                    folder_pk,
                    server_id: f.id,
                    parent_id: f.parent_id,
                    name: f.name.unwrap_or_default(),
                    folder_class: f.folder_class,
                    distinguished,
                    total_count: f.total_count,
                    size_bytes: f.size_bytes,
                    local_items: l.map(|l| l.items).unwrap_or(0),
                    local_cached_bytes: l.map(|l| l.cached_bytes).unwrap_or(0),
                }
            })
            .collect();
        Ok(StorageDto { quota_used: quota.used, quota_warning: quota.warning, quota_send: quota.send, folders, db_size_bytes: store.disk_size_bytes(), cache_size_bytes: store.attachment_cache_bytes() })
    })
    .await
    .map_err(err)??;
    info!(folders = dto.folders.len(), ms = started.elapsed().as_millis() as u64, "хранилище: размеры папок и квота");
    Ok(dto)
}
