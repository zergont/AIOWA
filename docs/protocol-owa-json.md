# Протокол OWA JSON: наблюдения со спайка (Exchange 2019 CU15, сборка 15.2.1748.39)

Снято с живого трафика собственной OWA автора 16.10.2026 через перехват XHR в её же вкладке. В документе нет секретов и персональных данных: cookies, канарейка, идентификаторы папок и писем заменены плейсхолдерами. Хост обозначается `{host}`. Полные формы запросов с реальными идентификаторами лежат локально в `fixtures/raw/` (не в репозитории).

## 1. Вход (forms-based authentication)

- Страница входа: `GET /owa/auth/logon.aspx?url=https://{host}/owa&reason=0` → 200. `reason` отличен от нуля при ошибке входа или истечении сессии. Статика страницы лежит в `/owa/auth/15.2.1748/...`: сборка видна до аутентификации.
- Форма: `POST https://{host}/owa/auth.owa`, `Content-Type: application/x-www-form-urlencoded`.

| Поле | Значение | Примечание |
|---|---|---|
| `destination` | `https://{host}/owa` | куда редиректить после входа |
| `flags` | `4` | «частный компьютер»: длинный таймаут сессии |
| `forcedownlevel` | `0` | полная версия |
| `username` | `DOMAIN\user` или UPN | формат уточняется в Rust-спайке |
| `password` | пароль | |
| `passwordText` | пусто | поле режима «показать пароль» |
| `isUtf8` | `1` | |

- После успешного входа сервер ставит cookies сессии и редиректит на `destination`. Из JavaScript видны только не-HttpOnly cookies: `PrivateComputer`, `PBack`, `ClientId`, `X-OWA-CANARY`. Cookies `cadata*` HttpOnly и из страницы не видны; их имена и срок жизни снимаются в Rust-спайке из `Set-Cookie`.
- `X-OWA-CANARY` — cookie длиной 76 символов; то же значение клиент отправляет заголовком в каждом запросе к `service.svc` и параметром запроса в канале уведомлений.

## 2. Загрузка сессии: `sessiondata.ashx`

Первый вызов после входа, делается стартовым скриптом страницы:

```
POST /owa/sessiondata.ashx?appcacheclient=0
X-OWA-CorrelationId: <guid-строка>
client-request-id: <та же строка>
(тело пустое)
```

Ответ — JSON с тремя разделами:

| Раздел | Содержимое |
|---|---|
| `owaUserConfig` | профиль пользователя и политики: `SessionSettings` (адрес, имя, `MailboxGuid`, `DefaultFolderIds` и `DefaultFolderNames`, квоты, `MaxMessageSizeInKb`, `UserCulture`), `UserOptions`, `PolicySettings` (в т. ч. `AllowOfflineOn`), `AttachmentPolicy`, `SegmentationSettings`, `RetentionPolicyTags`, `MasterCategoryList`, `ViewStateConfiguration` |
| `findFolders` | готовый ответ `FindFolder`: `Body.ResponseMessages.Items[0].RootFolder.Folders[]` с полями `DisplayName`, `FolderId{Id,ChangeKey}`, `ParentFolderId`, `DistinguishedFolderId`, `FolderClass`, `TotalCount`, `UnreadCount`, `ChildFolderCount`, `EffectiveRights`, `ExtendedProperty[]` |
| `findMailFolderItem` | готовый ответ `FindConversation` для входящих (первая страница) |

Клиенту этого достаточно, чтобы после входа показать дерево папок и входящие одним запросом. Обработчик ответа трактует HTTP 440 и 401 как истёкшую сессию и уводит на страницу входа: это и есть сигнал для повторного входа.

## 3. Операции `service.svc`

### 3.1. Конверт запроса

```
POST /owa/service.svc?action=<Operation>&EP=1&UA=0&ID=<n>&AC=1
Action: <Operation>
X-OWA-ActionName: <Operation>Action
X-OWA-ActionId: <n>              — отрицательный счётчик запросов в сессии
X-OWA-Attempt: 1
X-OWA-CANARY: <значение cookie X-OWA-CANARY>
X-OWA-ClientBuildVersion: 15.2.1748.39
X-OWA-CorrelationId: <id>_<timestamp>
client-request-id: <то же>
X-OWA-ClientBegin: 2026-10-16T18:25:24.108
X-Requested-With: XMLHttpRequest
Content-Type: application/json; charset=utf-8
X-OWA-UrlPostData: <JSON запроса в URL-кодировке>   — при EP=1
```

Два способа передачи тела:

- **`EP=1` в query** → JSON запроса передаётся в заголовке `X-OWA-UrlPostData` (URL-encoded), тело POST пустое. Так ходит большинство операций, включая запросы по 1,5 КБ.
- **Без `EP`** → JSON в теле POST (`UpdateUserConfiguration`, `GetPresence`, `GetConversationItems` при инкрементальном обновлении).

Параметр `UA=0` встречается у части операций и не влияет на формат. Для нашего клиента достаточно одного режима: JSON в теле без `EP` (проверяется в спайке, что сервер принимает его для всех операций).

### 3.2. Структура JSON

Две семьи запросов:

**EWS-подобные** (FindConversation, GetConversationItems, GetFolder, GetItem, FindItem, UpdateUserConfiguration и т. д.):

```json
{
  "__type": "FindConversationJsonRequest:#Exchange",
  "Header": {
    "__type": "JsonRequestHeaders:#Exchange",
    "RequestServerVersion": "V2016_02_03",
    "TimeZoneContext": {
      "__type": "TimeZoneContext:#Exchange",
      "TimeZoneDefinition": { "__type": "TimeZoneDefinitionType:#Exchange", "Id": "Russian Standard Time" }
    }
  },
  "Body": { "__type": "FindConversationRequest:#Exchange", "...": "поля как в EWS" }
}
```

Имена типов: `<Operation>JsonRequest:#Exchange` для конверта, `<Operation>Request:#Exchange` для тела, остальные типы — имена EWS-схемы с суффиксом `:#Exchange`. `RequestServerVersion` у разных операций разный: `Exchange2013`, `V2016_02_03`, `V2017_08_18`. Сервер в ответных заголовках сообщает `x-owa-owsversion: V2017_08_18` и `x-owa-minimumsupportedowsversion: V2_6`.

**OWA-специфичные** (SubscribeToNotification, GetExtensibilityContext, GetPresence): параметры лежат на верхнем уровне рядом с объектом `request`, у которого свой `Header` и `__type`.

### 3.3. Конверт ответа

```json
{
  "Header": { "ServerVersionInfo": { "MajorVersion": 15, "MinorVersion": 2, "MajorBuildNumber": 1748, "MinorBuildNumber": 39, "Version": "V2017_08_18" } },
  "Body": { "ResponseMessages": { "Items": [ { "__type": "...", "ResponseClass": "Success", "ResponseCode": "NoError", "...": "..." } ] } }
}
```

У `FindConversation` тело без `ResponseMessages`: `Body` сразу содержит `ResponseClass`, `ResponseCode`, `Conversations[]`, `IndexedOffset`, `TotalConversationsInView`. OWA-специфичные операции отвечают произвольным JSON (массивом у подписок). Полезные заголовки ответа: `x-owa-version`, `request-id`, `x-beserver`, `x-backend-begin/end` (серверное время обработки).

### 3.4. Наблюдённые операции

