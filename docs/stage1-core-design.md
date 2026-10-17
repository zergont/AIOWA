# Этап 1, ядро чтения: проект (черновик от 16.10.2026)

Цель этапа: без интерфейса выкачать почту в локальную базу и искать по ней из консоли. Критерий: `aiowa sync --days 90` заполняет базу входящими и отправленными за три месяца с телами; `aiowa search <запрос>` отвечает за сотни миллисекунд; повторный `sync` докачивает только изменения.

Входные факты из спайка: см. `protocol-owa-json.md`. Критичные: системный TLS (`native-tls`), `__type` первым ключом каждого JSON-объекта, одна сессия на процесс с `logoff.owa` в конце, истечение сессии = 440/401/редирект на `logon.aspx`, отказ CSRF = редирект на `errorfe.aspx`.

## 1. Воркспейс

```
Cargo.toml                 — [workspace], общие версии зависимостей, профиль release
crates/aiowa-core          — доменные типы без IO
crates/aiowa-transport     — trait MailTransport, DTO, ошибки
crates/aiowa-transport-owa — реализация поверх OWA JSON (reqwest async + native-tls)
crates/aiowa-store         — SQLite: схема, миграции, запросы, FTS5
crates/aiowa-sync          — движок синхронизации поверх transport + store
crates/aiowa-cli           — бинарник `aiowa`: login, folders, sync, search, stats
spike/owa-probe            — остаётся как есть, источник форм запросов
```

Направление зависимостей: `core ← transport ← transport-owa`; `core ← store`; `{transport, store} ← sync`; всё ← `cli`. Асинхронность: tokio с первого дня, потому что движок синхронизации и будущий Tauri живут в async; CLI оборачивает в `#[tokio::main]`.

## 2. Доменные типы (`aiowa-core`)

- `AccountId(i64)`: в схеме с первой миграции, в v1 всегда один аккаунт.
- `FolderId { id: String, change_key: Option<String> }`, `ItemId` то же, `ConversationId(String)`: непрозрачные строки сервера; локальные суррогатные ключи `folder_pk`, `item_pk` (i64) живут только в базе.
- `DistinguishedFolder` enum: Inbox, SentItems, Drafts, DeletedItems, JunkEmail, Archive, Calendar, Root (`msgfolderroot`), с `as_str()` для протокола.
- `Folder { id, parent_id, display_name, distinguished, folder_class, total_count, unread_count, child_count }`.
- `MessageMeta { item_id, conversation_id, internet_message_id, parent_folder_id, subject, from, sender, to, cc, received_at, sent_at, is_read, has_attachments, importance, size, item_class, preview }`.
- `Participant { name, email, routing_type }`.
- `MessageBody { item_id, body_type: Html|Text, unique_body: Option<String>, body: Option<String>, is_truncated }`.
- `AttachmentMeta { id, item_id, name, content_type, size, is_inline, content_id }`.
- `SyncState(String)`: непрозрачно.
- Время: `jiff::Timestamp` в типах, в базе `INTEGER` миллисекунд UTC.

## 3. Trait транспорта (`aiowa-transport`)

```rust
#[async_trait]
pub trait MailTransport: Send + Sync {
    async fn sync_folder_hierarchy(&self, state: Option<&SyncState>) -> Result<FolderChanges, TransportError>;
    async fn sync_folder_items(&self, folder: &FolderRef, state: Option<&SyncState>, max: u32) -> Result<ItemChanges, TransportError>;
    async fn get_items(&self, ids: &[ItemId], shape: ItemShape) -> Result<Vec<MessageFull>, TransportError>;
    async fn get_conversation_items(&self, id: &ConversationId, state: Option<&SyncState>) -> Result<ConversationItems, TransportError>;
    async fn find_conversations(&self, folder: &FolderRef, page: Page) -> Result<ConversationPage, TransportError>;
    async fn get_folder_counts(&self, folders: &[FolderRef]) -> Result<Vec<FolderCounts>, TransportError>;
    fn capabilities(&self) -> Capabilities;
}
```

