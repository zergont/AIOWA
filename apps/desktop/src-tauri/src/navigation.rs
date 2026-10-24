//! Главное окно никуда не уходит со страницы приложения. Ссылки из писем и карточек открываются
//! в браузере (`open_external`), но если переход всё же начнётся (ссылка без обработчика,
//! `location` из неожиданного места), окно его отменяет: чужая страница в окне получила бы
//! доступ к командам Tauri.
//!
//! Проверяется только главная рамка: на Windows wry вызывает обработчик навигации из
//! `NavigationStarting` (переходы вложенных рамок — `FrameNavigationStarting`, wry его не
//! подписывает), поэтому рамки письма, PDF-предпросмотра и печати сюда не попадают, и протоколу
//! вложений и IPC в списке главной рамки делать нечего.
//!
//! Страница собранного приложения грузится в окно один раз: повторная загрузка (F5 или Ctrl+R,
//! нажатые, пока фокус в рамке PDF, куда обработчики клавиш окна не дотягиваются) потеряла бы
//! набранное письмо и состояние окна. Само приложение страницу не перезагружает (перезапуск после
//! мастера — новый процесс), поэтому любая повторная загрузка отменяется. Браузерные клавиши
//! WebView2 выключены и так (`lib.rs`), это вторая линия. В разработке перезагрузки с dev-сервера
//! разрешены (Vite перезагружает страницу при правках).

use std::collections::HashSet;
use std::sync::Mutex;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime, Url};
use tracing::{debug, warn};

/// Хост собранного приложения на Windows: `http://tauri.localhost` (`https://` — при `useHttpsScheme`).
const APP_HOST: &str = "tauri.localhost";

/// Разрешён ли переход главной рамки окна по адресу. `dev_origin` — адрес dev-сервера в разработке.
pub(crate) fn is_app_url(url: &Url, dev_origin: Option<&str>) -> bool {
    match url.scheme() {
        // about:blank и about:srcdoc — пустые страницы без кода; на WebKit (не Windows) обработчик
        // видит и рамки, там это рамки письма и печати. tauri:// — приложение не на Windows.
        "about" => matches!(url.path(), "blank" | "srcdoc"),
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => is_app_origin(url) || dev_origin.is_some_and(|d| url.origin().ascii_serialization() == d),
        _ => false,
    }
}

/// Источник собранного приложения: ровно `http(s)://tauri.localhost`, без порта и без
/// `user:password@` — у таких адресов тот же хост, но это уже не страница приложения.
fn is_app_origin(url: &Url) -> bool {
    url.host_str() == Some(APP_HOST) && url.port().is_none() && url.username().is_empty() && url.password().is_none()
}

/// Страница собранного приложения (не dev-сервер): её загрузка в окно считается.
fn is_app_page(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => is_app_origin(url),
        _ => false,
    }
}

/// Решение по переходу главной рамки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    /// Чужой адрес.
    External,
    /// Повторная загрузка страницы собранного приложения в то же окно.
    Reload,
}

/// `app_loaded` — страница приложения уже загружалась в это окно.
pub(crate) fn decide(url: &Url, dev_origin: Option<&str>, app_loaded: bool) -> Verdict {
    if !is_app_url(url, dev_origin) {
        Verdict::External
    } else if app_loaded && is_app_page(url) {
        Verdict::Reload
    } else {
        Verdict::Allow
    }
}

/// Плагин с проверкой навигации для всех окон приложения.
pub(crate) fn guard<R: Runtime>() -> TauriPlugin<R> {
    // Окна (по метке), в которые страница приложения уже загружалась.
    let loaded = Mutex::new(HashSet::<String>::new());
    Builder::new("navigation-guard")
        .on_navigation(move |webview, url| {
            let dev = if tauri::is_dev() { webview.app_handle().config().build.dev_url.as_ref().map(|u| u.origin().ascii_serialization()) } else { None };
            let mut seen = loaded.lock().unwrap_or_else(|p| p.into_inner());
            match decide(url, dev.as_deref(), seen.contains(webview.label())) {
                Verdict::Allow => {
                    if is_app_page(url) && seen.insert(webview.label().to_string()) {
                        debug!(window = webview.label(), "страница приложения загружена в окно");
                    }
                    true
                }
                Verdict::External => {
                    // Адрес целиком не пишется: в нём может быть адрес почты или параметры письма.
                    warn!(scheme = url.scheme(), host = url.host_str().unwrap_or(""), "переход окна на внешний адрес отменён");
                    false
                }
                Verdict::Reload => {
                    warn!(window = webview.label(), "повторная загрузка окна отменена");
                    false
                }
            }
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str, dev: Option<&str>) -> bool {
        is_app_url(&Url::parse(s).unwrap(), dev)
    }

    #[test]
    fn only_app_pages_are_allowed() {
        assert!(ok("http://tauri.localhost/", None));
        assert!(ok("https://tauri.localhost/", None));
        assert!(ok("https://tauri.localhost/index.html", None));
        assert!(ok("tauri://localhost/", None));
        assert!(ok("about:blank", None));
        assert!(ok("about:srcdoc", None));
        assert!(ok("http://localhost:5173/", Some("http://localhost:5173")));

        assert!(!ok("http://tauri.localhost:8080/", None));
        assert!(!ok("http://a@tauri.localhost/", None));
        assert!(!ok("http://a:b@tauri.localhost/", None));
        assert!(!ok("http://aiowa-att.localhost/1", None));
        assert!(!ok("http://ipc.localhost/plugin:x", None));
        assert!(!ok("http://localhost:5173/", None));
        assert!(!ok("http://localhost:8080/", Some("http://localhost:5173")));
        assert!(!ok("https://mail.example.ru/owa/", None));
        assert!(!ok("https://tauri.localhost.evil.example/", None));
        assert!(!ok("mailto:ivanov@example.ru", None));
        assert!(!ok("file:///C:/Windows/win.ini", None));
        assert!(!ok("data:text/html,<script>alert(1)</script>", None));
        assert!(!ok("javascript:alert(1)", None));
        assert!(!ok("about:config", None));
    }

    #[test]
    fn app_page_loads_once() {
        let app = Url::parse("http://tauri.localhost/").unwrap();
        assert_eq!(decide(&app, None, false), Verdict::Allow);
        assert_eq!(decide(&app, None, true), Verdict::Reload);
        let index = Url::parse("http://tauri.localhost/index.html").unwrap();
        assert_eq!(decide(&index, None, true), Verdict::Reload);
        // Пустые страницы и dev-сервер не считаются: Vite перезагружает страницу при правках.
        assert_eq!(decide(&Url::parse("about:blank").unwrap(), None, true), Verdict::Allow);
        let dev = Url::parse("http://localhost:5173/").unwrap();
        assert_eq!(decide(&dev, Some("http://localhost:5173"), true), Verdict::Allow);
        assert_eq!(decide(&Url::parse("https://example.com/").unwrap(), None, false), Verdict::External);
    }
}
