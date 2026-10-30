//! Сведения о ящике после входа: адрес, `MailboxGuid` и отображаемое имя (§ 2 протокола).
//!
//! Основной путь — `POST /owa/sessiondata.ashx` (первый запрос страницы OWA после входа): раздел
//! `owaUserConfig.SessionSettings`. Запасной — операция `GetOwaUserConfiguration` в `service.svc`
//! (тот же `SessionSettings` в ответе). По этим сведениям окно находит базу ящика при любой форме
//! логина (`savvidi`, `savvidi@ngenergo`, `NGENERGO\savvidi`) и не синхронизирует ящик в базу
//! другого ящика.
//!
//! Политика журналов: адрес и имя в журнал не пишутся — только наличие и длины.

use crate::{net, truncate, OwaTransport};
use aiowa_transport::{Result, TransportError};
use reqwest::header::LOCATION;
use reqwest::StatusCode;
use serde_json::Value;
use std::sync::atomic::Ordering;
use tracing::{debug, warn};

/// Кто владелец ящика, в который выполнен вход.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxInfo {
    /// Основной адрес ящика (SMTP), в нижнем регистре.
    pub email: Option<String>,
    /// `MailboxGuid` в каноническом виде: строчные цифры с дефисами, без фигурных скобок.
    pub mailbox_guid: Option<String>,
    pub display_name: Option<String>,
}

impl MailboxInfo {
    /// Ничего не известно (ответ без нужных полей).
    pub fn is_empty(&self) -> bool {
        self.email.is_none() && self.mailbox_guid.is_none() && self.display_name.is_none()
    }
}

/// Ключи `SessionSettings` с основным адресом ящика — в порядке предпочтения (сняты с сервера
/// 25.10.2026, § 2): `UserEmailAddress` — основной SMTP ящика, `LogonEmailAddress` — адрес входа
/// (у своего ящика тот же).
const EMAIL_KEYS: &[&str] = &["UserEmailAddress", "LogonEmailAddress"];
/// Ключи с отображаемым именем.
const NAME_KEYS: &[&str] = &["UserDisplayName"];
/// Ключи с GUID ящика: 36 символов, с дефисами, без скобок.
const GUID_KEYS: &[&str] = &["MailboxGuid"];

/// GUID в каноническом виде: `{AB12…}` и `AB12…` → `ab12…` с дефисами 8-4-4-4-12. Не похоже на
/// GUID — `None`.
pub fn normalize_guid(raw: &str) -> Option<String> {
    let hex: String = raw.trim().trim_start_matches('{').trim_end_matches('}').chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let h = hex.to_ascii_lowercase();
    if h.chars().all(|c| c == '0') {
        return None;
    }
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// Адрес почты: непустой, с `@`, без пробелов; в нижнем регистре. Иначе `None`.
fn normalize_email(raw: &str) -> Option<String> {
    let s = raw.trim();
    let (local, domain) = s.split_once('@')?;
    if local.is_empty() || domain.is_empty() || s.chars().any(char::is_whitespace) {
        return None;
    }
    Some(s.to_lowercase())
}

fn first_text<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| obj.get(*k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()))
}

/// Объект `SessionSettings` в ответе `sessiondata.ashx` (`owaUserConfig.SessionSettings`) или
/// `GetOwaUserConfiguration` (ответ — сам объект `owaUserConfig`, `SessionSettings` в корне).
fn session_settings(v: &Value) -> Option<&Value> {
    ["/owaUserConfig/SessionSettings", "/SessionSettings"].iter().find_map(|p| v.pointer(p)).filter(|s| s.is_object())
}

/// Сведения о ящике из ответа `sessiondata.ashx` или `GetOwaUserConfiguration`. Нет
/// `SessionSettings` или нужных полей — пустой `MailboxInfo`.
pub fn parse_mailbox_info(v: &Value) -> MailboxInfo {
    let Some(ss) = session_settings(v) else { return MailboxInfo::default() };
    MailboxInfo {
        email: first_text(ss, EMAIL_KEYS).and_then(normalize_email),
        mailbox_guid: first_text(ss, GUID_KEYS).and_then(normalize_guid),
        display_name: first_text(ss, NAME_KEYS).map(String::from),
    }
}

