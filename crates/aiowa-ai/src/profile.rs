//! Подключение к модели (профиль): какой протокол, куда, какая модель; и где находится адресат —
//! на этом компьютере, на своём сервере в сети или в облаке. От места зависит политика
//! приватности (режим «только свои модели» облако не пускает).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::net::IpAddr;
use url::Url;

/// Адрес Anthropic по умолчанию.
pub const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
/// Адрес OpenAI по умолчанию (OpenAI-совместимый протокол).
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";

/// Таймаут по умолчанию, секунд: до первого байта ответа и между кусками потока.
pub const DEFAULT_TIMEOUT_S: u32 = 120;
pub const TIMEOUT_S_MIN: u32 = 10;
pub const TIMEOUT_S_MAX: u32 = 900;
/// Предел текста дополнительных полей запроса, байт.
const EXTRA_MAX: usize = 4096;
/// Поля, которые шлюз задаёт сам: дополнительные поля их не меняют.
const EXTRA_RESERVED: &[&str] = &["model", "messages", "stream", "stream_options", "max_tokens", "max_completion_tokens", "tools", "tool_choice", "n"];

/// Протокол подключения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Messages API Anthropic (`/v1/messages`).
    Anthropic,
    /// OpenAI-совместимый (`/v1/chat/completions`): OpenAI, vLLM, Ollama, LM Studio, llama.cpp.
    #[serde(rename = "openai")]
    OpenAi,
}

/// Где находится модель — для политики приватности и подписи в окне.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// Этот компьютер (`localhost`).
    Local,
    /// Свой сервер в сети (частный адрес, имя внутренней сети или отмечено автором).
    OwnServer,
    /// Облачный провайдер.
    Cloud,
}

/// Профиль подключения. Ключ хранится отдельно (диспетчер учётных данных Windows) и сюда не входит.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    /// Адрес, как его ввёл автор; пустой — адрес провайдера по умолчанию.
    #[serde(default)]
    pub base_url: String,
    /// Модель; пустая — ещё не выбрана (проверка подключения покажет список).
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_timeout")]
    pub timeout_s: u32,
    /// Автор отметил, что это свой сервер, хотя имя похоже на внешнее (`llm.company.ru`).
    /// Известные облачные адреса (OpenAI, Anthropic) это не меняет.
    #[serde(default)]
    pub own_server: bool,
    /// Дополнительные поля запроса к OpenAI-совместимому серверу — JSON-объект как его ввёл автор
    /// (пусто — нет): расширения своих шлюзов, например `{"priority": "interactive", "notes": false,
    /// "reasoning_effort": "low"}` у шлюза Горыныч. Поля, которые задаёт шлюз (`model`, `messages`,
    /// `stream`, длина ответа), не меняются.
    #[serde(default)]
    pub extra: String,
}

fn default_timeout() -> u32 {
    DEFAULT_TIMEOUT_S
}

