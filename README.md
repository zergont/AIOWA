# AIOWA

Оффлайн почтовый клиент для корпоративного Exchange в духе Outlook Web App, на Rust, сначала под Windows 11. Работает через тот же внутренний JSON API, что и браузерная OWA. Решения, проверенные факты, архитектура и дорожная карта собраны в [CONCEPT.md](CONCEPT.md); ИИ-функции отложены до паритета с обычным клиентом.

## Состояние

Этап 1, ядро чтения без интерфейса: синхронизация почты в локальную базу SQLite и полнотекстовый поиск из консоли. Работает на живом сервере Exchange 2019.

## Структура

```
crates/aiowa-core           доменные типы
crates/aiowa-transport      trait транспорта и модель ошибок
crates/aiowa-transport-owa  реализация поверх OWA JSON (reqwest, системный TLS)
crates/aiowa-store          SQLite: схема, миграции, FTS5
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
./target/debug/aiowa --host mail.example.com --user 'user@domain' search "запчасти колыма"
./target/debug/aiowa --host mail.example.com --user 'user@domain' stats
```

`--host` и `--user` можно задать переменными `AIOWA_HOST` и `AIOWA_USER`. База по умолчанию: `%LOCALAPPDATA%\AIOWA\<host>\<user>\mail.db`. Повторный `sync` докачивает только изменения.

## Документы

- [docs/protocol-owa-json.md](docs/protocol-owa-json.md): протокол OWA JSON по наблюдениям со спайка и клиента.
- [docs/stage1-core-design.md](docs/stage1-core-design.md) и [docs/stage1-core-design-review.md](docs/stage1-core-design-review.md): проект этапа 1 и его разбор.
- [docs/questionnaire-admins-security.md](docs/questionnaire-admins-security.md): вопросы администраторам Exchange и ИБ.
- [docs/pseudonymization-design.md](docs/pseudonymization-design.md): проработка схемы обезличивания для будущего ИИ-слоя.
