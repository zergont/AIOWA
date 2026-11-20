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
