//! Внешние картинки писем (docs/obkatka.md п. 18) — только по явному действию пользователя:
//! кнопка «Показать» в карточке письма или отправитель в списке доверенных. По умолчанию их нет:
//! такие картинки — частый маяк «письмо прочитано».
//!
//! Картинки скачивает приложение, а не рамка письма: у рамки выхода в сеть нет (CSP `img-src
//! data:`), и он не открывается. Отдельный HTTP-клиент — без cookies, без `Referer`, без сессии OWA;
//! только `http`/`https`, не локальные адреса, только изображения, не больше `MAX_IMAGES` картинок
//! по `MAX_IMAGE_BYTES` и `MAX_TOTAL_BYTES` на письмо. Окну уходят `data:`-адреса по исходному
//! адресу картинки; рамка подставляет их сама. Повторный показ — из кэша процесса. В журнал — только
//! счётчики, без адресов (в адресах картинок бывают метки получателя).

use crate::settings::Prefs;
use crate::{err, AppState, CmdResult};
use aiowa_core::BodyType;
use base64::Engine;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tauri::State;
use tracing::{info, warn};

/// Картинок с одного письма самое большее (остальные остаются скрытыми).
const MAX_IMAGES: usize = 40;
/// Одна картинка и все картинки письма вместе.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 20 * 1024 * 1024;
/// Одновременных загрузок.
const PARALLEL: usize = 4;
/// Кэш процесса: самое большее столько байт `data:`-адресов; переполнение — кэш с нуля.
const CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Клиент и кэш картинок процесса.
#[derive(Default)]
pub(crate) struct ExternalImagesState {
    client: OnceLock<reqwest::Client>,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    map: HashMap<String, Arc<String>>,
    bytes: usize,
}

impl ExternalImagesState {
    fn client(&self) -> CmdResult<reqwest::Client> {
        if let Some(c) = self.client.get() {
            return Ok(c.clone());
        }
        let c = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::limited(5))
            .referer(false)
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AIOWA")
            .build()
            .map_err(|e| format!("HTTP-клиент картинок не создан: {e}"))?;
        Ok(self.client.get_or_init(|| c).clone())
    }

    fn cached(&self, url: &str) -> Option<Arc<String>> {
        self.cache.lock().ok()?.map.get(url).cloned()
    }

    fn remember(&self, url: &str, data: Arc<String>) {
        let Ok(mut c) = self.cache.lock() else { return };
        if c.bytes + data.len() > CACHE_BYTES {
            c.map.clear();
            c.bytes = 0;
        }
        c.bytes += data.len();
        c.map.insert(url.to_string(), data);
    }
}

/// Ответ окну: `data:`-адрес по исходному адресу картинки (как его видит рамка в `src`), сколько
/// внешних картинок в письме и сколько не загрузилось.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExternalImagesDto {
    images: HashMap<String, String>,
    total: usize,
    failed: usize,
}

// --- чистые функции ----------------------------------------------------------------------------

/// Минимальная расшифровка сущностей HTML в значении атрибута — как её делает браузер для
/// `img.getAttribute("src")` у очищенного HTML (ammonia кодирует только эти).
fn decode_entities(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&#39;", "'").replace("&#x27;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Адреса внешних картинок (`<img src="http(s)://…">`) очищенного HTML письма в порядке
/// появления, без повторов, не больше `max`.
pub(crate) fn external_image_urls(html: &str, max: usize) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut from = 0;
    while out.len() < max {
        let Some(pos) = lower[from..].find("<img") else { break };
        let start = from + pos;
        let end = lower[start..].find('>').map(|e| start + e).unwrap_or(lower.len());
        let tag = &html[start..end];
        let tag_lower = &lower[start..end];
        from = end.max(start + 4);
        // Атрибут `src` (не `srcset`, не `data-src`): перед ним пробел.
        let mut search = 0;
        let value = loop {
            let Some(p) = tag_lower[search..].find("src") else { break None };
            let at = search + p;
            search = at + 3;
            let before = tag_lower[..at].chars().last();
            if !before.is_some_and(char::is_whitespace) {
                continue;
            }
            let rest = tag_lower[at + 3..].trim_start();
            if !rest.starts_with('=') {
                continue;
            }
            let offset = tag.len() - tag[at + 3..].trim_start().len() + 1;
            let raw = tag[offset..].trim_start();
            let v = match raw.chars().next() {
                Some(q @ ('"' | '\'')) => raw[1..].split(q).next().unwrap_or(""),
                _ => raw.split(|c: char| c.is_whitespace()).next().unwrap_or(""),
            };
            break Some(decode_entities(v.trim()));
        };
        if let Some(url) = value {
            let l = url.to_ascii_lowercase();
            if (l.starts_with("http://") || l.starts_with("https://")) && !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out
}

/// Адрес, который можно скачивать: `http`/`https`, хост не локальный (не `localhost`, не петля,
/// не link-local, не «любой»).
pub(crate) fn fetchable(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else { return false };
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    match u.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d != "localhost" && !d.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => !(ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()),
        Some(url::Host::Ipv6(ip)) => !(ip.is_loopback() || ip.is_unspecified()),
        None => false,
    }
}

// --- загрузка ------------------------------------------------------------------------------------

/// Одна картинка как `data:`-адрес; отказ — `None` (не картинка, больше предела, сеть).
async fn fetch_one(client: reqwest::Client, url: String, budget: Arc<Mutex<usize>>) -> Option<String> {
    let mut r = client.get(&url).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    if r.content_length().is_some_and(|n| n as usize > MAX_IMAGE_BYTES) {
        return None;
    }
    let content_type = r.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = r.chunk().await.ok()? {
        if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    {
        let mut left = budget.lock().ok()?;
        if bytes.len() > *left {
            return None;
        }
        *left -= bytes.len();
    }
    let mime = crate::image_mime(content_type.as_deref(), &bytes).or_else(|| crate::image_mime(None, &bytes))?;
    Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)))
}