- `FolderRef` = `Distinguished(DistinguishedFolder) | Id(FolderId)`.
- `ItemChanges { state: SyncState, created: Vec<ItemId>, updated: Vec<ItemId>, deleted: Vec<ItemId>, read_flag: Vec<(ItemId, bool)>, has_more: bool }`.
- `TransportError`: `SessionExpired`, `CsrfRejected`, `SessionLimit`, `Throttled { retry_after }`, `Network(source)`, `Protocol { op, status, raw }`, `Server { code, message }`, `Cancelled`; предикаты `is_retryable()`, `needs_reauth()`.
- Учётные данные через `trait CredentialProvider { fn password(&self, user) -> Result<Secret<String>> }`; реализация на `keyring` в CLI, не в транспорте.

## 4. Реализация OWA JSON (`aiowa-transport-owa`)

- `OwaSession`: `reqwest::Client` с cookie jar, `Policy::none()`, `native-tls`, таймаут 60 с; поля `canary`, `build_version` (из `x-owa-version`), `action_id: AtomicI64`.
- Вход как в спайке; повторный вход single-flight через `tokio::sync::Mutex<Option<Login>>`; одна попытка, затем `SessionExpired` наружу (без фоновых повторов).
- Конверт типизирован: `struct Envelope<B: Serialize> { #[serde(rename = "__type")] type_name: &'static str, header: RequestHeader, body: B }`; поля `__type` объявлены первыми в каждой структуре, serde сохраняет порядок полей структур. Для `serde_json::Value` там, где он нужен, фича `preserve_order`.
- Заголовки запроса: `Action`, `X-OWA-CANARY`, `Content-Type`; JSON в теле. Остальные `X-OWA-*` не отправляем (проверено).
- Ответы: толерантные структуры `#[serde(default)]`, без `deny_unknown_fields`, неизвестные поля в `extra: Map` на уровне debug-лога.
- Операции первой очереди: `SyncFolderHierarchy`, `SyncFolderItems`, `GetItem` (батчи до 50 id; форма с явными свойствами либо `ShapeName`, выбирается по результату пробы), `GetConversationItems`, `FindConversation`, `GetFolder`; `logoff`.
- Ограничения: не больше 2 одновременных запросов (`Semaphore`), backoff при `ErrorServerBusy`/`BackOffMilliseconds`, пауза 30 с при `SessionLimit`.

## 5. Хранилище (`aiowa-store`)

Один файл `mail.db` в `%LOCALAPPDATA%\AIOWA\<account>\` (переопределяется флагом), `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`, `auto_vacuum=INCREMENTAL`. Второй файл `index.db` для векторов появится на этапе 2; FTS5 живёт в `mail.db`, потому что это внешняя таблица по содержимому `items`.

```sql
CREATE TABLE accounts (
  account_id   INTEGER PRIMARY KEY,
  host         TEXT NOT NULL,
  login        TEXT NOT NULL,
  created_at   INTEGER NOT NULL
);

CREATE TABLE folders (
  folder_pk        INTEGER PRIMARY KEY,
  account_id       INTEGER NOT NULL REFERENCES accounts(account_id),
  server_id        TEXT NOT NULL,            -- FolderId.Id
  change_key       TEXT,
  parent_server_id TEXT,
  distinguished    TEXT,                     -- inbox, sentitems, ... или NULL
  display_name     TEXT NOT NULL,
  folder_class     TEXT,
  total_count      INTEGER NOT NULL DEFAULT 0,
  unread_count     INTEGER NOT NULL DEFAULT 0,
  sync_state       TEXT,                     -- SyncFolderItems state этой папки
  sync_priority    INTEGER NOT NULL DEFAULT 100,
  last_synced_at   INTEGER,
  deleted          INTEGER NOT NULL DEFAULT 0,
  UNIQUE(account_id, server_id)
);

CREATE TABLE items (
  item_pk             INTEGER PRIMARY KEY,
  account_id          INTEGER NOT NULL REFERENCES accounts(account_id),
  server_id           TEXT NOT NULL,         -- ItemId.Id
  change_key          TEXT,
  folder_pk           INTEGER NOT NULL REFERENCES folders(folder_pk),
  conversation_id     TEXT,
  internet_message_id TEXT,
  item_class          TEXT,
  subject             TEXT,
  from_name           TEXT,
  from_email          TEXT,
  sender_email        TEXT,
  received_at         INTEGER,
  sent_at             INTEGER,
  is_read             INTEGER NOT NULL DEFAULT 0,
  has_attachments     INTEGER NOT NULL DEFAULT 0,
  importance          TEXT,
  size                INTEGER,
  preview             TEXT,
  body_state          INTEGER NOT NULL DEFAULT 0,   -- 0 нет тела, 1 есть, 2 ошибка загрузки
  deleted             INTEGER NOT NULL DEFAULT 0,
  updated_at          INTEGER NOT NULL,
  UNIQUE(account_id, server_id)
);
CREATE INDEX items_folder_received ON items(folder_pk, received_at DESC);
CREATE INDEX items_conversation ON items(conversation_id);
CREATE INDEX items_internet_id ON items(internet_message_id);
CREATE INDEX items_body_state ON items(body_state) WHERE body_state = 0;

