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
| `GetItem` | при старте (несколько), открытие одиночных элементов | форма не снята |
| `FindItem` | при старте, календарь, напоминания | форма не снята |
| `GetCalendarView` | календарь, листание месяца | `Body {CalendarId: TargetFolderId{BaseFolderId: FolderId{Id, ChangeKey}}, RangeStart, RangeEnd}` (локальное время без зоны, зона в `Header.TimeZoneContext`), `RequestServerVersion: V2017_08_18`; ответ `Body {ResponseClass, ResponseCode, Items[]}` |
| `GetCalendarFolders`, `GetReminders` | старт, календарь | формы не сняты |
| `FindPeople` | старт (избранные контакты), автодополнение | форма не снята |
| `SubscribeToNotification` | старт и смена папки | `subscriptionData[{SubscriptionId, Parameters{NotificationType:"RowNotification", FolderId}}]`; ответ `[{SubscriptionId, SuccessfullyCreated, SubscriptionExists, ErrorInfo}]`. Типы: `RowNotification` (строки папки), `SuiteNotification` (не поддерживается без Broker), календарные |
| `UpdateUserConfiguration` | OWA сама пишет настройки (`FrequentlyUsedFolders`) | операция записи, клиенту не нужна |
| `GetExtensibilityContext` | старт | в ответе `EwsUrl: https://{host}/EWS/Exchange.asmx` и `RestUrl` — EWS доступен как резервный канал |
| `GetPresence`, `ResetPresence`, `InstantMessageSignIn` | Lync | клиенту не нужны |
| `GetFavorites`, `GetAllowedOptions`, `GetUserVoiceData`, `GetUnseenLightningIds` | старт | второстепенны |

Не наблюдалось в этой сессии (проверяется в Rust-спайке напрямую): `SyncFolderItems`, `SyncFolderHierarchy`, `MarkAllItemsAsRead`, `CreateItem`, `UpdateItem`, `GetAttachment`.

## 4. Канал уведомлений

Длинный опрос на `/owa/ev.owa2`:

- `POST /owa/ev.owa2?ns=PendingRequest&ev=FinishNotificationRequest&UA=0&cid=<guid>` с заголовком `X-OWA-CANARY` и пустым телом завершает предыдущий цикл;
- `GET /owa/ev.owa2?ns=PendingRequest&ev=PendingNotificationRequest&UA=0&cid=<guid>&brwnm=chrome&X-OWA-CANARY=<канарейка>&n=<токен>` держится открытым и отдаёт поток `<script>...</script>` блоков: `{id:'pg',data:'alive1',mark:'0'}` как keep-alive, `{id:'pg',data:'noerr'}`, и JSON-массивы событий `[{"EventType":"0","id":"<SubscriptionId>", ...}]` для подписок (RowNotification по папке, InstantMessageNotification и т. д.).

В наблюдении один цикл `PendingNotificationRequest` держался около 5,7 минуты, после чего клиент закрыл его `FinishNotificationRequest` (ответ `{cid, syncFnshRq}`) и открыл следующий. Канарейка в query-параметре нужна, потому что поток читается не через XHR. Для клиента на Rust достаточно `reqwest` со стримингом тела и разбором `<script>` блоков; на первом этапе можно обойтись опросом `FindConversation`/`SyncFolderItems` по таймеру.

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

## 6. Что это значит для клиента

1. Вход: POST формы, затем `sessiondata.ashx` даёт профиль, дерево папок и первую страницу входящих.
2. Все операции чтения — EWS-схема в JSON; документацию EWS можно использовать как спецификацию полей, имена типов строятся по правилу `<Имя>:#Exchange`.
3. Именованные `ShapeName` (`ItemPart`, `Conversation*ListView`) дают готовые наборы полей без перечисления свойств; `UniqueBody` приходит в `ItemPart`.
4. `SyncState` у `GetConversationItems` работает инкрементально; `SyncFolderItems` для папок проверить отдельно.
5. Истечение сессии: HTTP 440 или 401 на любом запросе → повторный вход через форму.
6. Клиент должен слать заголовки конверта (как минимум `Action`, `X-OWA-CANARY`, `X-Requested-With`, `Content-Type`); какие из остальных обязательны, выясняется в спайке убавлением по одному.