| Операция | Когда | Что важно |
|---|---|---|
| `FindConversation` | список бесед в папке | `ParentFolderId.BaseFolderId` = `DistinguishedFolderId {Id:"sentitems"}` или `FolderId`; `ShapeName` именованный (`ConversationSentItemsListView`, для входящих свой); `Paging`: `IndexedPageView {Offset, MaxEntriesReturned:25}` или `SeekToConditionPageView` по `ConversationInstanceKey` для подгрузки следующих; `SortOrder` по `ConversationLastDeliveryTime`; `ViewFilter:"All"` |
| `GetConversationItems` | открытие беседы и предзагрузка видимых строк | `Conversations[{ConversationId, SyncState}]`, `ShapeName:"ItemPart"`, `SortOrder:"DateOrderDescending"`, `MaxItemsToReturn:20`, `FoldersToIgnore` (удалённые); `ItemShape` с флагами безопасности (`FilterHtmlContent`, `BlockExternalImagesIfSenderUntrusted`, `MaximumBodySize: 2097152`). Ответ: `Conversation.ConversationNodes[]`, каждый узел `{InternetMessageId, ParentInternetMessageId, HasQuotedText, IsRootNode, Items[]}`. **У элемента есть `UniqueBody {BodyType, Value, IsTruncated}`**, а также `Subject`, `From`, `Sender`, `ToRecipients`, `CcRecipients`, `DateTimeReceived/Sent/Created`, `IsRead`, `ItemClass`, `ItemId{Id,ChangeKey}`, `ParentFolderId`, `Preview`, `Attachments[]` (`AttachmentId`, `Name`, `ContentType`, `Size`, `IsInline`, `ContentId`), `Flag`, `Importance`, `Sensitivity`, `ExtendedProperty[]`. Повторный вызов с `SyncState` из прошлого ответа возвращает только изменения |
| `GetFolder` | после смены папки | `FolderIds[]`, `FolderShape {BaseShape:"IdOnly", AdditionalProperties:[UnreadCount, TotalCount]}` |
| `GetItem` | при старте (несколько), открытие одиночных элементов | формы сняты клиентом: письма § 5b', события календаря § 5g |
| `FindItem` | при старте, календарь, напоминания | форма с `CalendarPageView` снята клиентом, § 5g |
| `GetCalendarView` | календарь, листание месяца | `Body {CalendarId: TargetFolderId{BaseFolderId: FolderId{Id, ChangeKey}}, RangeStart, RangeEnd}` (локальное время без зоны, зона в `Header.TimeZoneContext`), `RequestServerVersion: V2017_08_18`; ответ `Body {ResponseClass, ResponseCode, Items[]}` |
| `GetCalendarFolders`, `GetReminders` | старт, календарь | формы не сняты |
| `FindPeople` | старт (избранные контакты), автодополнение | форма не снята |
| `SubscribeToNotification` | старт и смена папки | `subscriptionData[{SubscriptionId, Parameters{NotificationType:"RowNotification", FolderId}}]`; ответ `[{SubscriptionId, SuccessfullyCreated, SubscriptionExists, ErrorInfo}]`. Типы: `RowNotification` (строки папки), `SuiteNotification` (не поддерживается без Broker), календарные. Точная форма и проверенные клиентом типы — § 5h, поток событий — § 4 |
| `UpdateUserConfiguration` | OWA сама пишет настройки (`FrequentlyUsedFolders`) | операция записи, клиенту не нужна |
| `GetExtensibilityContext` | старт | в ответе `EwsUrl: https://{host}/EWS/Exchange.asmx` и `RestUrl` — EWS доступен как резервный канал |
| `GetPresence`, `ResetPresence`, `InstantMessageSignIn` | Lync | клиенту не нужны |
| `GetFavorites`, `GetAllowedOptions`, `GetUserVoiceData`, `GetUnseenLightningIds` | старт | второстепенны |