CREATE TABLE recipients (
  item_pk    INTEGER NOT NULL REFERENCES items(item_pk) ON DELETE CASCADE,
  kind       INTEGER NOT NULL,              -- 1 to, 2 cc, 3 bcc
  name       TEXT,
  email      TEXT,
  position   INTEGER NOT NULL
);
CREATE INDEX recipients_email ON recipients(email);

CREATE TABLE bodies (
  item_pk       INTEGER PRIMARY KEY REFERENCES items(item_pk) ON DELETE CASCADE,
  body_type     TEXT NOT NULL,              -- HTML | Text
  unique_body   BLOB,                        -- zstd(UTF-8)
  full_body     BLOB,                        -- zstd(UTF-8), может быть NULL до этапа «полные тела»
  unique_text   TEXT,                        -- очищенный текст UniqueBody для FTS и ИИ
  is_truncated  INTEGER NOT NULL DEFAULT 0,
  fetched_at    INTEGER NOT NULL
);

CREATE TABLE attachments (
  attachment_pk INTEGER PRIMARY KEY,
  item_pk       INTEGER NOT NULL REFERENCES items(item_pk) ON DELETE CASCADE,
  server_id     TEXT NOT NULL,
  name          TEXT,
  content_type  TEXT,
  size          INTEGER,
  is_inline     INTEGER NOT NULL DEFAULT 0,
  content_id    TEXT
);

CREATE TABLE sync_state (
  account_id  INTEGER NOT NULL REFERENCES accounts(account_id),
  key         TEXT NOT NULL,                -- 'hierarchy', 'bodies_cursor', ...
  value       TEXT,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY(account_id, key)
);

