# AIOWA Desktop

Окно почты поверх локальной базы SQLite: дерево папок, список бесед, область чтения, поиск.
Приложение только читает базу — синхронизацию выполняет `aiowa sync` (crates/aiowa-cli).

Стек: Tauri 2 (Rust, WebView2) + Svelte 5 + Vite 8 + TypeScript.

## Запуск в разработке

```powershell
cd apps\desktop
npm install
$env:AIOWA_HOST = 'mail.ngenergo.ru'
$env:AIOWA_USER = 'login@domain'
npx.cmd tauri dev
```

Переменные окружения:

| Переменная   | Назначение                                                                 |
|--------------|----------------------------------------------------------------------------|
| `AIOWA_HOST` | Хост OWA, по нему и логину выбирается учётная запись в базе                |
| `AIOWA_USER` | Логин, как при `aiowa sync`                                                |
| `AIOWA_DB`   | Явный путь к `mail.db`; по умолчанию `%LOCALAPPDATA%\AIOWA\<host>\<user>\mail.db` |

`npx.cmd` вместо `npx`: в Windows PowerShell с политикой Restricted скрипт `npx.ps1` не запускается.

Если на машине несколько установок Rust, укажи ту, где стоит toolchain проекта (нужен rustc ≥ 1.90):

```powershell
$env:RUSTUP_HOME = 'D:\rust\rustup'
$env:CARGO_HOME  = 'D:\rust\cargo'
```

## Проверки

```powershell
npm run check            # svelte-check + tsc
npx.cmd vite build       # сборка фронтенда в dist/
cargo check -p aiowa-desktop
cargo test  -p aiowa-desktop --lib
```

## Структура

- `src/App.svelte` — три панели и строка поиска, состояние выбранной папки/беседы.
- `src/lib/components/` — `FolderTree`, `ConversationList` (виртуализация строк), `ReadingPane`,
  `MessageCard` (письмо в `iframe sandbox` с CSP), `SearchResults`.
- `src/lib/api.ts` — типы DTO и обёртки `invoke` над командами Tauri.
- `src/tokens.css` — дизайн-токены (светлая/тёмная тема, Fluent-подобные).
- `src-tauri/src/lib.rs` — команды `account_info`, `folders`, `conversations`, `conversation`, `search`;
  очистка HTML через ammonia.

## Безопасность отображения писем

HTML письма очищается на стороне Rust (ammonia: без скриптов, обработчиков событий и
`javascript:`), затем показывается в `<iframe sandbox="allow-same-origin" srcdoc>` с
`Content-Security-Policy: default-src 'none'; img-src data: cid:; style-src 'unsafe-inline'`.
Внешние картинки и шрифты не загружаются вообще.