Не наблюдалось в этой сессии (проверяется в Rust-спайке напрямую): `SyncFolderItems`, `SyncFolderHierarchy`, `MarkAllItemsAsRead`, `CreateItem`, `UpdateItem`, `GetAttachment`. Позже сняты клиентом: `SyncFolderItems`/`SyncFolderHierarchy`/`GetItem` (§ 5b'), `GetFileAttachment` (§ 5), `UpdateItem` (§ 5e), `CreateItem`/`CreateAttachment`/`SendItem`/`GetUserConfiguration` (§ 5f), `FindItem` с `CalendarView` и `GetItem` события (§ 5g).

## 4. Канал уведомлений

Длинный опрос на `/owa/ev.owa2`:

- `POST /owa/ev.owa2?ns=PendingRequest&ev=FinishNotificationRequest&UA=0&cid=<guid>` с заголовком `X-OWA-CANARY` и пустым телом завершает предыдущий цикл;
- `GET /owa/ev.owa2?ns=PendingRequest&ev=PendingNotificationRequest&UA=0&cid=<guid>&brwnm=chrome&X-OWA-CANARY=<канарейка>&n=<токен>` держится открытым и отдаёт поток `<script>...</script>` блоков: `{id:'pg',data:'alive1',mark:'0'}` как keep-alive, `{id:'pg',data:'noerr'}`, и JSON-массивы событий `[{"EventType":"0","id":"<SubscriptionId>", ...}]` для подписок (RowNotification по папке, InstantMessageNotification и т. д.).

В наблюдении один цикл `PendingNotificationRequest` держался около 5,7 минуты, после чего клиент закрыл его `FinishNotificationRequest` (ответ `{cid, syncFnshRq}`) и открыл следующий. Канарейка в query-параметре нужна, потому что поток читается не через XHR. Для клиента на Rust достаточно `reqwest` со стримингом тела и разбором `<script>` блоков; на первом этапе можно обойтись опросом `FindConversation`/`SyncFolderItems` по таймеру.

Уточнения по итогам клиента `aiowa` (23.10.2026, подробности и форма подписок — § 5h):

- Ответ `GET` потока: `200 OK`, `Content-Type: text/html; charset=UTF-8`, `Transfer-Encoding: chunked`, без сжатия (клиент шлёт `Accept-Encoding: identity`, чтобы блоки не задерживались в буфере gzip). Первый блок приходит через 150 мс после запроса.
- Служебные блоки — **JS-литералы без обёртки-вызова**, ровно `<script>{id:'pg',data:'alive1',mark:'0'}</script>`: `alive1` сразу после открытия, `{id:'pg',data:'noerr'}` через 40 с, затем `{id:'pg',data:'update'}` каждые 40 с, пока в ящике ничего не происходит; между ними сервер шлёт пустые `<script></script>` (24 за цикл). Все эти блоки клиент считает признаком живого потока (`KeepAlive`, пустые просто сбрасывают таймер); тишина дольше 150 с (больше трёх пропущенных keep-alive) — обрыв: мёртвое соединение после сна или смены сети иначе висело бы до таймаута без единого события.
- **Цикл закрывает сервер через 320 с** (5,3 мин): присылает `{id:'pg',data:'restart'}`, затем `{id:'pg',data:'alive0'}` и завершает chunked-ответ штатно (клиент видит конец тела без ошибки). Подписки при этом сохраняются: клиент шлёт `FinishNotificationRequest` (148 мс после штатного конца, около 870 мс, если поток оборвал клиент) и открывает следующий `PendingNotificationRequest` с `n` на единицу больше — новый цикл снова начинается с `alive1`.
- `FinishNotificationRequest` требует явного `Content-Length: 0`: пустой POST без заголовка длины IIS отвергает с `HTTP 411 Length Required`. С заголовком — `200 OK`, тело 61 байт `{"cid":"<guid>","syncFnshRq":<число>}`.
- `cid` и `SubscriptionId` клиент выбирает сам (guid из 36 символов); браузерная OWA для подписки на строки папки использует `<FolderId>RowNotification` (135 символов), серверу это безразлично.

## 5. Вложения (снято клиентом `aiowa` 17.10.2026)

Метаданные вложений приходят вместе с письмом в `GetItem` с `ShapeName: ItemPart`: `Attachments[]` с `AttachmentId{Id}`, `Name`, `ContentType`, `Size`, `IsInline`, `ContentId`. `AttachmentId.Id` на этом сервере — строка из 180 символов (base64, включает идентификатор письма), поэтому при перемещении письма идентификаторы его вложений меняются вместе с `ItemId`.

**Скачивание файла** идёт тем же путём, что у браузерной OWA, семейством `service.svc/s/<Op>` для GET-операций с бинарным ответом (как `GetPersonaPhoto`):

```
GET /owa/service.svc/s/GetFileAttachment?id=<AttachmentId.Id, URL-encoded>&X-OWA-CANARY=<канарейка>
Cookie: <cookies сессии>
X-OWA-CANARY: <канарейка>          — дублируется заголовком, как у service.svc
```

Ответ `200 OK` с телом файла как есть, `Content-Type` файла (`application/pdf`, `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`, ...) и `Content-Disposition: attachment; filename=%D0%A1%D0%BF...` (имя в процентном кодировании UTF-8 прямо в `filename=`, без `filename*=`). Замеры: 0,4–1,9 с на файл 30–550 КБ при одном запросе за раз; 436 файлов (349 МБ) за один прогон без отказов и троттлинга. Сессионные сигналы те же, что у `service.svc`: 440/401 или редирект на `logon.aspx` = истёкшая сессия, без канарейки — редирект на `errorfe.aspx`. Ответ `text/html` со статусом 200 означает страницу ошибки, а не файл (клиент проверяет тип).

Запасной путь, реализованный в клиенте на случай отказа GET (на этом сервере не понадобился): операция `GetAttachment` по EWS-схеме, `RequestServerVersion: Exchange2013`, тело

```json
{ "__type": "GetAttachmentRequest:#Exchange",
  "AttachmentIds": [ { "__type": "AttachmentId:#Exchange", "Id": "…" } ],
  "AttachmentShape": { "__type": "AttachmentResponseShape:#Exchange", "IncludeMimeContent": false, "BodyType": "Best" } }
```

с ответом `Body.ResponseMessages.Items[0].Attachments[0]` типа `FileAttachment:#Exchange` и содержимым в `Content` (base64). Для вложений-писем (`ItemAttachment`) поля `Content` нет.

Следствия для клиента: скачивать по одному, с ограничением размера по `Content-Length` и по счётчику прочитанных байт; инлайновые картинки (`IsInline: true`, есть `ContentId`) нужны только рендеру и не качаются индексом; текст извлекается локально (PDF с текстовым слоем, DOCX, XLSX/XLS, TXT/CSV), сканы дают пустой текст, OCR отложен.

## 5a. TLS сервера

Замерено `openssl s_client` 16.10.2026: сервер договаривается на **TLS 1.2, шифр `ECDHE-RSA-AES256-SHA384`** (CBC, не AEAD), ключ RSA 2048, подпись `rsa_pkcs1_sha256`. Библиотека rustls (в том числе через `rustls-platform-verifier`) поддерживает только AEAD-шифры (AES-GCM, ChaCha20) и обрывает рукопожатие с ошибкой `tls handshake eof`; системный Schannel Windows и OpenSSL соединяются нормально. Следствие для клиента: HTTP-стек на Windows должен использовать системный TLS (`reqwest` с `native-tls`, а не `rustls`), пока админы не включат на сервере GCM-шифры. Это же касается любого другого Rust-кода, который ходит на этот сервер (канал уведомлений, загрузка вложений).

## 5b. Проверено утилитой `spike/owa-probe` (Rust, 16.10.2026)

| Проверка | Результат |
|---|---|
| Вход через форму из Rust | работает: `POST /owa/auth.owa` → 302 на `/OWA`, cookies `cadata`, `cadataTTL`, `cadataKey`, `cadataIV`, `cadataSig` (все HttpOnly, без Expires = сессионные); `GET /OWA` → 301 `/owa/` ставит `ClientId` (на год), `UC`, `OutlookSession`, `X-BackEndCookie` (30 дней, привязка к бэкенду); `GET /owa/` ставит `X-OWA-CANARY`, `UserContext`, `tzid`. Весь вход около 2–3 с |
| `sessiondata.ashx` после входа | работает без дополнительных заголовков, кроме `X-OWA-CorrelationId`/`client-request-id`: три раздела, 57 папок |
| **Порядок ключей JSON** | **критично: `__type` должен быть первым ключом каждого объекта.** Иначе HTTP 500 `MemberAccessException: Невозможно создать абстрактный класс` на любой операции. В Rust: `serde_json` с фичей `preserve_order` и `__type` первым в литералах |
| JSON в теле без `EP` | принимается для всех операций |
| JSON в `X-OWA-UrlPostData` с `EP=1` | принимается |
| Минимальные заголовки | достаточно `Action`, `X-OWA-CANARY`, `Content-Type: application/json; charset=utf-8`; остальные `X-OWA-*` заголовки необязательны |
| Без `X-OWA-CANARY` | HTTP 302 на `/owa/auth/errorFE.aspx?httpCode=500`: так выглядит отказ CSRF-защиты, отличать от истечения сессии (440/401/редирект на `logon.aspx`) |
| `FindConversation` без `ShapeName` | `NoError`, но только идентификаторы; для полей нужен `ShapeName` (`ConversationListView` для входящих работает) или `AdditionalProperties` |
| `GetFolder` с `DistinguishedFolderId` | работает: входящие 77 177 элементов, 3 682 непрочитанных |
| `SyncFolderItems` по `inbox` | работает: `SyncState` 636 символов, изменения приходят пачками по `MaxChangesReturned` |
| `SyncFolderHierarchy` от `msgfolderroot` | работает: `SyncState` 1 648 символов |
| `GetConversationItems` с `ShapeName: ItemPart` | работает: 20 узлов, `UniqueBody` типа HTML, не усечён, `Subject` и `InternetMessageId` присутствуют |
| Задержки | обычный вызов 130–260 мс; `SyncFolderItems` первый вызов 4,5 с; первый `FindConversation` в свежей сессии однажды занял 49,6 с (повторить замер) |

## 5b'. Формы `SyncFolderItems` и `GetItem` (снято утилитой, 16.10.2026)

**`SyncFolderItems`** (`RequestServerVersion: Exchange2013`), запрос: `Body {ItemShape {BaseShape:"IdOnly", AdditionalProperties?}, SyncFolderId {TargetFolderId{BaseFolderId}}, MaxChangesReturned, SyncScope:"NormalItems", SyncState?}`. Ответ `Body.ResponseMessages.Items[0]`:

```json
{ "__type": "SyncFolderItemsResponseMessage:#Exchange", "ResponseClass": "Success", "ResponseCode": "NoError",
  "Changes": { "Changes": [
      { "__type": "SyncFolderItemsCreateType:#Exchange", "ChangeType": "Create",
        "Item": { "__type": "Message:#Exchange", "ItemId": { "Id": "…", "ChangeKey": "…" }, "...": "запрошенные свойства" } } ] },
  "SyncState": "…616 символов…", "TotalCount": 0 }
```

- Изменения лежат во вложенном массиве `Changes.Changes[]`; `ChangeType` принимает `Create`, `Update`, `Delete`, `ReadFlagChange` (последние два в этой сессии не наблюдались, форма по EWS: у `Delete` только `ItemId`, у `ReadFlagChange` `ItemId` и `IsRead`).
- `IncludesLastItemInRange` в ответе не было; есть `TotalCount` (семантика уточняется: 0 при неполной выдаче?).
- `AdditionalProperties` работают: и `item:Subject`, и `Subject` дают поле `Subject` у элементов. Значит метаданные можно получать прямо в синхронизации, без отдельного прохода `GetItem`.

**`GetItem`** (`RequestServerVersion: V2017_08_18`), запрос: `Body {ItemShape, ItemIds:[ItemId{Id, ChangeKey}], ShapeName?}`. Ответ `Body.ResponseMessages.Items[0].Items[0]`. Варианты формы, все `NoError`, 145–285 мс на одно письмо:

| Форма | Ключей | Что приходит |
|---|---|---|
| `IdOnly` + `AdditionalProperties` (с префиксами `item:`/`message:` или без) | 8 | ровно запрошенное; `UniqueBody` доступен и вне контекста беседы (22 644 символа HTML) |
| `ShapeName: "ItemPart"` | 43 | **рабочая форма для тел**: `UniqueBody`, `Attachments`, `From`, `Sender`, `ToRecipients`, `CcRecipients`, `DisplayTo`, `ConversationId`, `ConversationThreadId`, `InternetMessageId`, `ParentFolderId`, `ItemClass`, `IsRead`, `IsDraft`, `Flag`, `Importance`, `Sensitivity`, `Size`, `Preview`, `DateTimeReceived/Sent/Created`, `LastModifiedTime`, `HasAttachments`, `HasBlockedImages`, `IconIndex`, `RecipientCounts`, `ExtendedProperty` |
| `ShapeName: "ItemNormalizedBody"` | 42 | то же, но `NormalizedBody` (полное тело после нормализации) вместо `UniqueBody` |
| `ShapeName: "MailListItem"` | 28 | **форма для списка**: без тел и получателей; `Subject`, `From`, `Sender`, `DisplayTo`, даты, `IsRead`, `Flag`, `Importance`, `Size`, `Preview`, `ConversationId`, `ParentFolderId`, `ItemClass`, `HasAttachments`, `IconIndex`, `InstanceKey` |
| `BaseShape: "AllProperties"`, `MaximumBodySize: 4096` | 61 | плюс `Body` (усечён ровно до 4 096 символов), `InternetMessageHeaders`, `References`, `InReplyTo`, `ConversationTopic`, `ConversationIndex`, `IsFromMe`, `ReceivedBy`, `EffectiveRights`, `LastModifiedName`; `UniqueBody` отсутствует |

Вывод для синхронизации: `SyncFolderItems` с `AdditionalProperties` (или проверить `ShapeName: MailListItem` в нём) даёт метаданные списка; `GetItem` пачками с `ShapeName: ItemPart` даёт тела, получателей и вложения. Полное тело с цитатами при необходимости — отдельным `GetItem` с `ItemNormalizedBody`.

Уточнения по итогам клиента `aiowa` (16.10.2026):
- Имена свойств лучше давать со стандартными префиксами EWS: без префикса `LastModifiedTime` отвергается (`Invalid value 'LastModifiedTime' for enum PropertyUriEnum`, HTTP 500), тогда как `item:LastModifiedTime` принимается. Список, который работает в `SyncFolderItems`: `item:Subject`, `item:DateTimeReceived`, `item:DateTimeSent`, `item:LastModifiedTime`, `message:From`, `message:Sender`, `item:DisplayTo`, `message:IsRead`, `item:IsDraft`, `item:HasAttachments`, `item:Importance`, `item:Size`, `item:ItemClass`, `item:ConversationId`, `message:InternetMessageId`, `item:Preview`, `item:Flag`, `item:ParentFolderId`.
- `SyncFolderHierarchy` с `BaseShape: IdOnly` и `AdditionalProperties: [DisplayName, ...]` молча не отдаёт `DisplayName` (счётчики и `DistinguishedFolderId` приходят). С `BaseShape: AllProperties` имена есть.
- Темп `SyncFolderItems` по 512 с метаданными: первый вызов по папке 27 с (сервер строит состояние), далее 4–7 с на пачку. Полная выгрузка метаданных двух папок (77 177 входящих + 25 011 отправленных = 102 188 писем, 200 пачек) заняла 1 242 с вместе с телами 1 845 писем за 90 дней, без единой ошибки и без троттлинга при одном потоке запросов.
- Признак конца выдачи `IncludesLastItemInRange` сервер в этой сборке не присылает: последняя пачка просто короче запрошенного (377 и 435 элементов). Клиент считает папку завершённой по короткой пачке, а на случай ровно кратного числа элементов — по пустой выдаче.
- `TimeZoneContext.TimeZoneDefinition.Id = "UTC"` принимается; даты приходят со смещением `+00:00`/`Z` и разбираются `jiff`.

## 5c. Лимит сессий на ящик

После примерно десяти входов подряд без выхода (каждый запуск утилиты открывал новую сессию и не закрывал её) сервер начал отвергать новые входы: `POST /owa/auth.owa` → 302 на `/OWA` → 302 на `/owa/auth/errorfe.aspx?httpCode=500&owaError=Microsoft.Exchange.Data.Storage.TooManyObjectsOpenedException&inex=Microsoft.Mapi.MapiExceptionSessionLimit`. Это лимит одновременных MAPI-сессий на ящик (по умолчанию в Exchange 16 сессий OWA на пользователя). Браузерная OWA, уже вошедшая, продолжает работать; страдают только новые входы, в том числе и браузера пользователя, если он откроет OWA заново.

Следствия для клиента:
- одна сессия на всё время работы приложения; cookies живут в памяти процесса и переиспользуются всеми фоновыми задачами;
- при завершении работы и перед повторным входом обязательно `GET /owa/logoff.owa`;
- повторный вход только после `440`/`401`/редиректа на `logon.aspx`, не «на всякий случай»;
- на ошибку `errorfe.aspx` с `MapiExceptionSessionLimit` отвечать ожиданием (сессии освобождаются по тайм-ауту бездействия), а не новыми попытками входа;
- в тестах и спайках: `logoff` в конце каждой команды.

## 5d. Жизнь сессии при простое

| Простой без единого запроса | Результат |
|---|---|
| 30 мин | сессия жива: `GetFolder` → `NoError`, первый запрос после паузы 1 155 мс против 130–160 мс в активной сессии (сервер поднимает состояние сессии заново) |
| 60 мин | сессия жива: `GetFolder` → `NoError`, первый запрос после паузы 1 031 мс; `logoff.owa` после этого ответил 302 |
| 120 мин | сессия жива: `GetFolder` → `NoError`, первый запрос после паузы 1 159 мс |

Вывод: при входе с `flags=4` («частный компьютер») сессия переживает как минимум два часа без единого запроса; таймаут бездействия, если он есть, больше двух часов. Фоновая синхронизация раз в час может жить на одной сессии без повторных входов; сигнал к повторному входу только 440/401/редирект на `logon.aspx`. Первый запрос после долгой паузы стабильно занимает около секунды против 130–160 мс в активной сессии.

Вход под давлением лишних сессий (перед срабатыванием лимита) занимал около 8 с вместо 2–3 с.

## 5e. Операции записи (клиент `aiowa`, 23.10.2026)

Все три операции идут через тот же `service.svc` с `RequestServerVersion: V2017_08_18` (как `GetItem`), `__type` первым ключом, пачками по 25 идентификаторов. Ответ по каждому элементу — отдельное сообщение `Body.ResponseMessages.Items[i]` в порядке запроса; `ResponseClass: Error` у одного сообщения — отказ по этому письму, а не по всему вызову.

**`UpdateItem`** (прочитано/непрочитано, флаг):

```json
{ "__type": "UpdateItemRequest:#Exchange",
  "ConflictResolution": "AlwaysOverwrite", "MessageDisposition": "SaveOnly", "SuppressReadReceipts": true,
  "ItemChanges": [ { "__type": "ItemChange:#Exchange",
      "ItemId": { "__type": "ItemId:#Exchange", "Id": "…", "ChangeKey": "…" },
      "Updates": [ { "__type": "SetItemField:#Exchange",
          "Path": { "__type": "PropertyUri:#Exchange", "FieldURI": "message:IsRead" },
          "Item": { "__type": "Message:#Exchange", "IsRead": true } } ] } ] }
```

- Флаг: `FieldURI: "item:Flag"`, `Item: { "__type": "Message:#Exchange", "Flag": { "__type": "FlagType:#Exchange", "FlagStatus": "Flagged" } }`; снятие — `"NotFlagged"`.
- `AlwaysOverwrite` нужен потому, что локальный `ChangeKey` может отстать от сервера после чужих правок; `SaveOnly` обязателен для писем (иначе сервер требует `MessageDisposition`).
- **Снято с сервера 23.10.2026** (`aiowa mark-read` на одном старом письме входящих: `IsRead` false, затем обратно true; оба вызова по 480–500 мс, ответ 546 байт): HTTP 200, `Body.ResponseMessages.Items[0]` =

  ```json
  { "__type": "UpdateItemResponseMessage:#Exchange", "ResponseCode": "NoError", "ResponseClass": "Success",
    "Items": [ { "__type": "Message:#Exchange", "ItemId": { "__type": "ItemId:#Exchange", "Id": "<тот же Id>", "ChangeKey": "<новый>" } } ],
    "ConflictResults": { "...": "счётчик конфликтов, клиенту не нужен" } }
  ```

  `Id` письма не меняется (152 символа, как в базе). `ChangeKey` в ответе **короткий, 8 символов**, тогда как `SyncFolderItems`/`GetItem` отдают ключи по 40 символов: OWA JSON кладёт в ответ `UpdateItem` усечённую форму ключа. Второй вызов, отправленный с этим 8-символьным ключом, прошёл и вернул тот же 8-символьный ключ; ключ недельной давности из синхронизации с `AlwaysOverwrite` сервер тоже принял. Клиент записывает ключ из ответа в базу (`update_item_id`); полноценный ключ вернётся со следующим `SyncFolderItems` (`Update`). Для `MoveItem` и `DeleteItem` ключ не нужен. Отказ по элементу приходит тем же сообщением с `ResponseClass: Error` и `ResponseCode` (например, `ErrorItemNotFound`) — в этой проверке не наблюдался.

**`MoveItem`**:

```json
{ "__type": "MoveItemRequest:#Exchange",
  "ToFolderId": { "__type": "TargetFolderId:#Exchange",
      "BaseFolderId": { "__type": "FolderId:#Exchange", "Id": "…" } },
  "ItemIds": [ { "__type": "ItemId:#Exchange", "Id": "…", "ChangeKey": "…" } ],
  "ReturnNewItemIds": true }
```

`BaseFolderId` может быть и `{ "__type": "DistinguishedFolderId:#Exchange", "Id": "deleteditems" }`. В ответе ожидается `Items[0].ItemId` с новым идентификатором письма в целевой папке (идентификаторы вложений тоже меняются, см. § 5). **На сервере не проверялось**: реализовано по схеме EWS и покрыто юнит-тестами сборки тела и разбора ответа; первое живое выполнение — за человеком после сборки.

**`DeleteItem`**:

```json
{ "__type": "DeleteItemRequest:#Exchange",
  "ItemIds": [ { "__type": "ItemId:#Exchange", "Id": "…", "ChangeKey": "…" } ],
  "DeleteType": "MoveToDeletedItems", "SuppressReadReceipts": true, "ReturnMovedItemIds": true }
```

`DeleteType: "HardDelete"` — навсегда. Разбор ответа терпим к форме: новый идентификатор берётся из `Items[0].ItemId`, `MovedItemId` или `MovedItemIds[0]`, если ничего нет — остаётся исходный. **На сервере не проверялось** (та же оговорка, что у `MoveItem`).

Разбор по элементам (`protocol::parse_write_results`): при совпадении числа сообщений и идентификаторов — по позиции; иначе успешные сопоставляются по `ItemId` из ответа, единственное сообщение-ошибка на пачку относится ко всем элементам, элементы без ответа получают отказ `NoResponse`. `ErrorServerBusy` на всю пачку — `Throttled` (отказ вызова), а не отказ по письмам.

## 5f. Создание письма, вложения, отправка, подпись (клиент `aiowa`, 23.10.2026)

Снято на рабочем ящике одним прогоном `aiowa send --to-self --attach … --yes-i-know` (новое письмо самому себе, тема «[AIOWA] проверка отправки», тело из одной строки, одно текстовое вложение 1 621 байт) и тремя вызовами `aiowa signature` (только чтение). Все четыре операции — обычный `service.svc` с `__type` первым ключом; `CreateItem`, `CreateAttachment`, `SendItem` с `RequestServerVersion: V2017_08_18` (как `GetItem`), `GetUserConfiguration` с `Exchange2013`. Ни одна форма из контракта не была отвергнута: `Body` как объект `BodyContentType`, `Items` с `__type`, получатели как `EmailAddress:#Exchange` — всё принято с первого раза. Весь путь черновик → вложение → отправка занял около 0,6 с (219 + 184 + 229 мс).

**`CreateItem`** (черновик в «Черновиках»):

```json
{ "__type": "CreateItemRequest:#Exchange",
  "MessageDisposition": "SaveOnly",
  "SavedItemFolderId": { "__type": "TargetFolderId:#Exchange",
      "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "drafts" } },
  "Items": [ { "__type": "Message:#Exchange",
      "Subject": "…",
      "Body": { "__type": "BodyContentType:#Exchange", "BodyType": "HTML", "Value": "<div>…</div>" },
      "ToRecipients": [ { "__type": "EmailAddress:#Exchange", "Name": "…", "EmailAddress": "…", "RoutingType": "SMTP" } ],
      "CcRecipients": [ "…" ], "BccRecipients": [ "…" ],
      "Importance": "Normal", "IsRead": true } ] }
```

Пустые списки получателей клиент не передаёт; `Name` у получателя необязателен (в проверке его не было). Ответ HTTP 200, 548 байт, `Body.ResponseMessages.Items[0]` =

```json
{ "__type": "ItemInfoResponseMessage:#Exchange", "ResponseCode": "NoError", "ResponseClass": "Success",
  "Items": [ { "__type": "Message:#Exchange",
      "ItemId": { "__type": "ItemId:#Exchange", "Id": "<152 символа>", "ChangeKey": "<40 символов>" } } ] }
```

`Id` черновика той же длины, что у писем из синхронизации (152), `ChangeKey` здесь полный (40 символов), а не усечённый, как у `UpdateItem` (§ 5e).

Ответ и пересылка — тот же `CreateItem`, но элемент вместо `Message:#Exchange` имеет тип `ReplyToItem:#Exchange`, `ReplyAllToItem:#Exchange` или `ForwardItem:#Exchange` с полями `ReferenceItemId { "__type": "ItemId:#Exchange", Id, ChangeKey }`, `NewBodyContent` (та же форма, что `Body`), `Subject`, `ToRecipients`/`CcRecipients`/`BccRecipients`; `IsRead` не передаётся, `Importance` — только отличная от `Normal`. Сервер сам дописывает к `NewBodyContent` цитируемый оригинал, а у `ForwardItem` переносит и его вложения; тему и получателей клиент считает сам и передаёт явно. В отличие от нового письма, у ответов и пересылки все три списка получателей передаются всегда, пустой — как `[]`: по семантике EWS непереданный список у `ReplyToItem`/`ReplyAllToItem` сервер заполняет из оригинала сам, и письмо ушло бы тем, кого пользователь убрал из «Кому»/«Копии». **На сервере не проверялось**: формы покрыты юнит-тестами сборки тела (`protocol::outgoing_item_json`), первое живое выполнение — за человеком после сборки окна. Проверка без отправки: «Ответить всем» на письмо с «Копией», снять все чипы из «Копии», «Сохранить черновик» (`SaveOnly`), посмотреть `CcRecipients` черновика (OWA или `GetItem`) — должен быть пуст; черновик удалить навсегда. Если сервер проигнорирует `[]`, запасной путь — отдельный `UpdateItem` только по получателям (`SetItemField` для непустых, `DeleteItemField` для пустых) после `CreateItem`, без `item:Body`, чтобы не затереть добавленную сервером цитату.

**`CreateAttachment`** (по одному файлу на запрос; клиент шлёт файлы последовательно, с таймаутом запроса 10 мин вместо общих 90 с — `OwaConfig::upload_timeout`):

```json
{ "__type": "CreateAttachmentRequest:#Exchange",
  "ParentItemId": { "__type": "ItemId:#Exchange", "Id": "<Id черновика>", "ChangeKey": "<его ChangeKey>" },
  "Attachments": [ { "__type": "FileAttachment:#Exchange",
      "Name": "…", "ContentType": "text/plain", "IsInline": false, "Content": "<base64>" } ] }
```

Ответ HTTP 200, 847 байт (для файла 1,6 КБ), 184 мс:

```json
{ "__type": "AttachmentInfoResponseMessage:#Exchange", "ResponseCode": "NoError", "ResponseClass": "Success",
  "Attachments": [ { "__type": "FileAttachment:#Exchange",
      "AttachmentId": { "RootItemChangeKey": "<40 символов, новый>", "RootItemId": "<Id черновика, тот же>", "Id": "<AttachmentId>" },
      "LastModifiedTime": "…" } ] }
```

`RootItemId` совпадает с `Id` черновика, `RootItemChangeKey` — новый ключ черновика после добавления файла: именно его клиент подставляет в `SendItem` (`add_attachments` возвращает `ItemId` с ключом последнего ответа). Размер тела запроса — размер файла ×4/3 плюс конверт; лимиты клиента 25 МиБ на файл и 35 МиБ на письмо, серверный предел (`MaxMessageSizeInKb` из `sessiondata.ashx`, § 2) на этом файле не проверялся.

**`SendItem`**:

```json
{ "__type": "SendItemRequest:#Exchange",
  "ItemIds": [ { "__type": "ItemId:#Exchange", "Id": "<Id черновика>", "ChangeKey": "<RootItemChangeKey>" } ],
  "SaveItemToFolder": true,
  "SavedItemFolderId": { "__type": "TargetFolderId:#Exchange",
      "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "sentitems" } } }
```

Ответ HTTP 200, 232 байта, 229 мс; сообщение ответа **без `__type`**: `Body.ResponseMessages.Items[0]` = `{ "ResponseCode": "NoError", "ResponseClass": "Success" }`. Письмо ушло (копия должна появиться в «Отправленных» и прийти во «Входящие» при следующей синхронизации; это проверяет человек). Отказ сервера по письму ожидается тем же сообщением с `ResponseClass: Error` и `ResponseCode` (например, `ErrorItemNotFound`, если `Id`/`ChangeKey` устарели) — в проверке не наблюдался. Клиент при отказе после удачного `CreateItem` не удаляет черновик автоматически в окне (он остаётся в «Черновиках»); CLI `aiowa send` без `--keep-draft` удаляет его `DeleteItem HardDelete`.

**`UpdateItem` черновика** (повторное «сохранить черновик»): тот же `UpdateItemRequest`, что в § 5e (`AlwaysOverwrite`, `SaveOnly`), с `Updates` из `SetItemField` для `item:Subject`, `item:Body` (значение — `Message:#Exchange` с `Body: BodyContentType`), `item:Importance`, `message:ToRecipients`/`CcRecipients`/`BccRecipients` (значение — `Message:#Exchange` со списком) и `DeleteItemField { "__type": "DeleteItemField:#Exchange", "Path": PropertyUri }` для опустевших списков. Ответ ожидается как у § 5e (`Items[0].ItemId` с новым `ChangeKey`). **На сервере не проверялось**, покрыто юнит-тестом точной формы.

**`GetUserConfiguration`** (подпись пользователя из настроек OWA):

```json
{ "__type": "GetUserConfigurationRequest:#Exchange",
  "UserConfigurationName": { "__type": "UserConfigurationNameType:#Exchange",
      "Name": "OWA.UserOptions",
      "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "root" } },
  "UserConfigurationProperties": "All" }
```

- Объект `OWA.UserOptions` лежит в **корне ящика `root`** (non-IPM subtree, как его читает EWS Managed API), а не в корне почтовых папок: с `msgfolderroot` сервер отвечает `ResponseClass: Error`, `ErrorItemNotFound`, «The specified object was not found in the store., Объект конфигурации не найден. Имя = OWA.UserOptions.» (446 байт). Клиент пробует `root`, затем `msgfolderroot`; `ErrorItemNotFound` в обоих — подписи нет.
- Ответ с `root`: HTTP 200, 10 530 байт, 177–271 мс:

  ```json
  { "__type": "GetUserConfigurationResponseMessage:#Exchange", "ResponseCode": "NoError", "ResponseClass": "Success",
    "UserConfiguration": {
      "UserConfigurationName": { "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "root" }, "Name": "OWA.UserOptions" },
      "ItemId": { "Id": "<152 символа>", "ChangeKey": "<40 символов>" },
      "Dictionary": [ { "DictionaryKey":   { "Type": "String",  "Value": [ "signaturehtml" ] },
                        "DictionaryValue": { "Type": "String",  "Value": [ "<div>…</div>" ] } },
                      { "DictionaryKey":   { "Type": "String",  "Value": [ "autoaddsignature" ] },
                        "DictionaryValue": { "Type": "Boolean", "Value": [ true ] } },
                      "… всего 27 записей" ] } }
  ```

- `Value` у ключа и у значения — всегда **массив** из одного элемента (в схеме EWS элемент `Value` повторяемый); `Type` значения — `String`, `Boolean` и т. д. Клиент разбирает булевы значения и как JSON `true`/`false`, и как строки `"True"`/`"true"` (форма элемента в журнале не фиксировалась). Ключи подписи в **нижнем регистре**: `signaturehtml`, `signaturetext`, `autoaddsignature`, `autoaddsignatureonreply`; остальные ключи в смешанном регистре (`FavoriteFolders`, `FrequentlyUsedFolders`, `themeStorageId`, `timezone`, `NavigationBarWidth`, `IsReplyAllTheDefaultResponse`, `MruEmojis`, …). Клиент сравнивает ключи без учёта регистра и берёт `Value[0]`. На проверенном ящике: подпись 3 800 символов HTML (185 тегов, со `<style>` блоком OWA), `autoaddsignature = true`.
- `XmlData`/`BinaryData` в ответе нет: все настройки OWA 2019 хранит в словаре.

## 5g. Календарь: `FindItem` с `CalendarView` и `GetItem` события (клиент `aiowa`, 23.10.2026)

Снято запусками `aiowa calendar` (только чтение: `FindItem`, `GetItem`, каждый запуск с `logoff`) на рабочем ящике: неделя 19–26.10.2026 (пусто), год 2026 (30 экземпляров), подробности одной встречи с 35 участниками и одного экземпляра ежегодного события. Все операции — обычный `service.svc`, `__type` первым ключом, `RequestServerVersion: V2017_08_18`, `TimeZoneContext` = `UTC`.

**`FindItem` с `CalendarPageView`** — принят с первого раза; запасные формы клиента (даты без `Z`, операция `GetCalendarView` из § 3.4) не понадобились:

```json
{ "__type": "FindItemRequest:#Exchange",
  "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly",
      "AdditionalProperties": [ { "__type": "PropertyUri:#Exchange", "FieldURI": "item:Subject" },
          "… calendar:Start, calendar:End, calendar:IsAllDayEvent, calendar:Location, calendar:Organizer, calendar:LegacyFreeBusyStatus,",
          "… calendar:CalendarItemType, calendar:IsMeeting, calendar:IsCancelled, calendar:MyResponseType, item:Sensitivity,",
          "… item:HasAttachments, item:LastModifiedTime, calendar:AppointmentState" ] },
  "ParentFolderIds": [ { "__type": "DistinguishedFolderId:#Exchange", "Id": "calendar" } ],
  "Traversal": "Shallow",
  "Paging": { "__type": "CalendarPageView:#Exchange", "StartDate": "2025-12-31T21:00:00Z", "EndDate": "2026-12-31T21:00:00Z" } }
```

Ответ HTTP 200 (год: 30 элементов, 26 КБ, 215–360 мс; пустая неделя: 391 байт, 676 мс первым запросом свежей сессии):

```json
{ "__type": "FindItemResponseMessage:#Exchange", "ResponseClass": "Success", "ResponseCode": "NoError",
  "RootFolder": { "IncludesLastItemInRange": true, "TotalItemsInView": 30, "Groups": null,
    "Items": [ { "__type": "CalendarItem:#Exchange",
        "ItemId": { "Id": "<152 символа у одиночного, 168 у экземпляра>", "ChangeKey": "<40>" },
        "Subject": "…", "Sensitivity": "Normal", "HasAttachments": true, "LastModifiedTime": "2026-01-10T08:15:33Z",
        "Start": "2026-01-15T10:00:00Z", "End": "2026-01-15T11:00:00Z", "IsAllDayEvent": false,
        "FreeBusyType": "Tentative", "IsMeeting": true, "IsCancelled": false, "CalendarItemType": "Single",
        "ResponseType": "NoResponseReceived",
        "Organizer": { "Mailbox": { "Name": "…", "EmailAddress": "…", "RoutingType": "SMTP", "MailboxType": "Mailbox" } },
        "AppointmentState": 3 } ] } }
```

Особенности:

- Имена полей в ответе — OWA JSON, а не EWS: занятость приходит как **`FreeBusyType`** (запрошена `calendar:LegacyFreeBusyStatus`), свой ответ на приглашение — как **`ResponseType`** (запрошен `calendar:MyResponseType`). Клиент читает оба имени (`protocol::parse_calendar_event`).
- **`IsCancelled` в `FindItem` появляется только вместе с `calendar:AppointmentState`** в `AdditionalProperties`: без него поля в ответе нет вовсе (в `GetItem` оно есть всегда). Клиент считает событие отменённым по `IsCancelled` или по биту `0x4` в `AppointmentState` (наблюдались значения 0, 3, 7).
- `Location` не пришло ни разу — ни в `FindItem`, ни в `GetItem` с `AllProperties`: у проверенных событий места нет, пустые строки сервер, похоже, опускает. Клиент принимает и строку, и объект с `DisplayName` (`EnhancedLocation`).
- Даты `Start`/`End`/`LastModifiedTime` — ISO с `Z`, 20 символов. Повторяющиеся встречи приходят развёрнутыми в экземпляры (`CalendarItemType: Occurrence`), у каждого свой `ItemId`. `IncludesLastItemInRange: true`, `TotalItemsInView` равен числу элементов; неполная выдача на годовом окне не наблюдалась (клиент на `false` делит окно пополам и перечитывает половины).
- **События на весь день** приходят как настоящие моменты в UTC, без нормализации к полуночи контекста: событие на 6 мая, созданное в московском поясе, — `Start: 2026-05-05T21:00:00Z`, `End: 2026-05-06T21:00:00Z`; у события из пояса UTC+6 границы в `…T18:00:00Z`. Поэтому день показа всё-день события — **местная дата ближайшей полуночи к `start_ms`**, а не просто местная дата начала: при поясе организатора восточнее местного дата съехала бы на день назад.
- Наблюдённые значения: `CalendarItemType` — `Single`, `Occurrence`; `FreeBusyType` — `Free`, `Tentative`, `Busy`; `ResponseType` — `Unknown`, `Organizer`, `Tentative`, `Accept`, `NoResponseReceived`; `Sensitivity` — `Normal`. Встречается экземпляр нулевой длины (`Start == End`).

**`GetItem` события** — форма `IdOnly` + явный список свойств принята (запасная `AllProperties` не понадобилась):

```json
{ "__type": "GetItemRequest:#Exchange",
  "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly", "BodyType": "HTML",
      "FilterHtmlContent": true, "BlockExternalImagesIfSenderUntrusted": true, "MaximumBodySize": 2097152, "MaximumRecipientsToReturn": 500,
      "AdditionalProperties": [ "… те же 15 полей, что у FindItem …",
          "calendar:RequiredAttendees", "calendar:OptionalAttendees", "calendar:Recurrence", "item:Body", "item:ReminderIsSet", "item:ReminderMinutesBeforeStart" ] },
  "ItemIds": [ { "__type": "ItemId:#Exchange", "Id": "…", "ChangeKey": "…" } ],
  "ShapeName": null }
```

Ответ `ItemInfoResponseMessage:#Exchange`, `Items[0]` типа `CalendarItem:#Exchange` (встреча с 35 участниками — 22 КБ, 465 мс; одиночное событие без участников — 1,8 КБ, 160–215 мс):

```json
{ "__type": "CalendarItem:#Exchange", "ItemId": { "Id": "…", "ChangeKey": "…" }, "Subject": "…", "Sensitivity": "Normal",
  "Body": { "BodyType": "HTML", "Value": "<html>…</html>", "IsTruncated": false },
  "ReminderIsSet": true, "ReminderMinutesBeforeStart": 15, "HasAttachments": true, "LastModifiedTime": "…Z",
  "Start": "…Z", "End": "…Z", "IsAllDayEvent": false, "FreeBusyType": "Busy", "IsMeeting": true, "IsCancelled": false,
  "CalendarItemType": "Single", "ResponseType": "Accept",
  "Organizer": { "Mailbox": { "Name": "…", "EmailAddress": "…", "RoutingType": "SMTP", "MailboxType": "Mailbox" } },
  "RequiredAttendees": [ { "Mailbox": { "Name": "…", "EmailAddress": "…", "RoutingType": "SMTP", "MailboxType": "Mailbox" }, "ResponseType": "Unknown" }, "…" ],
  "AppointmentState": 3 }
```

- `ReminderMinutesBeforeStart` — число (а не строка, как в XML EWS); клиент принимает оба вида. `RequiredAttendees`/`OptionalAttendees` отсутствуют, когда пусты. У встречи, где пользователь не организатор, ответы участников все `Unknown`: чужие ответы сервер показывает только организатору.
- `item:Attachments` в явной форме не запрашивается (в контракте панели события вложений нет). `AllProperties` (проба `aiowa calendar --probe`) отдаёт сверх того `Attachments[]` (с `ContentId`/`IsInline` у картинок тела), `Body`, усечённое до `MaximumBodySize` (`IsTruncated`, `UTF8BodySize`), `IsOrganizer`, `IsRecurring`, `IsOnlineMeeting`, `JoinOnlineMeetingUrl`, `TimeZone`, `StartTimeZoneId`/`EndTimeZoneId` (например, `Russian Standard Time`), `StartWallClock`/`EndWallClock` (со смещением пояса), `UID`, `DateTimeStamp`, `AppointmentSequenceNumber`, `IntendedFreeBusyStatus`, `ResponseObjects`, `EffectiveRights`; у экземпляра — ещё `RecurrenceId`, `OriginalStart`, `IsSeriesCancelled`.
- **У экземпляра нет `Recurrence`** ни в явной форме, ни в `AllProperties` (как в EWS: образец хранится у «мастера» серии). Клиент достаёт его вторым `GetItem` по `RecurringMasterItemId` — сервер принял:

```json
{ "__type": "GetItemRequest:#Exchange",
  "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly",
      "AdditionalProperties": [ { "__type": "PropertyUri:#Exchange", "FieldURI": "calendar:Recurrence" } ] },
  "ItemIds": [ { "__type": "RecurringMasterItemId:#Exchange", "OccurrenceId": "<ItemId.Id экземпляра>", "ChangeKey": "<его ChangeKey>" } ],
  "ShapeName": null }
```

Ответ 773 байта, 178 мс, `Items[0]` =

```json
{ "__type": "CalendarItem:#Exchange", "ItemId": { "Id": "<мастера>", "ChangeKey": "…" }, "IsAllDayEvent": true,
  "Recurrence": { "RecurrencePattern": { "__type": "AbsoluteYearlyRecurrence:#Exchange", "DayOfMonth": 6, "Month": "May" },
                  "RecurrenceRange": { "__type": "NoEndRecurrence:#Exchange", "StartDate": "1977-05-06+00:00" } } }
```

`DayOfMonth`/`Interval` — числа, `Month`/`DaysOfWeek` — строки (`DaysOfWeek` через пробел, как в EWS; массив клиент тоже принимает), даты диапазона — `YYYY-MM-DD+00:00` (клиент берёт первые десять символов). Текст для панели собирает клиент (`protocol::recurrence_text`): «Ежегодно 6 мая, с 06.05.1977», «Каждую неделю: пн, ср, с 05.01.2026 по 30.06.2026», «Каждые 2 недели: пт, …, всего 10 раз». Еженедельные и ежемесячные образцы на сервере не наблюдались — их формы взяты по схеме EWS и покрыты юнит-тестами. Отказ сервера по этому второму запросу не фатален: подробности отдаются без текста повторения.

Хранилище (схема v4): `calendar_items` — экземпляры с `UNIQUE(account_id, server_id)`, моменты в миллисекундах UTC, тело zstd, `details_at` после `GetItem`; `calendar_ranges` — загруженные окна. Окно заменяется целиком (`Store::replace_calendar_range`): кэшированные экземпляры, пересекающие окно и не пришедшие с сервера, удаляются; подробности сохраняются, пока у экземпляра не сменился `ChangeKey`. CLI: `aiowa calendar --from ГГГГ-ММ-ДД --to ГГГГ-ММ-ДД [--details N|recurring|meeting] [--probe] [--offline]` печатает только счётчики, флаги и даты.

## 5h. Подписки на уведомления: `SubscribeToNotification` и поток `ev.owa2` (клиент `aiowa`, 23.10.2026)

Снято тремя запусками `aiowa notify` (только чтение и подписки, каждый запуск с `logoff`): 90 с с подпиской на «Входящие», 150 с на «Входящие» и «Отправленные», 7 минут на «Входящие» для наблюдения конца цикла. Форма запроса взята из захвата браузерной OWA (`fixtures/raw/owa-capture-shapes-2026-10-16.json`, записи с `action=SubscribeToNotification`) и принята сервером с первого раза; запасные формы клиента (`FolderId` объектом, без `__type`) не понадобились. Поток описан в § 4.

**`SubscribeToNotification`** — OWA-специфичный конверт (§ 3.2): без `__type` и `Body` на верхнем уровне, объект `request` несёт только `Header` с `RequestServerVersion: Exchange2013`, параметры рядом с ним. Клиент шлёт его как обычный `service.svc?action=SubscribeToNotification&ID=<n>&AC=1` с теми же заголовками (`Action`, `X-OWA-CANARY`, `Content-Type`), JSON в теле:

```json
{ "request": { "__type": "NotificationSubscribeJsonRequest:#Exchange",
      "Header": { "__type": "JsonRequestHeaders:#Exchange", "RequestServerVersion": "Exchange2013",
                  "TimeZoneContext": { "__type": "TimeZoneContext:#Exchange",
                                       "TimeZoneDefinition": { "__type": "TimeZoneDefinitionType:#Exchange", "Id": "UTC" } } } },
  "subscriptionData": [
    { "__type": "SubscriptionData:#Exchange", "SubscriptionId": "<guid клиента>",
      "Parameters": { "__type": "SubscriptionParameters:#Exchange", "NotificationType": "RowNotification",
                      "FolderId": "<FolderId.Id строкой, без ChangeKey>", "subscriptionIdSuffix": "" } },
    { "__type": "SubscriptionData:#Exchange", "SubscriptionId": "<guid>",
      "Parameters": { "__type": "SubscriptionParameters:#Exchange", "NotificationType": "NewMailNotification", "subscriptionIdSuffix": "" } } ] }
```

- `FolderId` в параметрах — **строка**, а не объект `{__type, Id}` (так шлёт браузерная OWA; тип `SubscriptionParameters` на сервере хранит его строкой). Несколько подписок уходят одним запросом.
- Ответ HTTP 200 (747 байт на пять подписок, 876 на шесть, 290–310 мс) — **массив без конверта `Header`/`Body`**, по элементу на подписку, в порядке запроса:

  ```json
  [ { "SubscriptionId": "<тот же guid>", "SuccessfullyCreated": true, "SubscriptionExists": false, "ErrorInfo": null }, "…" ]
  ```

  `SubscriptionExists: true` приходит при повторной подписке с тем же идентификатором (клиент считает её созданной). Отказ по типу — `SuccessfullyCreated: false` с текстом в `ErrorInfo`; HTTP-ошибки на этих формах не наблюдалось.

| `NotificationType` | Результат | Примечание |
|---|---|---|
| `RowNotification` (с `FolderId`) | создана | для «Входящих» и «Отправленных»; одна подписка на папку |
| `NewMailNotification` | создана | без `FolderId` |
| `HierarchyNotification` | создана | без `FolderId` |
| `CalendarItemNotification` (с `FolderId` календаря) | создана | идентификатор папки `calendar` из локальной базы |
| `UnseenItemNotification` | **отказ** | `ErrorInfo: "SubscribeToUnseenCountNotification is only supported through Broker not in OwaMapiNotificationManager."` — как и `SuiteNotification`, требует Broker, которого на этом сервере нет; счётчик непрочитанных клиент берёт из базы после синхронизации |

Подписки живут в серверной сессии: после повторного входа их нет, клиент (`OwaTransport`) очищает список при каждом входе и первым событием следующего открытого потока отдаёт `Reload`, по которому вызывающий подписывается заново. Поток открывается вне семафора транспорта: синхронизация и команды окна идут параллельно с ним. Вид события клиент определяет по `id` (`SubscriptionId`) из своего списка подписок, а для неизвестного идентификатора — по `__type`/`EventType` объекта.

**Поток `PendingNotificationRequest`**: формы блоков и длина цикла в § 4. За 90, 150 и 420 с наблюдения (последний запуск — два цикла: первый закрыт сервером на 320-й секунде, второй открыт клиентом после `FinishNotificationRequest`) в ящике ничего не происходило, поэтому в потоке были только служебные блоки (`alive1`, `noerr`, `update` каждые 40 с, `restart`/`alive0` в конце цикла); **форма массивов событий `[{"EventType": …, "id": "<SubscriptionId>", …}]` на этом сервере пока не снята** (клиент не создаёт и не меняет письма в проверках). Разбор событий в клиенте терпим к форме: `EventType` строкой или числом, `FolderId`/`ItemId` строкой или объектом `{Id, ChangeKey}`, отправитель новой почты строкой или объектом `{Name, EmailAddress}`/`{Mailbox}`, тема `Subject`, превью `PreviewText` или `Preview`; ключи первого реального события попадут в журнал отладки (`канал: событие`, только имена ключей) для дополнения этого раздела.

## 6. Что это значит для клиента

1. Вход: POST формы, затем `sessiondata.ashx` даёт профиль, дерево папок и первую страницу входящих.
2. Все операции чтения — EWS-схема в JSON; документацию EWS можно использовать как спецификацию полей, имена типов строятся по правилу `<Имя>:#Exchange`.
3. Именованные `ShapeName` (`ItemPart`, `Conversation*ListView`) дают готовые наборы полей без перечисления свойств; `UniqueBody` приходит в `ItemPart`.
4. `SyncState` у `GetConversationItems` работает инкрементально; `SyncFolderItems` для папок проверить отдельно.
5. Истечение сессии: HTTP 440 или 401 на любом запросе → повторный вход через форму.
6. Клиент должен слать заголовки конверта (как минимум `Action`, `X-OWA-CANARY`, `X-Requested-With`, `Content-Type`); какие из остальных обязательны, выясняется в спайке убавлением по одному.
