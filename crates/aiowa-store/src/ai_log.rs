//! Журнал обращений к языковым моделям (`docs/ai-layer-design.md` § 3.3): время, функция,
//! подключение, адресат (хост и место: этот компьютер, свой сервер, облако), модель, токены,
//! длительность, исход и SHA-256 тела запроса. Ни текста писем, ни ответа модели здесь нет.
//! Хранится полгода: старые записи убираются при добавлении новых.

use crate::{now_ms, Result, Store};
use aiowa_core::AccountId;
use rusqlite::params;

/// Сколько хранить записи журнала, мс (≈ 183 дня).
const KEEP_MS: i64 = 183 * 24 * 3600 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiCall {
    pub at: i64,
    pub feature: String,
    pub profile_id: String,
    pub host: String,
    /// `local` | `own_server` | `cloud`.
    pub place: String,
    pub model: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub duration_ms: i64,
    pub outcome: String,
    pub request_sha256: String,
}

/// Итог за период по одному подключению и модели.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiUsage {
    pub profile_id: String,
    pub model: String,
    pub place: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

impl Store {
    pub fn ai_call_add(&self, account: AccountId, c: &AiCall) -> Result<()> {
        let cutoff = now_ms() - KEEP_MS;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO ai_calls(account_id, at, feature, profile_id, host, place, model, input_tokens, output_tokens, duration_ms, outcome, request_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![account.0, c.at, c.feature, c.profile_id, c.host, c.place, c.model, c.input_tokens, c.output_tokens, c.duration_ms, c.outcome, c.request_sha256],
            )?;
            conn.execute("DELETE FROM ai_calls WHERE account_id = ?1 AND at < ?2", params![account.0, cutoff])?;
            Ok(())
        })
    }

    /// Последние обращения, свежие первыми.
    pub fn ai_calls_recent(&self, account: AccountId, limit: usize) -> Result<Vec<AiCall>> {
        self.with_conn(|conn| {
            let mut st = conn.prepare_cached(
                "SELECT at, feature, profile_id, host, place, model, input_tokens, output_tokens, duration_ms, outcome, request_sha256
                 FROM ai_calls WHERE account_id = ?1 ORDER BY at DESC, call_pk DESC LIMIT ?2",
            )?;
            let rows = st.query_map(params![account.0, limit as i64], |r| {
                Ok(AiCall {
                    at: r.get(0)?,
                    feature: r.get(1)?,
                    profile_id: r.get(2)?,
                    host: r.get(3)?,
                    place: r.get(4)?,
                    model: r.get(5)?,
                    input_tokens: r.get(6)?,
                    output_tokens: r.get(7)?,
                    duration_ms: r.get(8)?,
                    outcome: r.get(9)?,
                    request_sha256: r.get(10)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }

    /// Итоги с момента `since` (мс) по подключению и модели: обращения и токены.
    pub fn ai_usage_since(&self, account: AccountId, since: i64) -> Result<Vec<AiUsage>> {
        self.with_conn(|conn| {
            let mut st = conn.prepare_cached(
                "SELECT profile_id, model, place, COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
                 FROM ai_calls WHERE account_id = ?1 AND at >= ?2 GROUP BY profile_id, model, place ORDER BY COUNT(*) DESC",
            )?;
            let rows = st.query_map(params![account.0, since], |r| {
                Ok(AiUsage { profile_id: r.get(0)?, model: r.get(1)?, place: r.get(2)?, calls: r.get(3)?, input_tokens: r.get(4)?, output_tokens: r.get(5)? })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (Store, AccountId) {
        let path = std::env::temp_dir().join(format!("aiowa-ai-log-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let s = Store::open(&path).unwrap();
        let a = s.account("h", "u").unwrap();
        (s, a)
    }

    fn call(at: i64, model: &str, outcome: &str, tokens: Option<(i64, i64)>) -> AiCall {
        AiCall {
            at,
            feature: "проверка".into(),
            profile_id: "p1".into(),
            host: "api.anthropic.com".into(),
            place: "cloud".into(),
            model: model.into(),
            input_tokens: tokens.map(|t| t.0),
            output_tokens: tokens.map(|t| t.1),
            duration_ms: 1200,
            outcome: outcome.into(),
            request_sha256: "ab".repeat(32),
        }
    }

    #[test]
    fn journal_keeps_recent_and_sums_usage() {
        let (s, a) = store("sum");
        let now = now_ms();
        s.ai_call_add(a, &call(now - KEEP_MS - 1000, "old", "ok", Some((1, 1)))).unwrap();
        s.ai_call_add(a, &call(now - 2000, "m1", "ok", Some((100, 20)))).unwrap();
        s.ai_call_add(a, &call(now - 1000, "m1", "auth", None)).unwrap();
        s.ai_call_add(a, &call(now, "m2", "ok", Some((50, 5)))).unwrap();
        let recent = s.ai_calls_recent(a, 10).unwrap();
        assert_eq!(recent.iter().map(|c| c.model.as_str()).collect::<Vec<_>>(), ["m2", "m1", "m1"], "старше полугода — убрано");
        let usage = s.ai_usage_since(a, now - 10_000).unwrap();
        let m1 = usage.iter().find(|u| u.model == "m1").unwrap();
        assert_eq!((m1.calls, m1.input_tokens, m1.output_tokens), (2, 100, 20));
    }
}