/// Внешние картинки письма: по явному действию окна (кнопка или доверенный отправитель).
/// Настройка «не загружать» — отказ.
#[tauri::command]
pub(crate) async fn external_images(item_pk: i64, state: State<'_, AppState>, images: State<'_, ExternalImagesState>) -> CmdResult<ExternalImagesDto> {
    let (store, account) = (state.store.clone(), state.account);
    let urls = tauri::async_runtime::spawn_blocking(move || -> CmdResult<Vec<String>> {
        if Prefs::load(&store, account).external_images == "never" {
            return Err("Внешние картинки выключены в настройках".into());
        }
        let html = match store.message_body(item_pk).map_err(err)? {
            Some(b) if b.body_type == BodyType::Html => b.unique.or(b.full).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(external_image_urls(&crate::sanitize_keep_cid(&html), MAX_IMAGES))
    })
    .await
    .map_err(err)??;
    let total = urls.len();
    let mut out: HashMap<String, String> = HashMap::new();
    let mut todo: Vec<String> = Vec::new();
    for u in urls {
        match images.cached(&u) {
            Some(d) => {
                out.insert(u, d.as_ref().clone());
            }
            None if fetchable(&u) => todo.push(u),
            None => {}
        }
    }
    let from_cache = out.len();
    if !todo.is_empty() {
        let client = images.client()?;
        let budget = Arc::new(Mutex::new(MAX_TOTAL_BYTES));
        let gate = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
        let mut set = tokio::task::JoinSet::new();
        for u in todo {
            let (client, budget, gate) = (client.clone(), budget.clone(), gate.clone());
            set.spawn(async move {
                let _permit = gate.acquire_owned().await.ok();
                let data = fetch_one(client, u.clone(), budget).await;
                (u, data)
            });
        }
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((u, Some(data))) => {
                    let data = Arc::new(data);
                    images.remember(&u, data.clone());
                    out.insert(u, data.as_ref().clone());
                }
                Ok((_, None)) => {}
                Err(e) => warn!(error = %e, "загрузка внешней картинки прервана"),
            }
        }
    }
    let failed = total - out.len();
    info!(total, loaded = out.len(), from_cache, failed, "внешние картинки письма");
    Ok(ExternalImagesDto { images: out, total, failed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_of_img_src_only_external_unique_and_decoded() {
        let html = r#"<p><img src="https://x.ru/a.png?a=1&amp;b=2" alt="a"><img alt="b" SRC='http://y.ru/b.gif'>
            <img data-src="https://z.ru/no.png"><img srcset="https://z.ru/no2.png 2x" src="cid:logo@x"><img src="https://x.ru/a.png?a=1&amp;b=2">
            <img src=https://q.ru/c.jpg width=10></p>"#;
        assert_eq!(external_image_urls(html, 10), vec!["https://x.ru/a.png?a=1&b=2", "http://y.ru/b.gif", "https://q.ru/c.jpg"]);
        assert_eq!(external_image_urls(html, 1).len(), 1, "предел числа картинок");
        assert!(external_image_urls("<p>без картинок</p>", 10).is_empty());
    }

    #[test]
    fn urls_match_what_the_sanitizer_leaves() {
        // Рамка видит очищенный HTML: адрес с `&` после ammonia и расшифровки — тот же.
        let clean = crate::sanitize_keep_cid(r#"<img src="https://x.ru/p?a=1&b=2" onerror="x()"><script>1</script>"#);
        assert_eq!(external_image_urls(&clean, 10), vec!["https://x.ru/p?a=1&b=2"]);
    }

    #[test]
    fn local_and_odd_addresses_are_not_fetched() {
        assert!(fetchable("https://img.example.com/a.png"));
        assert!(fetchable("http://10.1.2.3/logo.gif"), "внутренние адреса компании — можно (логотипы интранета)");
        for bad in ["http://localhost/a.png", "http://LOCALHOST./a", "http://app.localhost/x", "http://127.0.0.1/a", "http://[::1]/a", "http://169.254.169.254/latest", "http://0.0.0.0/", "ftp://x.ru/a", "file:///c:/a.png", "не адрес"] {
            assert!(!fetchable(bad), "{bad}");
        }
    }
}