impl OwaTransport {
    /// Сведения о ящике: `sessiondata.ashx`, а если там нет адреса или GUID —
    /// `GetOwaUserConfiguration`. Ошибка основного пути при удачном запасном — не ошибка.
    pub async fn mailbox_info(&self) -> Result<MailboxInfo> {
        let primary = match self.session_data_value().await {
            Ok(v) => {
                let info = parse_mailbox_info(&v);
                debug!(email = info.email.is_some(), guid = info.mailbox_guid.is_some(), name = info.display_name.is_some(), "sessiondata.ashx: сведения о ящике");
                if info.email.is_some() && info.mailbox_guid.is_some() {
                    return Ok(info);
                }
                Ok(info)
            }
            // Сессии нет: запасной путь упрётся в то же самое.
            Err(e) if e.needs_reauth() || matches!(e, TransportError::AuthFailed(_) | TransportError::SessionLimit) => return Err(e),
            Err(e) => {
                warn!(error = %e, "sessiondata.ashx не ответил, пробую GetOwaUserConfiguration");
                Err(e)
            }
        };
        let fallback = self.owa_user_configuration_value().await.map(|v| parse_mailbox_info(&v));
        match (primary, fallback) {
            // Поля основного ответа в приоритете, недостающие — из запасного.
            (Ok(base), Ok(extra)) => {
                debug!(email = extra.email.is_some(), guid = extra.mailbox_guid.is_some(), "GetOwaUserConfiguration: сведения о ящике");
                Ok(MailboxInfo { email: base.email.or(extra.email), mailbox_guid: base.mailbox_guid.or(extra.mailbox_guid), display_name: base.display_name.or(extra.display_name) })
            }
            (Err(_), Ok(extra)) => Ok(extra),
            (Ok(base), Err(e)) => {
                debug!(error = %e, "GetOwaUserConfiguration не ответил, остаются сведения sessiondata.ashx");
                Ok(base)
            }
            (Err(first), Err(e)) => {
                warn!(error = %e, "GetOwaUserConfiguration тоже не ответил");
                Err(first)
            }
        }
    }

    /// Ответ `POST /owa/sessiondata.ashx?appcacheclient=0` как есть (§ 2): профиль, дерево папок и
    /// первая страница «Входящих». Истёкшая сессия — один повторный вход.
    pub async fn session_data_value(&self) -> Result<Value> {
        let _permit = self.inner.permits.acquire().await.map_err(|_| TransportError::Cancelled)?;
        for attempt in 0..2 {
            let generation = self.inner.generation.load(Ordering::SeqCst);
            match self.session_data_once().await {
                Err(e) if e.needs_reauth() && attempt == 0 => self.relogin_once(generation).await?,
                other => return other,
            }
        }
        Err(TransportError::SessionExpired)
    }

    async fn session_data_once(&self) -> Result<Value> {
        const OP: &str = "sessiondata.ashx";
        let inner = &self.inner;
        if inner.canary.read().expect("canary lock").is_none() {
            return Err(TransportError::SessionExpired);
        }
        let url = inner.base.join("owa/sessiondata.ashx?appcacheclient=0").expect("url");
        let corr = aiowa_transport::new_guid();
        let started = std::time::Instant::now();
        let req = inner
            .client
            .post(url)
            .header("X-OWA-CorrelationId", &corr)
            .header("client-request-id", &corr)
            .header("X-Requested-With", "XMLHttpRequest")
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .body(Vec::<u8>::new());
        let r = crate::send_retrying(req, "sessiondata", true).await?;
        let status = r.status();
        self.remember_build(r.headers());
        if status == StatusCode::from_u16(440).unwrap_or(StatusCode::UNAUTHORIZED) || status == StatusCode::UNAUTHORIZED {
            return Err(TransportError::SessionExpired);
        }
        if status.is_redirection() {
            let loc = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            return Err(crate::classify_redirect(&loc));
        }
        let text = r.text().await.map_err(net)?;
        debug!(%status, ms = started.elapsed().as_millis(), bytes = text.len(), "sessiondata.ashx");
        if !status.is_success() {
            return Err(TransportError::Protocol { op: OP.into(), status: status.as_u16(), raw: crate::non_json_excerpt(&text) });
        }
        serde_json::from_str::<Value>(&text).map_err(|e| TransportError::Protocol { op: OP.into(), status: status.as_u16(), raw: format!("не JSON ({e}), {} байт: {}", text.len(), crate::non_json_excerpt(&text)) })
    }

