# Этап 1: разбор проекта (2026-10-16)

Сводка трёх разборов (данные и SQLite; корректность синхронизации; архитектура Rust) проекта `stage1-core-design.md`, сверенная с `protocol-owa-json.md` (включая §5b' с уже снятыми формами `SyncFolderItems` и `GetItem`). Рамка по решению автора: ИИ-функции за скобками, цель — полный OWA-клиент. Поэтому схема закладывается сразу под чтение и запись (флаги, категории, черновики, очередь операций), а код этапа 1 реализует только чтение. `index.db` для векторов из §5 откладывается без срока; `unique_text` остаётся — он нужен FTS и сниппетам.

## 1. Вердикт

Код писать можно после правки документа: день работы над текстом плюс одна проба по разделу 4. Эксперты разошлись: архитектор считает каркас готовым («блокеров нет, шесть дешёвых правок»), эксперты по данным и синхронизации называют по одному–два блокера. Принимаю их позицию: три решения ломают замысел и определяют API между крейтами, то есть не правятся «потом»: contentless-FTS (нельзя удалять и показывать сниппеты), незафиксированная атомарность `SyncState` с батчем (потерянные `Delete` не вернутся никогда) и контракт трейта, который отдаёт только id и не выражает частичных результатов. Четвёртый добавляю сам: смена `ItemId` при перемещении определяет, будет ли API хранилища «upsert по server_id» или «усыновление» строки.

Остальные правки (полный ресинк, защищённый разбор `Changes`, индексы FK, граница async/sync, logoff по Ctrl+C) — major, они описаны в разделе 3 и вносятся в документ до первой строки кода, но архитектуру не меняют. Что верно и остаётся — в разделе 5.

## 2. Блокеры

**Б1. `items_fts` объявлена как `content=''` (contentless), а не как внешнее содержимое.** Для такой таблицы FTS5 запрещает `DELETE`/`UPDATE`, не умеет `rebuild`, а чтение любой колонки даёт NULL — `snippet()`/`highlight()`, обещанные в §7, вернут пустоту. Оба эксперта (данные, синхронизация) сходятся на двух вариантах: `contentless_delete=1` (SQLite ≥ 3.43, без `rebuild`, сниппеты вручную) или внешнее содержимое по представлению. Выбираю представление `items_fts_src` (SQL в 3.5): тексты не дублируются, сниппеты и `rebuild` работают, удаление — обычный `DELETE FROM items_fts WHERE rowid=?` до изменения строки-источника.

**Б2. Не сказано, что `folders.sync_state` пишется в той же транзакции, что и изменения батча.** Если состояние уходит вперёд без изменений — ICS больше не пришлёт эти `Delete`/`Update`/`ReadFlagChange`; если отстаёт — батч придёт повторно, что безопасно только при полной идемпотентности. Правка: API хранилища `apply_item_batch(folder_pk, &[ItemChange], new_state, gen)` и `apply_hierarchy_batch(changes, new_state)` — одна SQLite-транзакция на вызов, состояние берётся из каждого ответа, включая пустые. Ответ, не дошедший до коммита, ничего не меняет; это же делает Ctrl+C «после текущего батча» корректным по построению.

**Б3. Контракт трейта: `ItemChanges` отдаёт только id, `get_items` — `Vec<MessageFull>`.** §6 рассчитывает на метаданные и даты «из той же выдачи», а трейт их вернуть не может; без дат окно `--days` невычислимо, отдельная фаза метаданных стоит ~1 544 вызова `GetItem` на входящие. Проба §5b' уже сняла вопрос: `AdditionalProperties` в `SyncFolderItems` работают. `GetItem` отвечает по одному `ResponseMessage` на каждый id, и одно письмо, удалённое между sync и `GetItem`, через `Vec<MessageFull>` либо роняет батч из 50, либо теряет соответствие id↔item. Правка — в 3.3.

**Б4. Идентичность письма = `ItemId`, который Exchange меняет при перемещении.** `SyncFolderItems` источника присылает `Delete`, приёмника — `Create` с новым id и тем же `InternetMessageId`. Без правила тело выкачивается заново, FTS перестраивается, старая строка остаётся надгробием. Эксперты расходятся: данные предлагают «усыновление» (перенести `server_id`/`folder_pk` в старую строку, сохранив `item_pk`), синхронизация — копировать `bodies`/`attachments` на новый `item_pk`, потому что порядок событий между папками не гарантирован (Create в приёмнике может прийти раньше Delete в источнике). Выбираю гибрид: при `Create` искать совпадение по `internet_message_id + size + received_at` в другой папке; найденная строка уже `deleted=1` — усыновить (сохраняются `item_pk` и rowid FTS); жива — скопировать тело и признаки на новую строку, а `Delete`, когда придёт, пометит старую. Тело не перекачивается ни в одном случае; `size + received_at` не даёт склеить письмо самому себе (Inbox и SentItems).

## 3. Правки по разделам

### 3.1. Воркспейс и крейты

Разбиение и направление зависимостей остаются. Уточнения:

- `aiowa-core` и `aiowa-transport` без serde и reqwest. Все wire-структуры OWA (`__type`, PascalCase, `:#Exchange`) — приватный модуль `wire` в `aiowa-transport-owa` с `From`/`TryFrom` в домен и тестом на каждую конвертацию. Иначе формат OWA протечёт в store, sync, будущий Tauri и резервный EWS-транспорт (протокол §3.4 даёт `EwsUrl`).
- `aiowa-store` — синхронный крейт без tokio. `rusqlite::Transaction` держит `&Connection` и потому `!Send`; под `#[async_trait]` транзакция через `.await` не скомпилируется. Правило: «ни один `Transaction` не живёт через `.await`». В `aiowa-sync` один writer-поток (`std::thread` + `mpsc`/`oneshot`) либо `spawn_blocking`; чтение для `search`/`stats` — отдельное read-only соединение (WAL разрешает).
- `[workspace.dependencies]` с версиями, проверенными архитектором по crates.io на 16.10.2026: reqwest 0.13.5 (`default-features = false`), tokio 1.53.3, serde 1.0.229, serde_json 1.0.154 (`preserve_order`), jiff 0.2.38, rusqlite 0.40.2 (`bundled`), async-trait 0.1.92, keyring 4.2.0, secrecy 0.10.3, zstd 0.14.3, clap 4.6.7, thiserror 2, tokio-util, axum 0.8.9 только в dev-dependencies; `edition = "2024"`.
- Добавить раздел о тестах: unit без сети и tokio (golden-JSON запросов, фикстуры → DTO → домен, чистая `classify_response(status, location, body)`, страж «`__type` первым ключом»); integration на FakeExchange (axum на `127.0.0.1:0` с переключателями «сессия истекла», «ServerBusy», «SyncState невалиден» и счётчиком входов); `FakeTransport` в памяти для `aiowa-sync`; e2e только под `#[ignore]` с `shutdown()` в guard.

### 3.2. Типы

- `AccountId` остаётся идентификатором аккаунта (= каталог и файл базы), но не колонкой таблиц (см. 3.5).
- `MessageMeta` расширить полями, которые дорого дозапрашивать: `is_draft`, `flag {status, start, due, complete}`, `categories`, `last_verb`, `icon_index`, `sensitivity`, `display_to`, `display_cc`, `in_reply_to`, `references`, `conversation_index`, `created_at`, `server_modified_at`. Повторный проход по 1 млн писем ради одного поля стоит часы.
- Время: в домене `jiff::Timestamp`, в базе миллисекунды UTC; `sort_ts = coalesce(received_at, sent_at, created_at)` считается в коде (у черновиков нет `DateTimeReceived`).
- Определить типы, которых в §2–3 нет: `MessageFull { meta, to, cc, body: Option<MessageBody>, attachments }`, `ItemShape { MetaOnly | WithUniqueBody { max_body_bytes } | WithFullBody { max_body_bytes } }`, `FolderChanges { state, created: Vec<Folder>, updated, deleted: Vec<FolderId>, has_more }`, `FolderCounts`, `Capabilities { max_get_item_ids, max_sync_changes, supports_unique_body }`, `SessionInfo { default_folder_ids, time_zone, user_culture, mailbox_guid, categories }`.
- `BodyType` на проводе — `"HTML"`, не `"Html"`.

### 3.3. Trait транспорта

Эксперты по синхронизации и архитектуре предложили разные формы `ItemChanges`: упорядоченный `Vec<ItemChange>` против четырёх векторов с `meta: Option`. Беру упорядоченный вариант — порядок внутри батча сохраняется, а метаданные не опциональны, раз проба подтвердила `AdditionalProperties`:

```rust
pub enum ItemChange {
    Create(MessageMeta), Update(MessageMeta), Delete(ItemId),
    ReadFlag { id: ItemId, is_read: bool },
    Unparsed { id: Option<ItemId>, raw: String },   // неизвестный __type; строка-заглушка, состояние продвигается
}
pub struct ItemChanges { pub state: SyncState, pub changes: Vec<ItemChange>, pub has_more: bool }

pub enum ItemResult { Ok(MessageFull), Failed { id: ItemId, code: ResponseCode, message: String } }
async fn get_items(&self, ids: &[ItemId], shape: ItemShape) -> Result<Vec<ItemResult>, TransportError>; // len == ids.len(), порядок = ids
fn session_info(&self) -> &SessionInfo;
async fn shutdown(&self) -> Result<(), TransportError>;   // OWA: GET /owa/logoff.owa, таймаут 5 с
```

`Err(TransportError)` — только отказ всего вызова. `ErrorItemNotFound` в `Failed` движок трактует как удаление, не как ошибку тела.

`TransportError` без reqwest-типов и без `Cancelled` (отмена — `CancellationToken` в sync; здесь позиции архитектора и эксперта по синхронизации расходятся, беру архитектора): `SessionExpired`, `CsrfRejected`, `SessionLimit`, `Throttled { retry_after: Option<Duration> }`, `SyncStateInvalid { folder }`, `Network { kind: Timeout|Connect|Tls|Io|Other, source: Box<dyn Error + Send + Sync> }`, `Protocol { op, status, snippet ≤ 2 КБ без тел писем }`, `Server { code: ResponseCode, message }`. `ResponseCode` — newtype над `String` с константами, не enum (иначе теряется текст неизвестного кода). Предикаты: `is_retryable`, `needs_reauth`, `needs_full_resync`, `retry_after`.

`CredentialProvider`: синхронный `fn password(&self, host, user) -> Result<SecretString, _>` — типа `Secret<String>` в secrecy 0.10 нет.

### 3.4. Реализация OWA

- **Повторный вход с поколением.** `auth: Mutex<Auth { generation, canary }>`; `call` снимает `(gen, canary)`, при `SessionExpired` зовёт `reauth(gen)`, который входит только если `generation` не изменился, и перечитывает `X-OWA-CANARY` из `Arc<Jar>` (после входа канарейка новая; старая даст ложный `CsrfRejected`). Вход и logoff — мимо семафора, иначе дедлок при двух занятых permit'ах. Один прозрачный повтор того же запроса. Политика в sync: `SessionExpired` после неудачного повторного входа → при `logon.aspx?reason≠0` стоп с кодом 2, иначе ещё одна попытка через 60 с и стоп с сохранённым состоянием (компромисс между «одна попытка» архитектора и «повторять батч» эксперта по синхронизации).
- **Конверт как тип.** `trait OwaOp { NAME, SERVER_VERSION, Body, Response }`; `Envelope` и `RequestHeader` с `rename_all = "PascalCase"` (сейчас `header`/`body` ушли бы в нижнем регистре). Полиморфные позиции — internally tagged enum по `__type`; `Option` в запросах — `skip_serializing_if`, кроме доказанных спайком `null`. Без `extra: Map` через `flatten` (удваивает пик памяти): для исследования — `serde_ignored` под флагом `trace_protocol`; `default` на уровне полей, обязательные (`ItemId.Id`, `SyncState`, `ResponseClass`) без default.
- **Разбор `Changes.Changes[]` поэлементно**: `Value` → типизированно по `__type`/`ChangeType`; неудача → `Unparsed`. Иначе один `MeetingRequest`/`Contact` заклинит папку навсегда.
- **reqwest 0.13**: `default-features = false`, фичи `native-tls, cookies, json, gzip, charset, system-proxy`, без `http2` (h2 поверх CBC-шифра запрещён RFC 7540), `.cookie_provider(jar)`, `.redirect(Policy::none())`, `.http1_only()`, `connect_timeout` 10 с; таймаут на операцию: 30 с `GetFolder`/`sessiondata`, 60 с `Sync*`/`FindConversation` (наблюдались 49,6 с), 120 с `GetItem` с телами (эксперты дали 60 и 120 для `Sync*`; беру 60 — состояние после таймаута не теряется).
- **Шаг `sessiondata.ashx`** после входа: `DefaultFolderIds`, часовой пояс, `MasterCategoryList`, квоты. `FolderRef::Distinguished` резолвится локально, на сервер уходит всегда `FolderId`.
- **`TimeZoneContext = "UTC"`** во всех запросах почты, даты парсятся с `Z`; пояс пользователя из sessiondata — в `accounts.time_zone` для показа и календаря. Так решается разногласие «UTC» против «пояс из профиля».
- `Throttled`: `BackOffMilliseconds` лениво из `MessageXml: Option<Value>`, HTTP 503/429 с `Retry-After`; `correlation_id` в `tracing::span` и в `Protocol`. Ручной `Debug` для `OwaSession` без cookies, канарейки, пароля.

### 3.5. Схема базы

Решения: одна база на аккаунт (путь уже такой; `account_id` из дочерних таблиц убран, `UNIQUE(server_id)` простой; единый ящик по нескольким аккаунтам в UI — k-way merge по `sort_ts`/score); `STRICT`-таблицы; `conversations` сейчас, а не позже (миграция по 1 млн строк); FTS по представлению с единой функцией нормализации `fts_norm()` (HTML→текст уже сделан при записи, функция делает ё→е и схлопывание пробелов; регистрируется на каждом соединении, её версия в `settings('fts_norm_version')`, смена = `rebuild`); `group_concat(... ORDER BY)` требует SQLite ≥ 3.44 — проверять `sqlite_version()` при старте. Тела: `codec`/`dict_id` для словаря zstd, `page_size = 8192`. Решение по 9.1: этап 1 хранит UniqueBody; полное тело догружается при открытии и кэшируется (бит 2 в `body_state`).

```sql
-- Бутстрап новой базы, до первой таблицы, вне транзакции:
PRAGMA page_size = 8192; PRAGMA auto_vacuum = INCREMENTAL; PRAGMA journal_mode = WAL;
-- При каждом открытии (писатель — одно соединение; читатели — пул):
PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;
PRAGMA journal_size_limit = 67108864; PRAGMA wal_autocheckpoint = 1000;
PRAGMA cache_size = -65536; PRAGMA temp_store = MEMORY;
-- Пишущие транзакции: BEGIN IMMEDIATE; после батча PRAGMA wal_checkpoint(PASSIVE);
-- TRUNCATE только по завершении sync и без активных читателей.

CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL, app_version TEXT NOT NULL) STRICT;
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT) STRICT, WITHOUT ROWID;
CREATE TABLE sync_state (            -- серверные курсоры: 'hierarchy:msgfolderroot', 'hierarchy:archivemsgfolderroot', 'subscriptions'
  key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER NOT NULL) STRICT, WITHOUT ROWID;

CREATE TABLE accounts (              -- одна строка: база = аккаунт
  account_id INTEGER PRIMARY KEY CHECK (account_id = 1),
  host TEXT NOT NULL, login TEXT NOT NULL, email TEXT, display_name TEXT, mailbox_guid TEXT,
  time_zone TEXT, server_build TEXT, quota_used INTEGER, quota_warn INTEGER,
  created_at INTEGER NOT NULL, last_login_at INTEGER) STRICT;

CREATE TABLE folders (
  folder_pk        INTEGER PRIMARY KEY,
  server_id        TEXT NOT NULL UNIQUE,          -- FolderId.Id
  change_key       TEXT,
  parent_server_id TEXT,                          -- сырое ParentFolderId.Id для сопоставления
  parent_pk        INTEGER REFERENCES folders(folder_pk),
  root             TEXT NOT NULL DEFAULT 'msgfolderroot',
  distinguished    TEXT,                          -- из sessiondata.DefaultFolderIds
  display_name     TEXT NOT NULL,
  folder_class     TEXT,
  total_count      INTEGER NOT NULL DEFAULT 0,
  unread_count     INTEGER NOT NULL DEFAULT 0,
  child_count      INTEGER NOT NULL DEFAULT 0,
  effective_rights INTEGER,                       -- 1 CreateContents, 2 Delete, 4 Modify, 8 Read, 16 CreateHierarchy
  is_hidden        INTEGER NOT NULL DEFAULT 0,    -- PR_ATTR_HIDDEN
  sync_state       TEXT,                          -- SyncFolderItems этой папки
  sync_priority    INTEGER NOT NULL DEFAULT 100,
  full_sync_gen    INTEGER NOT NULL DEFAULT 0,    -- поколение полной выгрузки элементов
  seen_gen         INTEGER NOT NULL DEFAULT 0,    -- поколение полной выгрузки иерархии
  last_synced_at   INTEGER,
  local_dirty      INTEGER NOT NULL DEFAULT 0,
  deleted          INTEGER NOT NULL DEFAULT 0,
  deleted_at       INTEGER) STRICT;
CREATE INDEX folders_parent ON folders(parent_pk);

CREATE TABLE conversations (
  conversation_pk  INTEGER PRIMARY KEY,
  server_id        TEXT NOT NULL UNIQUE,          -- ConversationId.Id
  topic            TEXT,
  last_received_at INTEGER,
  item_count       INTEGER NOT NULL DEFAULT 0,
  unread_count     INTEGER NOT NULL DEFAULT 0,
  has_attachments  INTEGER NOT NULL DEFAULT 0,
  flag_status      INTEGER NOT NULL DEFAULT 0,
  sync_state       TEXT) STRICT;                  -- SyncState GetConversationItems

CREATE TABLE items (
  item_pk             INTEGER PRIMARY KEY,
  server_id           TEXT NOT NULL UNIQUE,       -- ItemId.Id
  change_key          TEXT,
  folder_pk           INTEGER NOT NULL REFERENCES folders(folder_pk),
  conversation_pk     INTEGER REFERENCES conversations(conversation_pk),
  conversation_index  BLOB,
  internet_message_id TEXT,
  parent_internet_message_id TEXT,
  in_reply_to         TEXT,
  references_hdr      TEXT,
  item_class          TEXT,
  subject             TEXT,
  from_name           TEXT,
  from_email          TEXT,
  sender_email        TEXT,
  display_to          TEXT,
  display_cc          TEXT,
  received_at         INTEGER,
  sent_at             INTEGER,
  created_at          INTEGER,
  sort_ts             INTEGER NOT NULL,           -- coalesce(received_at, sent_at, created_at)
  server_modified_at  INTEGER,                    -- LastModifiedTime
  is_read             INTEGER NOT NULL DEFAULT 0,
  is_draft            INTEGER NOT NULL DEFAULT 0,
  has_attachments     INTEGER NOT NULL DEFAULT 0,
  importance          TEXT,
  sensitivity         TEXT,
  icon_index          TEXT,
  flag_status         INTEGER NOT NULL DEFAULT 0, -- 0 NotFlagged, 1 Flagged, 2 Complete
  flag_start_at       INTEGER,
  flag_due_at         INTEGER,
  flag_complete_at    INTEGER,
  last_verb           INTEGER,                    -- 102 replied, 103 reply-all, 104 forwarded
  last_verb_at        INTEGER,
  size                INTEGER,
  preview             TEXT,
  meta_state          INTEGER NOT NULL DEFAULT 0, -- 0 только id, 1 метаданные есть, 2 запись не разобрана
  body_state          INTEGER NOT NULL DEFAULT 0, -- маска: 1 unique body, 2 full body, 4 ошибка последней загрузки
  body_attempts       INTEGER NOT NULL DEFAULT 0,
  fts_indexed         INTEGER NOT NULL DEFAULT 0, -- DELETE из items_fts делать только при 1
  seen_gen            INTEGER NOT NULL DEFAULT 0,
  local_dirty         INTEGER NOT NULL DEFAULT 0, -- маска: 1 is_read, 2 flag, 4 folder, 8 categories
  deleted             INTEGER NOT NULL DEFAULT 0,
  deleted_at          INTEGER,
  updated_at          INTEGER NOT NULL) STRICT;
CREATE INDEX items_folder_list   ON items(folder_pk, sort_ts DESC, item_pk DESC) WHERE deleted = 0;
CREATE INDEX items_folder_unread ON items(folder_pk, sort_ts DESC) WHERE deleted = 0 AND is_read = 0;
CREATE INDEX items_flagged       ON items(sort_ts DESC) WHERE deleted = 0 AND flag_status = 1;
CREATE INDEX items_conv          ON items(conversation_pk, sort_ts DESC);
CREATE INDEX items_internet_id   ON items(internet_message_id) WHERE internet_message_id IS NOT NULL;
CREATE INDEX items_meta_queue    ON items(sort_ts DESC) WHERE meta_state = 0 AND deleted = 0;
CREATE INDEX items_body_queue    ON items(sort_ts DESC) WHERE body_state = 0 AND deleted = 0;
CREATE INDEX items_body_retry    ON items(sort_ts DESC) WHERE (body_state & 4) <> 0 AND deleted = 0 AND body_attempts < 5;
CREATE INDEX items_tombstones    ON items(deleted_at) WHERE deleted = 1;

CREATE TABLE recipients (
  item_pk  INTEGER NOT NULL REFERENCES items(item_pk) ON DELETE CASCADE,
  kind     INTEGER NOT NULL,                      -- 1 to, 2 cc, 3 bcc, 4 reply-to
  position INTEGER NOT NULL,
  name     TEXT,
  email    TEXT COLLATE NOCASE,
  PRIMARY KEY (item_pk, kind, position)) STRICT, WITHOUT ROWID;  -- кластер по письму, индекс по item_pk не нужен
CREATE INDEX recipients_email ON recipients(email);

CREATE TABLE zstd_dicts (dict_id INTEGER PRIMARY KEY, created_at INTEGER NOT NULL, dict BLOB NOT NULL) STRICT;
CREATE TABLE bodies (
  item_pk      INTEGER PRIMARY KEY REFERENCES items(item_pk) ON DELETE CASCADE,
  body_type    TEXT NOT NULL,                     -- HTML | Text
  codec        INTEGER NOT NULL DEFAULT 1,        -- 0 raw, 1 zstd, 2 zstd+dict
  dict_id      INTEGER REFERENCES zstd_dicts(dict_id),
  unique_body  BLOB,
  full_body    BLOB,
  unique_text  TEXT,                              -- HTML→текст, открытым текстом: читается представлением для FTS и snippet()
  text_len     INTEGER,
  is_truncated INTEGER NOT NULL DEFAULT 0,
  fetched_at   INTEGER NOT NULL) STRICT;

CREATE TABLE attachments (
  attachment_pk  INTEGER PRIMARY KEY,
  item_pk        INTEGER NOT NULL REFERENCES items(item_pk) ON DELETE CASCADE,
  server_id      TEXT NOT NULL,
  name TEXT, content_type TEXT, size INTEGER,
  is_inline      INTEGER NOT NULL DEFAULT 0,
  content_id     TEXT,
  local_path     TEXT,                            -- %LOCALAPPDATA%\AIOWA\<account>\attachments\<attachment_pk>, не BLOB
  sha256         BLOB,
  fetched_at     INTEGER,
  last_access_at INTEGER) STRICT;                 -- LRU-вытеснение по квоте
CREATE INDEX attachments_item   ON attachments(item_pk);
CREATE INDEX attachments_cached ON attachments(last_access_at) WHERE local_path IS NOT NULL;

CREATE TABLE categories (                         -- MasterCategoryList из sessiondata
  name TEXT PRIMARY KEY COLLATE NOCASE, color INTEGER, guid TEXT) STRICT, WITHOUT ROWID;
CREATE TABLE item_categories (
  item_pk  INTEGER NOT NULL REFERENCES items(item_pk) ON DELETE CASCADE,
  category TEXT NOT NULL COLLATE NOCASE,
  PRIMARY KEY (item_pk, category)) STRICT, WITHOUT ROWID;
CREATE INDEX item_categories_cat ON item_categories(category);

CREATE TABLE conversation_folders (               -- беседа видна в папке, если в ней есть хоть одно её письмо
  conversation_pk  INTEGER NOT NULL REFERENCES conversations(conversation_pk) ON DELETE CASCADE,
  folder_pk        INTEGER NOT NULL REFERENCES folders(folder_pk) ON DELETE CASCADE,
  last_received_at INTEGER NOT NULL,
  item_count       INTEGER NOT NULL,
  unread_count     INTEGER NOT NULL,
  PRIMARY KEY (folder_pk, last_received_at DESC, conversation_pk)) STRICT, WITHOUT ROWID;
CREATE INDEX conversation_folders_conv ON conversation_folders(conversation_pk);

CREATE TABLE address_book (                       -- автодополнение; наполняется из from/recipients при синхронизации
  email TEXT PRIMARY KEY COLLATE NOCASE, name TEXT,
  weight INTEGER NOT NULL DEFAULT 0, last_used_at INTEGER) STRICT, WITHOUT ROWID;

CREATE TABLE pending_ops (                        -- очередь локальных операций (этап записи; таблица — сейчас)
  op_id      INTEGER PRIMARY KEY,
  kind       TEXT NOT NULL,                       -- mark_read, flag, move, delete, categorize, save_draft, send, create_folder, rename_folder
  item_pk    INTEGER REFERENCES items(item_pk),
  folder_pk  INTEGER REFERENCES folders(folder_pk),
  payload    TEXT NOT NULL,                       -- JSON
  created_at INTEGER NOT NULL,
  state      INTEGER NOT NULL DEFAULT 0,          -- 0 queued, 1 in_flight, 2 failed, 3 done
  attempts   INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  depends_on INTEGER REFERENCES pending_ops(op_id)) STRICT;
CREATE INDEX pending_ops_state ON pending_ops(state, op_id);
CREATE INDEX pending_ops_item  ON pending_ops(item_pk) WHERE state < 3;

CREATE TABLE drafts (
  draft_pk INTEGER PRIMARY KEY,
  item_pk INTEGER REFERENCES items(item_pk),      -- серверная копия в Drafts, NULL пока не сохранена
  mode INTEGER NOT NULL,                          -- 0 new, 1 reply, 2 reply_all, 3 forward
  source_item_pk INTEGER REFERENCES items(item_pk),
  to_json TEXT, cc_json TEXT, bcc_json TEXT, subject TEXT, body_html TEXT,
  saved_at INTEGER NOT NULL, server_change_key TEXT, dirty INTEGER NOT NULL DEFAULT 1) STRICT;
CREATE TABLE draft_attachments (
  draft_pk INTEGER NOT NULL REFERENCES drafts(draft_pk) ON DELETE CASCADE,
  name TEXT NOT NULL, local_path TEXT, content_type TEXT, size INTEGER, server_attachment_id TEXT,
  state INTEGER NOT NULL DEFAULT 0,               -- 0 local, 1 uploaded, 2 error
  PRIMARY KEY (draft_pk, name)) STRICT, WITHOUT ROWID;

-- Полнотекстовый индекс: внешнее содержимое по представлению. fts_norm() — скалярная функция из Rust
-- (deterministic), регистрируется на каждом соединении, включая read-only для search.
CREATE VIEW items_fts_src AS
SELECT i.item_pk,
       fts_norm(i.subject)   AS subject,
       fts_norm(i.from_name) AS from_name,
       i.from_email          AS from_email,
       (SELECT group_concat(coalesce(r.name,'') || ' ' || coalesce(r.email,''), ' ' ORDER BY r.kind, r.position)
          FROM recipients r WHERE r.item_pk = i.item_pk)                                   AS recipients,
       (SELECT group_concat(a.name, ' ' ORDER BY a.attachment_pk)
          FROM attachments a WHERE a.item_pk = i.item_pk AND a.is_inline = 0)              AS attachment_names,
       fts_norm(coalesce(b.unique_text, i.preview))                                        AS body_text
  FROM items i LEFT JOIN bodies b ON b.item_pk = i.item_pk;

CREATE VIRTUAL TABLE items_fts USING fts5(
  subject, from_name, from_email, recipients, attachment_names, body_text,
  content='items_fts_src', content_rowid='item_pk',
  tokenize='unicode61 remove_diacritics 2');   -- prefix='2 3' решить по замеру (+30–50 % индекса)

-- store::reindex(item_pk), в той же транзакции, что меняет items/bodies/recipients/attachments:
--   if fts_indexed: DELETE FROM items_fts WHERE rowid = ?1;      -- ДО изменения источников
--   ...изменения...
--   INSERT INTO items_fts(rowid, subject, from_name, from_email, recipients, attachment_names, body_text)
--   SELECT item_pk, subject, from_name, from_email, recipients, attachment_names, body_text
--     FROM items_fts_src WHERE item_pk = ?1;  UPDATE items SET fts_indexed = 1 WHERE item_pk = ?1;
-- Мягкое удаление: DELETE FROM items_fts WHERE rowid = ?1; UPDATE items SET deleted=1, deleted_at=?now, fts_indexed=0 WHERE item_pk=?1;
-- Чистка при простое, пачками: DELETE FROM items WHERE item_pk IN (SELECT item_pk FROM items
--   WHERE deleted = 1 AND deleted_at < ?week_ago LIMIT 500);  PRAGMA incremental_vacuum(4096);
-- Поиск: SELECT i.item_pk, bm25(items_fts, 8.0, 4.0, 4.0, 2.0, 2.0, 1.0) AS score, snippet(items_fts, 5, '<b>', '</b>', '…', 24)
--   FROM items_fts JOIN items i ON i.item_pk = items_fts.rowid WHERE items_fts MATCH ?1 AND i.deleted = 0 ORDER BY score LIMIT 20;
-- Список папки (keyset вместо OFFSET): WHERE folder_pk = ?1 AND deleted = 0 AND (sort_ts, item_pk) < (?2, ?3)
--   ORDER BY sort_ts DESC, item_pk DESC LIMIT 100;
-- Смена токенизатора: DROP TABLE items_fts; CREATE VIRTUAL TABLE ...; INSERT INTO items_fts(items_fts) VALUES('rebuild');
```

Миграции: `PRAGMA user_version` + `schema_migrations`, файлы `migrations/NNNN_*.sql` через `include_str!`, forward-only, в `BEGIN IMMEDIATE`; перед миграцией ≥ 2 — `VACUUM INTO 'mail.db.bak-N'`. После первичной загрузки — `('optimize')`. Построитель поисковых запросов: токены в кавычках, `*` у последнего, операторы `from:`/`to:`/`has:attachment` → фильтры по колонкам.

### 3.6. Синхронизация

Инварианты: `SyncState` и `ChangeKey` не зависят от сессии — повторный вход не требует ресинка; корень всегда `msgfolderroot`, в `SyncFolderItems` всегда `FolderId`; сервер прав для всех полей, кроме отмеченных битом `local_dirty`; применение батча идемпотентно (Create известного = Update, Update неизвестного = Create, Delete/ReadFlag неизвестного = no-op; тест «батч дважды → одинаковый дамп»).

0. **Старт.** Файловая блокировка `<account>\lock`. Вход → `sessiondata.ashx` → обновить `accounts`, `categories`, `folders.distinguished` по `DefaultFolderIds`. Два слота: A — только `Sync*`/`GetFolder` строго последовательно, B — `GetItem` тел (разделение по назначению от эксперта по синхронизации; семафор при этом живёт в транспорте, как настаивает архитектор). При `Throttled` останавливаются оба: ждать `retry_after`, иначе 5→10→20→40→60 с с джиттером ±20 %, суммарно до 10 мин на батч, затем стоп с сохранённым состоянием. `SessionLimit` при входе — ждать 5 мин, до 3 попыток.
1. **Иерархия.** `SyncFolderHierarchy` с `sync_state['hierarchy:msgfolderroot']`, `FolderShape`: Default + `ParentFolderId`, `FolderClass`, `TotalCount`, `UnreadCount`, `ChildFolderCount`, `EffectiveRights`, `PR_ATTR_HIDDEN`. Каждый ответ → `apply_hierarchy_batch` одной транзакцией: upsert по `server_id`, `parent_pk` вторым проходом, `Delete` → `deleted=1` папки, каскадно её подпапок и всех `items` (их `SyncFolderItems` больше не вызовется) с удалением из FTS; `Update` с новым `ParentFolderId` — обычный upsert (`FolderId` при перемещении папки стабилен). Цикл до `IncludesLastFolderInRange`. Фильтр: `folder_class LIKE 'IPF.Note%'`, не скрытая, не служебная; приоритеты как в проекте. Архивный ящик (`archivemsgfolderroot`) на этапе 1 не синхронизируется; ключ и колонка `root` заложены.
2. **Элементы.** Для каждой папки по приоритету `SyncFolderItems(FolderId, MaxChangesReturned=512, SyncScope=NormalItems)`, `ItemShape`: `IdOnly` + `AdditionalProperties` [`item:ItemClass`, `item:Subject`, `item:DateTimeReceived`, `item:DateTimeSent`, `item:DateTimeCreated`, `item:LastModifiedTime`, `item:Size`, `item:HasAttachments`, `item:Importance`, `item:Sensitivity`, `item:ConversationId`, `item:IsDraft`, `item:Preview`, `item:Flag`, `item:Categories`, `message:InternetMessageId`, `message:IsRead`, `message:From`, `message:Sender`, `item:DisplayTo`, `item:DisplayCc`] — либо `ShapeName: MailListItem` (28 ключей по §5b'), если проба покажет, что он принимается в `SyncFolderItems`. Каждый ответ разбирается поэлементно и применяется `apply_item_batch(folder_pk, changes, new_state, gen)` одной транзакцией: `Create`/`Update` → правило Б4, иначе upsert по `server_id`, `meta_state=1`, `sort_ts`, upsert `conversations`, `seen_gen=gen`, `reindex` (FTS получает тему, отправителя, превью — так ищется весь ящик, а не 5–10 % в окне); `Delete` → мягкое удаление + FTS; `ReadFlag` → `is_read`, если не поднят бит 1 `local_dirty`; затем `folders.sync_state`. Цикл `while has_more`, даже при пустом `Changes`; предохранитель — 20 пустых батчей подряд или трижды неизменный `SyncState`. Метаданные пишутся для всех элементов папки; окно `--days` относится только к телам.
3. **Тела** — параллельно, в слоте B, с первого же батча (выгрузка идёт от новых к старым, окно перечисляется первым). Кандидаты: `body_state=0 AND deleted=0 AND sort_ts >= окно ORDER BY sort_ts DESC`. Батч адаптивный: старт 20, при `ErrorTimeoutExpired`/`ErrorBatchProcessingStopped`/таймауте — пополам до 5, после 5 успехов — удвоение до 50; дополнительно Σ`size` ≤ 8 МБ (эксперты дали 8 и 16 МБ, беру меньшее до замера). `GetItem` с `ShapeName: ItemPart` (подтверждён §5b'), `MaximumBodySize` 512 КБ с `is_truncated`, ответ читать с лимитом 64 МБ. Результаты по позиции, сверяя `ItemId.Id`: `Ok` → `bodies` (zstd, `unique_text`), `recipients`, `attachments`, `item_categories`, флаги, `address_book`, `body_state |= 1`, `reindex`; `ErrorItemNotFound` → `deleted=1`; `ErrorBatchProcessingStopped`/`ErrorServerBusy`/`ErrorTimeoutExpired` → оставить 0; прочие → `body_state |= 4`, `body_attempts+1`, после 3 — только по `sync --retry-failed`. Тело письма при `Update` не сбрасывается (этап 1 — никогда; черновики — при смене `ChangeKey`).
4. **Счётчики.** `SyncFolderHierarchy` не сообщает об изменении `TotalCount`/`UnreadCount`: один батчевый `GetFolder` по всем синхронизируемым папкам в начале (знаменатель прогресса) и в конце запуска.
5. **Сбои состояния.** `SyncStateInvalid` → полный ресинк папки с поколением: `full_sync_gen+1`, `sync_state=NULL`, каждый `Create` ставит `seen_gen=gen`, по концу выгрузки одной транзакцией `UPDATE items SET deleted=1 WHERE folder_pk=? AND deleted=0 AND seen_gen<gen` (+FTS); тела не трогаются. `ErrorSyncFolderNotFound` → папка и её `items` `deleted=1`, `SyncFolderHierarchy` вне очереди. То же для иерархии с `folders.seen_gen`. Команда `sync --resync [folder]`.
6. **Завершение.** `CancellationToken` проверяется между батчами; запрос в полёте при отмене дропается, ничего из него не сохраняется; записи в SQLite всегда дожидаются (не под `select!`). Затем `transport.shutdown()` (logoff) на всех путях, включая ошибку; второй Ctrl+C — немедленный выход без logoff. Снять блокировку.

### 3.7. CLI

Команды проекта остаются, плюс `sync --resync [folder]`, `sync --retry-failed`, `--db PATH`, `--password-stdin` для FakeExchange и автоматизации (не аргумент и не env). Один экземпляр на базу (файловая блокировка). Коды выхода: 0 ок, 2 неверные учётные данные (`logon.aspx?reason≠0`), 3 сеть/TLS, 4 `SessionLimit`, 5 прервано пользователем (состояние сохранено). `--host/--user` с `env = AIOWA_HOST/AIOWA_USER`, `tracing-subscriber` с `EnvFilter`. `stats` показывает разбивку по таблицам (`dbstat`) и покрытие тел по папкам и датам. В лог никогда: пароль, cookies, канарейка, `SyncState` целиком, тела писем.

## 4. Что проверить в пробе до кода

Уже снято (§5b'): `AdditionalProperties` в `SyncFolderItems` работают, префикс `item:` необязателен; `GetItem` с `ShapeName: ItemPart` даёт тела, получателей, вложения, `Flag`, `IsDraft`, `LastModifiedTime`; `UniqueBody` доступен вне беседы. Осталось:

1. **Признак конца выдачи `SyncFolderItems`**: в ответе не было `IncludesLastItemInRange`, есть `TotalCount` неясной семантики. Снять ответы при `MaxChangesReturned=512` на входящих до конца: появляется ли `IncludesLastItemInRange: true` на последней странице (возможно, `false` просто не сериализуется), что означает `TotalCount`. Без этого цикл фазы 2 не написать.
2. `SyncFolderItems` с полным списком `AdditionalProperties` из 3.6: какие свойства приходят (особенно `item:Preview`, `item:Flag`, `item:Categories`, `message:InternetMessageId`, `item:DisplayTo`), время страницы 512; принимается ли `ShapeName: MailListItem`.
3. JSON-формы `Update`, `Delete`, `ReadFlagChange`: прочитать письмо в браузере, переместить, удалить, получить новое — затем инкрементальный вызов. `__type` элементов не-Message (`MeetingRequest`, `Contact`) в почтовых папках.
4. Перемещение Inbox → подпапка: `Delete` в источнике, `Create` в приёмнике с другим `ItemId.Id` и тем же `InternetMessageId`; порядок, в котором их видят два `SyncFolderItems`.
5. Полная выгрузка 77 177 элементов: время, длина `SyncState` в конце и время инкрементального вызова с ним; порядок `Create` (даты первого и последнего батча); пустой `Changes` до конца.
6. Испорченный символ в `SyncState` → форма `ErrorInvalidSyncStateData` (HTTP-статус, `ResponseClass`/`ResponseCode`/`MessageText`); валидность `SyncState` после logoff и через сутки.
7. `GetItem` батчами 10/20/50 с `ItemPart`: время и размер ответа; порядок `ResponseMessages` = порядок `ItemIds`; один битый id в батче — `ErrorItemNotFound` только для него или `ErrorBatchProcessingStopped`; принимает ли `ItemShape` `MaximumBodySize`/`FilterHtmlContent`; `UniqueBody` для SMIME; `Categories` и `LastVerbExecuted` в `ItemPart` или через `ExtendedProperty`.
8. Троттлинг: 6 параллельных `GetItem` — HTTP-код `ErrorServerBusy`, где лежит `BackOffMilliseconds`; 2 параллельных `SyncFolderItems` — ошибка `ConcurrentSyncCalls`.
9. `TimeZoneContext.Id = "UTC"` принимается; формат дат (`Z` или смещение); `DateTimeReceived` у черновика и в Sent Items.
10. `SyncFolderHierarchy`: `IncludesLastFolderInRange`; `FolderShape` с `AdditionalProperties` (`PR_ATTR_HIDDEN` 0x10F4, `EffectiveRights`); формы `Update` и `Delete`.
11. Длины `ItemId.Id` и `ConversationId.Id` из `fixtures/raw` (при 100–150 байтах это ~0,6 ГБ индексов на 1 млн писем; решить, хранить ли base64-декодированный BLOB или `server_id_hash`).
12. Сжатие zstd без словаря и со словарём на 2–5 тыс. реальных HTML-тел; `sqlite_version()` в rusqlite 0.40.2 `bundled` ≥ 3.44; простой сессии 60/120 мин (замер идёт).

## 5. Что оставить как есть

- Разбиение на крейты и направление зависимостей; tokio с первого дня и `#[async_trait]` (dyn-совместимость нужна Tauri и выбору транспорта в рантайме); `spike/owa-probe` как источник фикстур.
- reqwest + native-tls (единственный вариант под TLS 1.2 CBC сервера), `Policy::none()` (302 — сигнал состояния сессии), cookie jar в памяти, одна сессия на процесс, минимальные заголовки, JSON в теле без `EP=1` (совместимо с растущим `SyncState`), `__type` первым полем структур.
- Повторный вход только по 440/401/`logon.aspx`, никаких входов «на всякий случай», logoff при завершении, семафор на 2 запроса, backoff при `ErrorServerBusy`.
- Суррогатные ключи и rowid FTS = `item_pk`; `bodies` отдельно от `items` (список папки не читает BLOB-страницы); zstd для HTML и текстовая выжимка отдельно; FTS в том же файле и той же транзакции, без триггеров; WAL + `synchronous=NORMAL` + `foreign_keys=ON`; транзакция на батч; `unicode61` без trigram; мягкое удаление с отложенной чисткой; `auto_vacuum=INCREMENTAL`; время как INTEGER мс UTC; `sync_state` как KV плюс `folders.sync_state`; пароль вне базы.
- Порядок фаз иерархия → элементы → тела через `GetItem`; корень `msgfolderroot`; пачки по 512; клиентское окно `--days` при сохранении всех id папки; `change_key` рядом с `server_id` (нужен для `UpdateItem`/`MoveItem`); `parent_server_id` строкой без FK.
- `CredentialProvider` как трейт в transport с реализацией на keyring в CLI; `DistinguishedFolder::as_str()` в core (имена EWS общие для OWA и EWS).

## 6. Открытые вопросы для автора

1. SQLCipher: подключать до того, как появятся большие базы (параметры шифра и `page_size` фиксируются при создании файла; позже — `sqlcipher_export`, минуты на 5 ГБ и двойное место), или остаться на EFS/BitLocker + DPAPI для ключей? Оценка накладных расходов — 5–15 %.
2. Бюджет диска: 4–7 ГБ на 1 млн писем с UniqueBody (items 1–1,3 ГБ, тела 1–3 ГБ, `unique_text` 1–2 ГБ, FTS 0,5–1 ГБ). Нужна ли политика retention (тела старше N лет не хранить) и квота кэша вложений?
3. Список бесед: считать локально из `conversation_folders` (OWA-семантика, агрегаты обновляются в транзакции батча) или на этапе 1 ограничиться письмами, а беседы отложить до UI? Схема заложена в любом случае.
4. `prefix='2 3'` в FTS: +30–50 % к индексу против скорости запросов вида «договор*» — решить по замеру на реальной базе.
5. Канал уведомлений `ev.owa2` против опроса `SyncFolderItems` по таймеру: для этапа 1 опрос; когда переходить на длинный опрос и нужен ли `SubscribeToNotification` в трейте?
6. Online-архив (`archivemsgfolderroot`): есть ли он у автора и когда включать его в синхронизацию.
7. Гибридное правило перемещений (Б4): устраивает ли, что до прихода `Delete` тело временно хранится дважды, или предпочесть чистое «копирование» ценой смены `item_pk` при каждом перемещении?