CREATE VIRTUAL TABLE items_fts USING fts5(
  subject, from_name, from_email, recipients, unique_text,
  content='',                                 -- внешнее содержимое: заполняется кодом, не триггерами
  tokenize = 'unicode61 remove_diacritics 2'
);
-- rowid = item_pk; запись в FTS делается тем же кодом, что пишет items/bodies, в одной транзакции.
```

Решения: тела хранятся сжатыми (zstd) и отдельно текстовая выжимка для поиска; FTS-таблица без `content=` таблицы-источника, чтобы не держать тексты дважды; русский стемминг и теневая колонка откладываются на этап 2 (начнём с `unicode61` и оценим качество на реальных запросах). Удаления сервера зеркалятся флагом `deleted`, физическая чистка отложенной задачей.

## 6. Синхронизация (`aiowa-sync`)

Фазы при `sync`:

1. `SyncFolderHierarchy` от `msgfolderroot` с сохранённым состоянием → upsert папок, пометка удалённых. Приоритеты: inbox 0, sentitems 1, остальные почтовые папки 50, служебные (junk, deleted, drafts) 90, календарь и контакты не синхронизируются на этапе 1.
2. Для каждой папки по приоритету: `SyncFolderItems` пачками по 512 до `IncludesLastItemInRange`; создание/обновление пишется в `items` с метаданными из той же выдачи, если форма их содержит, иначе id-заглушки с `body_state=0` и отдельная фаза метаданных. Фильтр по глубине (`--days N`) применяется на стороне клиента: элементы старше окна не запрашиваются телами, но id и даты сохраняются.
3. Тела: выборка `items` где `body_state=0` в окне, батчи `GetItem` по 20–50 id с формой «метаданные + UniqueBody + вложения»; запись `bodies`, `attachments`, `recipients`, FTS в одной транзакции на батч.
4. Повторный `sync`: шаги 1–3 с сохранёнными состояниями; изменения флагов чтения и удаления применяются точечно.

Ограничения: 2 параллельных запроса, транзакция на батч, `wal_checkpoint(TRUNCATE)` каждые 5 000 писем, журнал прогресса в stdout (`tracing`), отмена по Ctrl+C с сохранением состояния после текущего батча.

## 7. CLI

`aiowa --host H --user U <cmd>`: `login` (проверка + logoff), `folders`, `sync [--days 90] [--folders inbox,sentitems] [--no-bodies]`, `search <запрос> [--limit 20]` (FTS5 с подсветкой сниппетов; порядок от новых к старым, как в OWA, bm25 только разводит одинаковые даты), `stats` (размер базы, покрытие по папкам и датам). Пароль через `keyring`, как в спайке.

## 8. Что проверить до написания кода

- Форма `SyncFolderItems`: как выглядят `Changes` (массив или объект по типам), приходят ли метаданные при `BaseShape: IdOnly` с `ShapeName`, какой `ShapeName` допустим.
- Форма `GetItem`: какие `FieldURI` принимает OWA JSON (`item:Subject` или `Subject`), доступен ли `UniqueBody` вне контекста беседы, работает ли `ShapeName: ItemPart` в `GetItem`, лимит id в одном запросе.
- Поведение при `--days`: `SyncFolderItems` не умеет фильтровать по дате, значит первичная выгрузка всё равно пройдёт по всем id папки (77 тыс. во входящих = ~150 вызовов по 512). Приемлемо.

## 8a. Что изменилось после разбора (16.10.2026, см. `stage1-core-design-review.md`)

Принято и уже в коде:
- FTS5 с внешним содержимым по представлению `items_fts_src` (а не contentless): сниппеты работают, тексты не дублируются; флаг `items.fts_indexed`, снятие с индекса до изменения источника (блокер Б1).
- Состояние синхронизации пишется в одной транзакции с пачкой: `apply_item_batch(folder_pk, changes, state, complete)` и `apply_hierarchy_batch(changes, state)` (блокер Б2).
- `ItemChanges` несёт метаданные (`AdditionalProperties` в `SyncFolderItems` с префиксами `item:`/`message:`), `get_items` возвращает `Vec<ItemResult>` по позиции, `ErrorItemNotFound` = удаление (блокер Б3).
- Перемещения: при `Create` неизвестного id ищется удалённая строка с тем же `InternetMessageId` и временем получения и усыновляется с сохранением тела и места в индексе (блокер Б4, упрощённо: без копирования тела для живой строки).
- `TimeZoneContext = UTC`, только HTTP/1.1, `connect_timeout` 10 с; `SyncFolderHierarchy` с `BaseShape: AllProperties` (при `IdOnly` сервер молча не отдаёт `DisplayName`).
- Предохранители цикла `SyncFolderItems`: три пустые выдачи подряд = папка завершена, трижды неизменный `SyncState` = ошибка.

Отложено до следующей миграции схемы (пока база тестовая и пересоздаётся): `STRICT`-таблицы, таблицы `conversations`/`conversation_folders`, `categories`, `pending_ops`, `drafts`, `address_book`, `sort_ts`, маска `body_state`, `body_attempts`, поколения `seen_gen` для полного ресинка, кэш вложений в файлах, словари zstd, `fts_norm()` с ё→е, адаптивный размер пачки `GetItem`, параллельная фаза тел, файловая блокировка и коды выхода CLI. Все они не меняют API между крейтами и вносятся по мере появления соответствующих функций клиента.

## 8b. Первые цифры на реальном ящике (16–17.10.2026)

| Что | Результат |
|---|---|
| Полная выгрузка метаданных входящих и отправленных | 102 188 писем, 200 пачек по 512, 1 242 с вместе с телами за 90 дней; ошибок 0, троттлинга не было |
| Тела за 90 дней (`GetItem`, `ItemPart`, пачки по 25) | 1 845 писем, около 1 с на пачку |
| Инкрементальный запуск через 20 минут | 2,0 с: иерархия без изменений, две папки по одной пустой пачке |
| Миграция схемы 1 → 2 с пересборкой FTS | около 7 с на 90 тыс. писем |
| Размер базы | около 180 МБ на 102 тыс. писем с метаданными и 1,8 тыс. тел; с индексом метаданных по всем письмам около 300 МБ (включая WAL до контрольной точки) |
| Поиск FTS5 | 1–24 мс на 90–102 тыс. писем; находит письма 2011 года по теме и отправителю без тела |

Урок окружения разработки: процессы, запущенные из инструментов Claude в этой сессии, видят `%LOCALAPPDATA%` через виртуализацию пакетного приложения (запись уходит в `AppData\Local\Packages\Claude_...\LocalCache`), а терминал пользователя пишет в настоящий каталог. Для тестов из инструментов указывать `--db` внутри проекта.

## 8c. Итерация 2 (17.10.2026): вложения, все папки, беседы, поиск как в OWA

Четыре части, все в коде и проверены на реальном ящике одной сессией. Схема базы — версия 3 (миграция с версии 2 на 102 тыс. писем около 10 с, включая пересборку индекса).

**Вложения с поиском по содержимому.**
- Транспорт: `MailTransport::get_attachment(attachment_id, max_bytes) -> AttachmentContent { bytes, content_type, file_name }`. В OWA это `GET /owa/service.svc/s/GetFileAttachment?id=…&X-OWA-CANARY=…` (форма снята с живого сервера, см. `protocol-owa-json.md` §5); запасной путь — операция `GetAttachment` с base64, выбирается автоматически при отказе GET и запоминается на сессию. Лимит размера проверяется по `Content-Length` и по мере чтения (`TransportError::TooLarge`).
- Новый крейт `aiowa-extract`: чистые функции «байты → текст» для PDF (`pdf-extract`, без OCR), DOCX (`zip` + `quick-xml`, текст `w:t` с разделителями абзацев и ячеек), XLSX/XLS/ODS (`calamine`, лист за листом, ячейки через пробел), TXT/CSV (BOM, UTF-8, иначе windows-1251). Остальные типы — `Unsupported`, паника библиотек разбора превращается в `Failed`.
- Схема: у `attachments` колонки `local_path`, `sha256`, `text`, `text_state` (0 ожидает, 1 текст есть, 2 тип не поддерживается, 3 ошибка извлечения, 4 ошибка скачивания), `fetched_at`, `last_access_at`; уникальный индекс `(item_pk, server_id)`, поэтому повторная запись письма не теряет скачанное. Файлы лежат в `attachments/` рядом с базой как `<attachment_pk>-<имя>`; `evict_attachment_cache(квота)` удаляет самые давние по `last_access_at`, текст и хэш остаются.
- Индекс: представление `items_fts_src` получило колонку `attachment_text` (имена и тексты не инлайновых вложений письма через перевод строки), `items_fts` пересобран с шестью колонками, веса `bm25(8, 4, 2, 1, 1, 0.5)`. Совпадение в тексте вложения определяется по сниппету колонки 5: если подсветка есть только там, `search` показывает сниппет из вложения и имя файла (по подсвеченным словам в текстах вложений письма).
- Политика в `aiowa-sync`: отдельная фаза после тел, по папкам в том же порядке; кандидаты — не инлайновые вложения писем в окне `--days`, не скачанные, размером до `--max-attachment-mb` (25 по умолчанию); по одному запросу, извлечение текста в `spawn_blocking`; неустранимый отказ по одному вложению помечается (`text_state = 4`) и не останавливает остальные, повторно такие не запрашиваются. После фазы — вытеснение кэша до `--cache-mb` (2048 по умолчанию).

**Все почтовые папки.** `SyncOptions.folders: FolderSelection::{AllMail, Named(Vec<String>)}`; `--folders all` по умолчанию, явный список принимает известные имена (`inbox`) и отображаемые («Проекты»). Правило `Store::folders_for_sync`: класс `IPF.Note` или `IPF.Note.*`, не удалённые, кроме служебных по `DistinguishedFolderId` (`junkemail`, `deleteditems`, `drafts`, `outbox`, `conversationhistory`, `syncissues` и её `conflicts`/`localfailures`/`serverfailures`) и всех их подпапок по цепочке `parent_server_id`; порядок: `inbox`, `sentitems`, остальные по убыванию `total_count`. Сервер сообщает `DistinguishedFolderId` для всех служебных папок этого ящика, поэтому сопоставление по именам не понадобилось.

**Беседы.** Таблицы `conversations` (`server_id` = `ConversationId`, `topic` без префиксов `RE:/FW:/Отв:` через `aiowa_topic()`, `item_count`, `unread_count`, `last_received_at`, `has_attachments`) и `conversation_folders` (ключ `(conversation_pk, folder_pk)`, индекс `(folder_pk, last_received_at DESC)`); у `items` колонка `conversation_pk`. Письмо без `ConversationId` образует беседу `item:<ItemId>`. Агрегаты не ведутся дельтами, а пересчитываются из `items` для затронутых пар (беседа, папка) в конце той же транзакции, что применила пачку: это идемпотентно и переживает перемещения (усыновление строки добавляет в набор старую папку). Миграция заполняет таблицы по уже загруженным письмам. `aiowa list --folder inbox --limit 30`: тема, участники (отправители свежие первыми, без повторов), писем в папке/во всей беседе, непрочитанных, дата последнего, признак вложений; 1 мс на 90 тыс. писем.

**Поиск как в OWA.** Разбор запроса в `aiowa-store/src/query.rs`: операторы `от:/from:`, `кому:/to:`, `тема:/subject:`, `вложение:/has:attachment`, `папка:/in:/folder:`, `после:/after:` и `до:/before:` (даты `YYYY-MM-DD`, `DD.MM.YYYY`, `YYYY-MM`, `YYYY`, по системному часовому поясу, `до:` включительно), значения в кавычках. Текстовые условия становятся выражением FTS5 (`"слово"*`, фраза `"a" + "b"`, фильтры по колонкам `{from_name from_email}: (…)`, `recipients: (…)`, `subject: (…)`, части через явный `AND`), остальные — условиями SQL по `items`/`folders`; запрос из одних фильтров работает как список по дате. Нормализация ё→е: скалярная функция `fts_norm()` из Rust регистрируется на каждом соединении до миграций и применяется в представлении и к запросу; регистр не трогается (его сворачивает токенизатор), чтобы сниппеты читались как оригинал. Рядом `norm_lower()` для сравнений имён папок.

**Цифры на реальном ящике (17.10.2026, одна сессия, `sync --days 30`, все папки).**

| Что | Результат |
|---|---|
| Папки к синхронизации по правилу | 8 из 57 в иерархии: входящие, отправленные, `_СЭД_` (50 135), «Сводки ЦДС» (2 132), четыре мелкие; служебные и не почтовые исключены по `DistinguishedFolderId` и классу |
| Метаданные | 64 490 новых писем за 132 пачки (остаток отправленных и вся `_СЭД_`), ошибок 0; ~3 с на пачку 512 с пересчётом бесед |
| Тела за 30 дней | 662 письма (475 входящих, 157 отправленных, 29 сводок, 1 СЭД), ошибок 0 |
| Вложения за 30 дней | 436 файлов, 349 МБ, 0,4–1,9 с на файл; с текстом 347, тип не поддерживается 83 (картинки, архивы, `.doc`), ошибка извлечения 6 (PDF без текстового слоя или битые) |
| Текст из вложений | PDF 202 из 208 (430 тыс. символов), XLSX 119 из 119 (8,2 млн символов: таблицы большие), DOCX 14 из 14 |
| Всего | 1 561,6 с; база 377 МБ на 154 467 писем, 111 145 бесед, 689 тел, кэш вложений 352 МБ |
| Миграция 2 → 3 на базе из 102 тыс. писем | около 10 с (беседы и агрегаты по папкам, пересборка индекса с колонкой вложений) |
| Поиск | «хомут выпуска» 18 мс (хит из PDF показан с именем файла), «лисхор» 11 мс (слово из XLSX), с операторами `вложение: после: папка:` 96 мс, по отправителю 11 мс; `list --folder inbox --limit 30` 1 мс |

Замечания по итогам: библиотека `pdf-extract` пишет в журнал предупреждения о глифах и инлайновых картинках на каждый PDF, в CLI они приглушены фильтром `pdf_extract=error,lopdf=error`; сервер отдаёт имя файла в `Content-Disposition: attachment; filename=%D0%A1...` (процентами, без `filename*=`), клиент его декодирует, но для кэша берёт имя из метаданных письма.

**Что отложено.** OCR сканов; `prefix=` в FTS; параллельная загрузка вложений (сейчас по одному, 0,5–2 с на файл); повтор вложений с `text_state = 4` по флагу; полное тело с цитатами; `STRICT`-таблицы и прочее из 8a.

## 9. Открытые вопросы

- Хранить ли полное тело с цитатами сразу или только UniqueBody (экономия 3–5×, но «показать письмо целиком» требует догрузки с сервера).
- Нужна ли таблица `conversations` на этапе 1 или достаточно группировки по `conversation_id` при чтении.
- SQLCipher: подключать на этапе 1 или после обкатки (концепт рекомендует проверить совместимость с FTS5 в спайке).
