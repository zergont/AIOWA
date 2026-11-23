//! Журнал обращений внешних агентов через MCP-сервер (`docs/ai-layer-design.md` § 11.4): время,
//! инструмент, клиент, сколько элементов и символов отдано, длительность и исход. Ни запросов
//! агента, ни текста писем здесь нет. Хранится полгода: старые записи убираются при добавлении.

use crate::{now_ms, Result, Store};
use aiowa_core::AccountId;
use rusqlite::params;

/// Сколько хранить записи журнала, мс (≈ 183 дня).
const KEEP_MS: i64 = 183 * 24 * 3600 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCall {
    pub at: i64,
    /// Имя инструмента (`search_mail`, `get_message`…).
    pub tool: String,
    /// Как назвался клиент при подключении (`clientInfo.name`), обрезанное.
    pub client: String,
    /// Сколько элементов отдано: писем, событий, контактов, папок, вложений.
    pub items: i64,
    /// Сколько символов ушло агенту.
    pub chars: i64,
    pub duration_ms: i64,
    /// `ok` | `disabled` | `denied` | `invalid` | `not_found` | `error`.
    pub outcome: String,
}

/// Вложение, выгруженное агенту файлом в папку выгрузки (`mcp_exports`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpExport {
    /// Ключ записи; при добавлении не нужен.
    pub export_pk: i64,
    pub at: i64,
    pub attachment_pk: i64,
    /// Имя файла в папке выгрузки.
    pub name: String,
    /// Полный путь к выгруженному файлу.
    pub path: String,
    pub size: i64,
    pub client: String,
}

/// Итог за период: обращения, элементы и символы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct McpUsage {
    pub calls: i64,
    pub items: i64,
    pub chars: i64,
}

