# owa-probe

Консольный спайк протокола OWA JSON для Exchange 2019. Проверяет, что клиент на Rust может войти через форму OWA, получить данные сессии, читать папки и беседы и держать сессию. Результаты наблюдений сведены в [docs/protocol-owa-json.md](../../docs/protocol-owa-json.md).

## Сборка

Нужен Rust (stable). TLS берётся из системы Windows (`native-tls`): сервер работает на TLS 1.2 с CBC-шифром, который rustls не поддерживает.

```bash
cargo build
```

## Пароль

Пароль хранится в диспетчере учётных данных Windows (служба `AIOWA`, пользователь = логин OWA) и никогда не передаётся аргументом. Сохранить его один раз, интерактивно, из своего терминала:

```powershell
powershell -ExecutionPolicy Bypass -File .\set-password.ps1
```

Скрипт спросит логин и пароль и запишет логин (не пароль) в `.env.local`, который исключён из git. Удалить пароль: `owa-probe ... forget-password`.

## Команды

```text
owa-probe --host <host> --user <login> login                 вход, cookies без значений, сводка сессии
owa-probe --host <host> --user <login> folders               дерево папок с числом элементов
owa-probe --host <host> --user <login> inbox [--count N] [--folder inbox|sentitems|...]
owa-probe --host <host> --user <login> probe                 проверки транспорта и операций
owa-probe --host <host> --user <login> soak --idle-min N     жизнь сессии после N минут простоя
```

`--host` и `--user` можно задать переменными окружения `AIOWA_HOST` и `AIOWA_USER`.

## Что уже подтверждено

- Вход через `POST /owa/auth.owa`, cookies `cadata*`, канарейка из `GET /owa/`.
- `sessiondata.ashx` отдаёт профиль, дерево папок и первую страницу входящих.
- Операции `FindConversation`, `GetFolder`, `SyncFolderItems`, `SyncFolderHierarchy`, `GetConversationItems` (с `UniqueBody`).
- `__type` обязан быть первым ключом каждого JSON-объекта (`serde_json` с `preserve_order`).
- Достаточно заголовков `Action`, `X-OWA-CANARY`, `Content-Type`.
