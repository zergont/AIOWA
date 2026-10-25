//! Состояние вида окна в базе учётной записи: списки папок под ключами `sync_state` (например,
//! свёрнутые ветки дерева папок). Окно работает с локальными ключами `folder_pk`, а в базе лежат
//! серверные идентификаторы папок (JSON-массив строк): так состояние переживает пересоздание базы,
//! где у тех же папок будут другие ключи. Папки, которых больше нет (удалены на сервере или
//! локально), из списка выбрасываются, но только когда дерево в базе есть: пока в ней нет ни одной
//! живой папки (новая база, иерархия ещё не выгружена), список не трогается.

use crate::{Result, Store};
use aiowa_core::AccountId;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

/// Не удалённые папки учётной записи: серверный идентификатор → локальный ключ.
fn live_folders(c: &Connection, account: AccountId) -> rusqlite::Result<HashMap<String, i64>> {
    let mut st = c.prepare_cached("SELECT server_id, folder_pk FROM folders WHERE account_id = ?1 AND deleted = 0")?;
    let rows = st.query_map(params![account.0], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    rows.collect()
}

/// Значение ключа как список идентификаторов; нет ключа или значение негодное — пусто.
fn parse_ids(raw: Option<&str>) -> Vec<String> {
    raw.and_then(|v| serde_json::from_str::<Vec<String>>(v).ok()).unwrap_or_default()
}

impl Store {
    /// Папки из списка под ключом `key` в виде локальных ключей не удалённых папок, в порядке
    /// списка. Нет ключа или значение негодное — пусто. Если часть папок исчезла (помечена
    /// удалённой или её нет в непустом дереве), список в базе тут же переписывается без них.
    /// Живых папок в базе нет вовсе (дерево ещё не выгружено) — пусто, а список остаётся как был:
    /// иначе первый же показ окна до синхронизации иерархии стёр бы его.
    pub fn folder_list_state(&self, account: AccountId, key: &str) -> Result<Vec<i64>> {
        let raw = self.get_state(account, key)?;
        let ids = parse_ids(raw.as_deref());
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let live = self.with_conn(|c| Ok(live_folders(c, account)?))?;
        if live.is_empty() {
            return Ok(Vec::new());
        }
        let kept: Vec<&String> = ids.iter().filter(|id| live.contains_key(id.as_str())).collect();
        if kept.len() != ids.len() {
            let value = serde_json::to_string(&kept).map_err(|e| crate::StoreError::Other(e.to_string()))?;
            self.set_state(account, key, Some(&value))?;
        }
        Ok(kept.into_iter().filter_map(|id| live.get(id.as_str()).copied()).collect())
    }

    /// Записывает под ключом `key` список папок `folder_pks` (локальные ключи) их серверными
    /// идентификаторами, в том же порядке. Неизвестные и удалённые папки и повторы пропускаются.
    /// Возвращает, сколько папок записано.
    pub fn set_folder_list_state(&self, account: AccountId, key: &str, folder_pks: &[i64]) -> Result<usize> {
        let live = self.with_conn(|c| Ok(live_folders(c, account)?))?;
        let by_pk: HashMap<i64, &str> = live.iter().map(|(id, pk)| (*pk, id.as_str())).collect();
        let mut seen = HashSet::new();
        let ids: Vec<&str> = folder_pks.iter().filter(|pk| seen.insert(**pk)).filter_map(|pk| by_pk.get(pk).copied()).collect();
        let value = serde_json::to_string(&ids).map_err(|e| crate::StoreError::Other(e.to_string()))?;
        self.set_state(account, key, Some(&value))?;
        Ok(ids.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{Folder, FolderId};

    fn tmp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("aiowa-store-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(dir.join("mail.db")).unwrap()
    }

    fn folder(name: &str, parent: Option<&str>) -> Folder {
        Folder {
            id: FolderId { id: format!("F-{name}"), change_key: Some("ck".into()) },
            parent_id: parent.map(|p| FolderId { id: format!("F-{p}"), change_key: None }),
            display_name: name.to_string(),
            distinguished: None,
            folder_class: Some("IPF.Note".into()),
            total_count: 0,
            unread_count: 0,
            child_folder_count: 0,
        }
    }

    const KEY: &str = "desktop.collapsed_folders";

    #[test]
    fn folder_list_is_stored_by_server_id_and_drops_deleted_folders() {
        let s = tmp_store("viewstate");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Проекты", None), folder("Альфа", Some("Проекты")), folder("Бета", Some("Проекты")), folder("Архив", None)]).unwrap();
        let pk = |name: &str| s.resolve_folder(acc, name).unwrap().unwrap().folder_pk;
        let (projects, alpha, beta) = (pk("Проекты"), pk("Альфа"), pk("Бета"));

        // Ключа нет — пусто; негодное значение — тоже пусто, без ошибки.
        assert!(s.folder_list_state(acc, KEY).unwrap().is_empty());
        s.set_state(acc, KEY, Some("не json")).unwrap();
        assert!(s.folder_list_state(acc, KEY).unwrap().is_empty());

        // Запись: в базе серверные идентификаторы в порядке окна, без повторов и неизвестных ключей.
        assert_eq!(s.set_folder_list_state(acc, KEY, &[alpha, projects, alpha, 9999]).unwrap(), 2);
        assert_eq!(s.get_state(acc, KEY).unwrap().as_deref(), Some(r#"["F-Альфа","F-Проекты"]"#));
        assert_eq!(s.folder_list_state(acc, KEY).unwrap(), vec![alpha, projects]);

        // Ключ другой учётной записи в той же базе не виден.
        let other = s.account("h", "другой").unwrap();
        assert!(s.folder_list_state(other, KEY).unwrap().is_empty());

        // Папку удалили (синхронизация иерархии пометила удалённой): при чтении она выпадает из
        // выдачи и из значения в базе.
        s.set_folder_list_state(acc, KEY, &[alpha, beta, projects]).unwrap();
        s.mark_folder_deleted(acc, &FolderId { id: "F-Бета".into(), change_key: None }).unwrap();
        assert_eq!(s.folder_list_state(acc, KEY).unwrap(), vec![alpha, projects]);
        assert_eq!(s.get_state(acc, KEY).unwrap().as_deref(), Some(r#"["F-Альфа","F-Проекты"]"#));

        // Пустой список — пустой массив, а не отсутствие ключа.
        assert_eq!(s.set_folder_list_state(acc, KEY, &[]).unwrap(), 0);
        assert_eq!(s.get_state(acc, KEY).unwrap().as_deref(), Some("[]"));
        assert!(s.folder_list_state(acc, KEY).unwrap().is_empty());
    }

    #[test]
    fn folder_list_is_kept_until_folders_exist() {
        let s = tmp_store("viewstate-empty");
        let acc = s.account("h", "u").unwrap();
        let saved = r#"["F-Альфа","F-Бета"]"#;

        // Новая база, дерева ещё нет: выдача пуста, сохранённый список не тронут.
        s.set_state(acc, KEY, Some(saved)).unwrap();
        assert!(s.folder_list_state(acc, KEY).unwrap().is_empty());
        assert_eq!(s.get_state(acc, KEY).unwrap().as_deref(), Some(saved));

        // Папки в базе есть, но все помечены удалёнными — живых нет, тоже не трогаем.
        s.upsert_folders(acc, &[folder("Альфа", None)]).unwrap();
        s.mark_folder_deleted(acc, &FolderId { id: "F-Альфа".into(), change_key: None }).unwrap();
        assert!(s.folder_list_state(acc, KEY).unwrap().is_empty());
        assert_eq!(s.get_state(acc, KEY).unwrap().as_deref(), Some(saved));

        // Дерево пришло: «Бета» на месте, «Альфа» удалена, «Гамма» в списке, но её в дереве нет —
        // удалённая и отсутствующая выпадают, список переписан.
        s.upsert_folders(acc, &[folder("Бета", None)]).unwrap();
        s.set_state(acc, KEY, Some(r#"["F-Гамма","F-Альфа","F-Бета"]"#)).unwrap();
        let beta = s.resolve_folder(acc, "Бета").unwrap().unwrap().folder_pk;
        assert_eq!(s.folder_list_state(acc, KEY).unwrap(), vec![beta]);
        assert_eq!(s.get_state(acc, KEY).unwrap().as_deref(), Some(r#"["F-Бета"]"#));

        // Ключ другой учётной записи в той же базе: у неё папок нет — её список цел.
        let other = s.account("h", "другой").unwrap();
        s.set_state(other, KEY, Some(saved)).unwrap();
        assert!(s.folder_list_state(other, KEY).unwrap().is_empty());
        assert_eq!(s.get_state(other, KEY).unwrap().as_deref(), Some(saved));
    }

    #[test]
    fn folder_list_survives_recreated_folder_rows() {
        // База пересоздана (те же папки получили другие локальные ключи): значение с серверными
        // идентификаторами по-прежнему находит их.
        let s = tmp_store("viewstate-recreate");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Проекты", None), folder("Альфа", Some("Проекты"))]).unwrap();
        let alpha = s.resolve_folder(acc, "Альфа").unwrap().unwrap().folder_pk;
        s.set_folder_list_state(acc, KEY, &[alpha]).unwrap();
        let value = s.get_state(acc, KEY).unwrap();

        let s2 = tmp_store("viewstate-recreate-2");
        let acc2 = s2.account("h", "u").unwrap();
        s2.upsert_folders(acc2, &[folder("Пусто", None), folder("Ещё", None), folder("Проекты", None), folder("Альфа", Some("Проекты"))]).unwrap();
        s2.set_state(acc2, KEY, value.as_deref()).unwrap();
        let alpha2 = s2.resolve_folder(acc2, "Альфа").unwrap().unwrap().folder_pk;
        assert_ne!(alpha, alpha2, "в новой базе у папки другой ключ");
        assert_eq!(s2.folder_list_state(acc2, KEY).unwrap(), vec![alpha2]);
    }
}