/// Ошибка в профиле: текст для окна.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError(pub String);

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Profile {
    /// Адрес API без завершающей косой: у Anthropic — корень (`https://api.anthropic.com`), к нему
    /// добавляется `/v1/...`; у OpenAI-совместимых — с `/v1` (`http://srv:8000/v1`), к нему
    /// добавляется `/chat/completions` и `/models`.
    pub fn api_base(&self) -> Result<Url, ProfileError> {
        let raw = self.base_url.trim();
        let raw = if raw.is_empty() {
            match self.kind {
                ProviderKind::Anthropic => ANTHROPIC_BASE,
                ProviderKind::OpenAi => OPENAI_BASE,
            }
        } else {
            raw
        };
        // Без схемы — http для своих серверов было бы удобно, но небезопасно для облака:
        // схему автор пишет сам.
        let mut url = Url::parse(raw).map_err(|_| ProfileError(format!("Неверный адрес «{raw}»: нужен вид http://сервер:порт или https://сервер")))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
            return Err(ProfileError(format!("Неверный адрес «{raw}»: нужен вид http://сервер:порт или https://сервер")));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ProfileError("В адресе подключения не должно быть «?» и «#»".into()));
        }
        let path = url.path().trim_end_matches('/').to_string();
        let path = match self.kind {
            // `https://api.anthropic.com/v1` тоже понятен: `/v1` добавляется к каждому запросу.
            ProviderKind::Anthropic => path.strip_suffix("/v1").unwrap_or(&path).to_string(),
            // Корень сервера — к нему `/v1`; свой путь (прокси, шлюз) — как есть.
            ProviderKind::OpenAi if path.is_empty() => "/v1".to_string(),
            ProviderKind::OpenAi => path,
        };
        url.set_path(&path);
        Ok(url)
    }

    /// Где находится адресат (для политики и подписи).
    pub fn place(&self) -> Place {
        if self.kind == ProviderKind::Anthropic {
            return Place::Cloud;
        }
        let Ok(url) = self.api_base() else { return Place::Cloud };
        let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']).to_ascii_lowercase();
        if is_known_cloud(&host) {
            return Place::Cloud;
        }
        if host == "localhost" || host.ends_with(".localhost") {
            return Place::Local;
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            if ip.is_loopback() {
                return Place::Local;
            }
            if is_private(ip) {
                return Place::OwnServer;
            }
        } else if !host.contains('.') || [".local", ".lan", ".internal", ".intranet", ".corp", ".home.arpa"].iter().any(|s| host.ends_with(s)) {
            // Имя без точки или внутренней зоны — из сети компании или дома.
            return Place::OwnServer;
        }
        if self.own_server { Place::OwnServer } else { Place::Cloud }
    }

    /// Имя адресата для журнала: только хост, без пути и ключей.
    pub fn host(&self) -> String {
        self.api_base().ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
    }

    /// Дополнительные поля запроса: `None` — их нет. Ошибка — не объект, зарезервированное поле
    /// или поля у Anthropic.
    pub fn extra_fields(&self) -> Result<Option<Map<String, Value>>, ProfileError> {
        let raw = self.extra.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        if self.kind == ProviderKind::Anthropic {
            return Err(ProfileError("Дополнительные поля запроса — только у OpenAI-совместимых подключений".into()));
        }
        if raw.len() > EXTRA_MAX {
            return Err(ProfileError("Дополнительные поля запроса слишком длинные".into()));
        }
        let map = match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(m)) => m,
            _ => return Err(ProfileError("Дополнительные поля запроса — JSON-объект, например {\"priority\": \"interactive\"}".into())),
        };
        if let Some(k) = map.keys().find(|k| EXTRA_RESERVED.contains(&k.as_str())) {
            return Err(ProfileError(format!("Поле «{k}» задаёт сам AIOWA: уберите его из дополнительных полей")));
        }
        Ok((!map.is_empty()).then_some(map))
    }

    /// Проверка перед сохранением: имя, адрес, таймаут, дополнительные поля.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.name.trim().is_empty() {
            return Err(ProfileError("Укажите название подключения".into()));
        }
        self.api_base()?;
        if !(TIMEOUT_S_MIN..=TIMEOUT_S_MAX).contains(&self.timeout_s) {
            return Err(ProfileError(format!("Таймаут — от {TIMEOUT_S_MIN} до {TIMEOUT_S_MAX} секунд")));
        }
        if self.model.chars().any(char::is_control) {
            return Err(ProfileError("Неверное название модели".into()));
        }
        self.extra_fields()?;
        Ok(())
    }
}

fn is_known_cloud(host: &str) -> bool {
    const CLOUD: &[&str] = &["openai.com", "anthropic.com", "openrouter.ai", "mistral.ai", "groq.com", "together.xyz", "deepseek.com", "googleapis.com", "x.ai"];
    CLOUD.iter().any(|c| host == *c || host.ends_with(&format!(".{c}")))
}