impl Store {
    pub fn mcp_call_add(&self, account: AccountId, c: &McpCall) -> Result<()> {
        let cutoff = now_ms() - KEEP_MS;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO mcp_calls(account_id, at, tool, client, items, chars, duration_ms, outcome) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![account.0, c.at, c.tool, c.client, c.items, c.chars, c.duration_ms, c.outcome],
            )?;
            conn.execute("DELETE FROM mcp_calls WHERE account_id = ?1 AND at < ?2", params![account.0, cutoff])?;
            Ok(())
        })
    }

    /// Записать выгрузку; повторная выгрузка по тому же пути обновляет прежнюю запись.
    pub fn mcp_export_add(&self, account: AccountId, e: &McpExport) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO mcp_exports(account_id, at, attachment_pk, name, path, size, client) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(account_id, path) DO UPDATE SET at = excluded.at, attachment_pk = excluded.attachment_pk, name = excluded.name, size = excluded.size, client = excluded.client",
                params![account.0, e.at, e.attachment_pk, e.name, e.path, e.size, e.client],
            )?;
            Ok(())
        })
    }

    fn mcp_exports_where(&self, account: AccountId, filter: &str, value: i64) -> Result<Vec<McpExport>> {
        self.with_conn(|conn| {
            let mut st = conn.prepare(&format!("SELECT export_pk, at, attachment_pk, name, path, size, client FROM mcp_exports WHERE account_id = ?1 AND {filter}"))?;
            let rows = st.query_map(params![account.0, value], |r| Ok(McpExport { export_pk: r.get(0)?, at: r.get(1)?, attachment_pk: r.get(2)?, name: r.get(3)?, path: r.get(4)?, size: r.get(5)?, client: r.get(6)? }))?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }

    /// Последние выгрузки, свежие первыми.
    pub fn mcp_exports_recent(&self, account: AccountId, limit: usize) -> Result<Vec<McpExport>> {
        self.mcp_exports_where(account, "1 = 1 ORDER BY at DESC, export_pk DESC LIMIT ?2", limit as i64)
    }

    /// Выгрузки старше `before` (мс): кандидаты на удаление файла по сроку хранения.
    pub fn mcp_exports_before(&self, account: AccountId, before: i64) -> Result<Vec<McpExport>> {
        self.mcp_exports_where(account, "at < ?2 ORDER BY at", before)
    }

    /// Запись о выгрузке по пути файла: показать в Проводнике можно только то, что выгружено.
    pub fn mcp_export_by_path(&self, account: AccountId, path: &str) -> Result<Option<McpExport>> {
        self.with_conn(|conn| {
            let mut st = conn.prepare_cached("SELECT export_pk, at, attachment_pk, name, path, size, client FROM mcp_exports WHERE account_id = ?1 AND path = ?2")?;
            let mut rows = st.query_map(params![account.0, path], |r| Ok(McpExport { export_pk: r.get(0)?, at: r.get(1)?, attachment_pk: r.get(2)?, name: r.get(3)?, path: r.get(4)?, size: r.get(5)?, client: r.get(6)? }))?;
            rows.next().transpose().map_err(Into::into)
        })
    }

    /// Забыть выгрузку (файл удалён по сроку или его уже нет).
    pub fn mcp_export_forget(&self, export_pk: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM mcp_exports WHERE export_pk = ?1", params![export_pk])?;
            Ok(())
        })
    }

    /// Последние обращения агентов, свежие первыми.
    pub fn mcp_calls_recent(&self, account: AccountId, limit: usize) -> Result<Vec<McpCall>> {
        self.with_conn(|conn| {
            let mut st = conn.prepare_cached("SELECT at, tool, client, items, chars, duration_ms, outcome FROM mcp_calls WHERE account_id = ?1 ORDER BY at DESC, call_pk DESC LIMIT ?2")?;
            let rows = st.query_map(params![account.0, limit as i64], |r| Ok(McpCall { at: r.get(0)?, tool: r.get(1)?, client: r.get(2)?, items: r.get(3)?, chars: r.get(4)?, duration_ms: r.get(5)?, outcome: r.get(6)? }))?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }

    /// Итог удачных обращений с момента `since` (мс).
    pub fn mcp_usage_since(&self, account: AccountId, since: i64) -> Result<McpUsage> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(items), 0), COALESCE(SUM(chars), 0) FROM mcp_calls WHERE account_id = ?1 AND at >= ?2 AND outcome = 'ok'",
                params![account.0, since],
                |r| Ok(McpUsage { calls: r.get(0)?, items: r.get(1)?, chars: r.get(2)? }),
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_are_listed_updated_and_forgotten() {
        let path = std::env::temp_dir().join(format!("aiowa-mcp-exports-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let s = Store::open(&path).unwrap();
        let acc = s.account("h", "u").unwrap();
        let export = |at: i64, pk: i64, path: &str, size: i64| McpExport { export_pk: 0, at, attachment_pk: pk, name: path.rsplit('/').next().unwrap().into(), path: path.into(), size, client: "Claude".into() };
        s.mcp_export_add(acc, &export(1_000, 7, "D:/out/7-смета.pdf", 10)).unwrap();
        s.mcp_export_add(acc, &export(2_000, 8, "D:/out/8-скан.pdf", 20)).unwrap();
        // Повторная выгрузка по тому же пути — та же запись с новым временем и размером.
        s.mcp_export_add(acc, &export(3_000, 7, "D:/out/7-смета.pdf", 11)).unwrap();
        let recent = s.mcp_exports_recent(acc, 10).unwrap();
        assert_eq!(recent.iter().map(|e| (e.attachment_pk, e.at, e.size)).collect::<Vec<_>>(), [(7, 3_000, 11), (8, 2_000, 20)]);
        assert_eq!(s.mcp_exports_recent(acc, 1).unwrap().len(), 1);
        let old = s.mcp_exports_before(acc, 2_500).unwrap();
        assert_eq!(old.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["D:/out/8-скан.pdf"]);
        assert_eq!(s.mcp_export_by_path(acc, "D:/out/8-скан.pdf").unwrap().map(|e| e.attachment_pk), Some(8));
        assert!(s.mcp_export_by_path(acc, "D:/out/чужой.pdf").unwrap().is_none());
        s.mcp_export_forget(old[0].export_pk).unwrap();
        assert_eq!(s.mcp_exports_recent(acc, 10).unwrap().len(), 1);
    }

    fn call(at: i64, tool: &str, outcome: &str, items: i64, chars: i64) -> McpCall {
        McpCall { at, tool: tool.into(), client: "тест".into(), items, chars, duration_ms: 12, outcome: outcome.into() }
    }

    #[test]
    fn journal_keeps_recent_and_sums_ok_calls() {
        let path = std::env::temp_dir().join(format!("aiowa-mcp-log-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let s = Store::open(&path).unwrap();
        let a = s.account("h", "u").unwrap();
        let now = now_ms();
        s.mcp_call_add(a, &call(now - KEEP_MS - 1000, "old", "ok", 1, 10)).unwrap();
        s.mcp_call_add(a, &call(now - 2000, "search_mail", "ok", 12, 3400)).unwrap();
        s.mcp_call_add(a, &call(now - 1000, "get_message", "disabled", 0, 0)).unwrap();
        s.mcp_call_add(a, &call(now, "get_message", "ok", 1, 900)).unwrap();
        let recent = s.mcp_calls_recent(a, 10).unwrap();
        assert_eq!(recent.iter().map(|c| c.tool.as_str()).collect::<Vec<_>>(), ["get_message", "get_message", "search_mail"], "старше полугода — убрано");
        assert_eq!(s.mcp_usage_since(a, now - 10_000).unwrap(), McpUsage { calls: 2, items: 13, chars: 4300 }, "отказы в итог не входят");
    }
}