    /// Ответ операции `GetOwaUserConfiguration` как есть: запасной путь к `SessionSettings`.
    /// Параметров у операции нет: тело — пустой объект `{}` без конверта (сервер принимает, § 2),
    /// ответ — объект `owaUserConfig` целиком (те же 29 разделов, что в `sessiondata.ashx`).
    pub async fn owa_user_configuration_value(&self) -> Result<Value> {
        let v = self.call_raw("GetOwaUserConfiguration", serde_json::json!({})).await?;
        if let Some(fault) = v.pointer("/Body/FaultMessage").and_then(Value::as_str) {
            return Err(TransportError::Protocol { op: "GetOwaUserConfiguration".into(), status: 200, raw: truncate(fault, 300) });
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn guid_is_normalized() {
        assert_eq!(normalize_guid("{0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9}").as_deref(), Some("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9"));
        assert_eq!(normalize_guid(" 0a1b2c3d4e5f60718293a4b5c6d7e8f9 ").as_deref(), Some("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9"));
        assert_eq!(normalize_guid("00000000-0000-0000-0000-000000000000"), None, "пустой GUID — не идентификатор");
        assert_eq!(normalize_guid("не guid"), None);
        assert_eq!(normalize_guid("0a1b2c3d-4e5f-6071-8293"), None);
    }

    #[test]
    fn session_settings_are_parsed_from_sessiondata() {
        // Форма по § 2 (ключи сняты с сервера, значения выдуманы).
        let v = json!({
            "owaUserConfig": {
                "SessionSettings": {
                    "UserDisplayName": "Иванов Иван",
                    "UserEmailAddress": "Ivanov@Example.RU",
                    "LogonEmailAddress": "ivanov@example.ru",
                    "UserPrincipalName": "ivanov@example.local",
                    "MailboxGuid": "{0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9}",
                    "UserProxyAddresses": ["SMTP:ivanov@example.ru"],
                    "QuotaUsed": 123
                },
                "UserOptions": {}
            },
            "findFolders": {},
            "findMailFolderItem": {}
        });
        let info = parse_mailbox_info(&v);
        assert_eq!(info.email.as_deref(), Some("ivanov@example.ru"));
        assert_eq!(info.mailbox_guid.as_deref(), Some("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9"));
        assert_eq!(info.display_name.as_deref(), Some("Иванов Иван"));
        assert!(!info.is_empty());
    }

    #[test]
    fn fallback_shapes_and_missing_fields() {
        // GetOwaUserConfiguration: SessionSettings в корне ответа; без UserEmailAddress — адрес входа.
        let v = json!({"UserOptions": {}, "SessionSettings": {"LogonEmailAddress": "a@x.ru", "MailboxGuid": "0a1b2c3d4e5f60718293a4b5c6d7e8f9"}});
        let info = parse_mailbox_info(&v);
        assert_eq!(info.email.as_deref(), Some("a@x.ru"));
        assert!(info.mailbox_guid.is_some());
        assert_eq!(info.display_name, None);
        // Негодные значения не принимаются.
        let bad = json!({"owaUserConfig": {"SessionSettings": {"UserEmailAddress": "без собаки", "MailboxGuid": "", "UserDisplayName": "  "}}});
        assert!(parse_mailbox_info(&bad).is_empty());
        assert!(parse_mailbox_info(&json!({"owaUserConfig": {}})).is_empty());
        assert!(parse_mailbox_info(&json!([])).is_empty());
    }
}