/// Частные, служебные и локальные адреса: такой сервер — в своей сети.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private() || v4.is_link_local() || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kind: ProviderKind, base: &str) -> Profile {
        Profile { id: "p".into(), name: "П".into(), kind, base_url: base.into(), model: "m".into(), timeout_s: DEFAULT_TIMEOUT_S, own_server: false, extra: String::new() }
    }

    #[test]
    fn api_base_is_normalized() {
        assert_eq!(p(ProviderKind::Anthropic, "").api_base().unwrap().as_str(), "https://api.anthropic.com/");
        assert_eq!(p(ProviderKind::Anthropic, "https://api.anthropic.com/v1/").api_base().unwrap().as_str(), "https://api.anthropic.com/");
        assert_eq!(p(ProviderKind::OpenAi, "").api_base().unwrap().as_str(), "https://api.openai.com/v1");
        assert_eq!(p(ProviderKind::OpenAi, "http://gpu:8000").api_base().unwrap().as_str(), "http://gpu:8000/v1");
        assert_eq!(p(ProviderKind::OpenAi, "http://gpu:8000/").api_base().unwrap().as_str(), "http://gpu:8000/v1");
        assert_eq!(p(ProviderKind::OpenAi, "http://localhost:1234/v1/").api_base().unwrap().as_str(), "http://localhost:1234/v1");
        assert_eq!(p(ProviderKind::OpenAi, "https://gw.corp/openai/v1").api_base().unwrap().as_str(), "https://gw.corp/openai/v1");
        assert!(p(ProviderKind::OpenAi, "gpu:8000").api_base().is_err(), "без схемы");
        assert!(p(ProviderKind::OpenAi, "ftp://gpu").api_base().is_err());
        assert!(p(ProviderKind::OpenAi, "http://gpu:8000/v1?key=1").api_base().is_err());
    }

    #[test]
    fn place_follows_address() {
        assert_eq!(p(ProviderKind::Anthropic, "http://localhost:9").place(), Place::Cloud, "Anthropic — всегда облако");
        assert_eq!(p(ProviderKind::OpenAi, "").place(), Place::Cloud);
        assert_eq!(p(ProviderKind::OpenAi, "http://localhost:11434").place(), Place::Local);
        assert_eq!(p(ProviderKind::OpenAi, "http://127.0.0.1:1234/v1").place(), Place::Local);
        assert_eq!(p(ProviderKind::OpenAi, "http://[::1]:8000").place(), Place::Local);
        assert_eq!(p(ProviderKind::OpenAi, "http://192.168.1.20:8000").place(), Place::OwnServer);
        assert_eq!(p(ProviderKind::OpenAi, "http://10.0.0.5:8000").place(), Place::OwnServer);
        assert_eq!(p(ProviderKind::OpenAi, "http://gpu-box:8000").place(), Place::OwnServer);
        assert_eq!(p(ProviderKind::OpenAi, "http://llm.office.lan:8000").place(), Place::OwnServer);
        assert_eq!(p(ProviderKind::OpenAi, "https://llm.example.ru").place(), Place::Cloud, "внешнее имя — облако, пока автор не отметит");
        let mut own = p(ProviderKind::OpenAi, "https://llm.example.ru");
        own.own_server = true;
        assert_eq!(own.place(), Place::OwnServer);
        let mut fake = p(ProviderKind::OpenAi, "https://api.openai.com/v1");
        fake.own_server = true;
        assert_eq!(fake.place(), Place::Cloud, "известное облако отметкой не становится своим");
        assert_eq!(p(ProviderKind::OpenAi, "http://8.8.8.8:80").place(), Place::Cloud);
    }

    #[test]
    fn validate_checks_name_address_timeout() {
        assert!(p(ProviderKind::OpenAi, "http://gpu:8000").validate().is_ok());
        let mut bad = p(ProviderKind::OpenAi, "http://gpu:8000");
        bad.name = " ".into();
        assert!(bad.validate().is_err());
        let mut slow = p(ProviderKind::OpenAi, "http://gpu:8000");
        slow.timeout_s = 5;
        assert!(slow.validate().is_err());
        assert_eq!(p(ProviderKind::OpenAi, "http://gpu:8000/v1").host(), "gpu");
    }

    #[test]
    fn extra_fields_are_an_object_without_reserved_keys() {
        let mut g = p(ProviderKind::OpenAi, "http://192.168.0.79:4000/src/aiowa/v1");
        assert_eq!(g.place(), Place::OwnServer);
        assert_eq!(g.api_base().unwrap().as_str(), "http://192.168.0.79:4000/src/aiowa/v1");
        assert_eq!(g.extra_fields().unwrap(), None);
        g.extra = r#"{"priority":"interactive","notes":false}"#.into();
        let m = g.extra_fields().unwrap().unwrap();
        assert_eq!(m["priority"], "interactive");
        assert!(g.validate().is_ok());
        g.extra = "[1]".into();
        assert!(g.validate().is_err(), "не объект");
        g.extra = r#"{"stream":false}"#.into();
        assert!(g.validate().unwrap_err().0.contains("stream"));
        let mut a = p(ProviderKind::Anthropic, "");
        a.extra = r#"{"x":1}"#.into();
        assert!(a.validate().is_err(), "у Anthropic дополнительных полей нет");
    }
}
