# AIOWA

Оффлайн почтовый клиент для корпоративного Exchange в духе Outlook Web App, на Rust, сначала под Windows 11. Работает через тот же внутренний JSON API, что и браузерная OWA. Решения, проверенные факты, архитектура и дорожная карта собраны в [CONCEPT.md](CONCEPT.md); ИИ-функции отложены до паритета с обычным клиентом.

## Состояние

Этап 1, ядро чтения без интерфейса: синхронизация всех почтовых папок в локальную базу SQLite, вложения с извлечением текста, беседы и поиск с операторами из консоли. Работает на живом сервере Exchange 2019.

## Структура

```
crates/aiowa-core           доменные типы
crates/aiowa-transport      trait транспорта и модель ошибок
crates/aiowa-transport-owa  реализация поверх OWA JSON (reqwest, системный TLS)
crates/aiowa-store          SQLite: схема, миграции, FTS5, беседы, кэш вложений
crates/aiowa-extract        текст из вложений: PDF, DOCX, XLSX, TXT/CSV
crates/aiowa-sync           движок синхронизации
crates/aiowa-cli            бинарник `aiowa`
spike/owa-probe             спайк протокола, источник форм запросов
docs/                       протокол, проект этапа 1 и его разбор, опросник для админов и ИБ
```

## Сборка и запуск

Нужен Rust stable. Пароль хранится в диспетчере учётных данных Windows и вводится один раз интерактивно.

```bash
cargo build -p aiowa-cli
./target/debug/aiowa --host mail.example.com --user 'user@domain' set-password
./target/debug/aiowa --host mail.example.com --user 'user@domain' login
./target/debug/aiowa --host mail.example.com --user 'user@domain' sync --days 90
./target/debug/aiowa --host mail.example.com --user 'user@domain' list --folder inbox --limit 30
./target/debug/aiowa --host mail.example.com --user 'user@domain' search "запчасти колыма от:петров после:2026-09 вложение:"
./target/debug/aiowa --host mail.example.com --user 'user@domain' stats
./target/debug/aiowa --host mail.example.com --user 'user@domain' mailbox --record   # чей ящик: адрес и MailboxGuid в базу
./target/debug/aiowa --host mail.example.com --user 'user@domain' db-summary         # сводка базы без сети
```

`--host` и `--user` можно задать переменными `AIOWA_HOST` и `AIOWA_USER`. База по умолчанию: `%LOCALAPPDATA%\AIOWA\<host>\<user>\mail.db`, файлы вложений в `attachments` рядом с ней. `sync` берёт все почтовые папки кроме служебных (`--folders inbox,sentitems` для явного выбора), метаданные за всё время, тела и вложения за `--days`; повторный запуск докачивает только изменения. Операторы поиска: `от:`/`from:`, `кому:`/`to:`, `тема:`/`subject:`, `вложение:`/`has:attachment`, `папка:`/`in:`, `после:`/`до:`; ё и е равнозначны.

## Документы

- [docs/protocol-owa-json.md](docs/protocol-owa-json.md): протокол OWA JSON по наблюдениям со спайка и клиента.
- [docs/stage1-core-design.md](docs/stage1-core-design.md) и [docs/stage1-core-design-review.md](docs/stage1-core-design-review.md): проект этапа 1 и его разбор.
- [docs/questionnaire-admins-security.md](docs/questionnaire-admins-security.md): вопросы администраторам Exchange и ИБ.
- [docs/pseudonymization-design.md](docs/pseudonymization-design.md): проработка схемы обезличивания для будущего ИИ-слоя.
