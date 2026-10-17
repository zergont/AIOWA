# AIOWA Desktop

Окно почты поверх локальной базы SQLite: дерево папок, список бесед, область чтения, поиск.
Синхронизацию с сервером приложение запускает само (строка состояния внизу окна); `aiowa sync`
из консоли остаётся для проб и полных прогонов с нестандартными ключами.

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

## Синхронизация из окна

- Пароль берётся из диспетчера учётных данных Windows (служба `AIOWA`, ключ = логин), как у CLI:
  перед первым запуском выполни `aiowa --host … --user … set-password`. Формы ввода пароля в окне нет;
  без пароля строка состояния покажет ошибку с этой подсказкой.
- Одна сессия OWA на процесс: вход при первой синхронизации, `logoff.owa` при закрытии окна
  (при жёстком завершении процесса, например при пересборке `tauri dev`, сессия на сервере остаётся
  до истечения; лимит сервера около 10 сессий).
- Запуск: автоматически при старте, затем раз в 2 минуты при видимом окне, и по кнопке «Обновить».
  Повторный запуск, пока идёт предыдущий, не делается. «Остановить» прерывает после текущей пачки.
- Что синхронизируется: все почтовые папки (как `--folders all`), тела и вложения за 90 дней
  (`sync_start { days }` позволяет задать иное окно; 0 = без ограничения).
- Команды: `sync_start`, `sync_status`, `sync_cancel`; ход приходит событием `sync-progress`
  (структура `SyncStatus` в `src/lib/api.ts`). Время последней успешной синхронизации хранится в базе
  (`sync_state`, ключ `desktop.last_sync_ok_ms`) и показывается после перезапуска.
- После завершения окно перечитывает папки, текущий список бесед и открытую беседу, не сбивая выбор.

## Проверки

```powershell
npm run check            # svelte-check + tsc
npx.cmd vite build       # сборка фронтенда в dist/
cargo check -p aiowa-desktop
cargo test  -p aiowa-desktop --lib
```

## Структура

- `src/App.svelte` — три панели, строка поиска и строка состояния; выбранная папка/беседа;
  автозапуск синхронизации и обновление списков после неё.
- `src/lib/components/` — `FolderTree`, `ConversationList` (виртуализация строк), `ReadingPane`,
  `MessageCard` (письмо в `iframe sandbox` с CSP), `SearchResults`, `StatusBar` (кнопка «Обновить»,
  ход, время последней синхронизации, ошибки).
- `src/lib/api.ts` — типы DTO, обёртки `invoke` над командами Tauri и подписка `onSyncProgress`.
- `src/tokens.css` — дизайн-токены (светлая/тёмная тема, Fluent-подобные).
- `src-tauri/src/lib.rs` — команды `account_info`, `folders`, `conversations`, `conversation`, `search`;
  очистка HTML через ammonia; выход из сессии OWA при закрытии.
- `src-tauri/src/sync.rs` — состояние синхронизации, транспорт OWA процесса, команды `sync_*`,
  событие `sync-progress`.

## Безопасность отображения писем

HTML письма очищается на стороне Rust (ammonia: без скриптов, обработчиков событий и
`javascript:`), затем показывается в `<iframe sandbox="allow-same-origin" srcdoc>` с
`Content-Security-Policy: default-src 'none'; img-src data: cid:; style-src 'unsafe-inline'`.
Внешние картинки и шрифты не загружаются вообще.
