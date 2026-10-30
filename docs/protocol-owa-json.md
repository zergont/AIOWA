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

### 2.1. Чей ящик: `SessionSettings` (клиент `aiowa mailbox --probe`, 25.10.2026)

Снято командой `aiowa mailbox --probe` (только чтение, `logoff` в конце; печатаются имена ключей, типы и длины строк, без значений). Запрос — как выше: пустое тело с `Content-Length: 0`, заголовки `X-OWA-CorrelationId`, `client-request-id`, `X-Requested-With: XMLHttpRequest`; канарейка не нужна. Ответ — три раздела `owaUserConfig`, `findFolders`, `findMailFolderItem`.

`owaUserConfig` — 29 ключей: `AdMarket`, `ApplicationSettings`, `AsyncSendEnabled`, `AttachmentPolicy`, `CanaryTokenForOutlookExport`, `FlightConfiguration`, `GroupsSets`, `HexCID`, `InlineExploreContent`, `IsConsumerChild`, `LeanPopoutEnabled`, `LinkPreviewEnabled`, `MailTipsLargeAudienceThreshold`, `MasterCategoryList`, `MaxRecipientsPerMessage`, `MobileDevicePolicySettings`, `PolicySettings`, `PolicyTipsEnabled`, `PublicComputersDetectionEnabled`, `RecoverDeletedItemsEnabled`, `RetentionPolicyTags`, `RmsV2UIEnabled`, `SafetyUserOptions`, `SegmentationSettings`, `SessionSettings`, `SmimeAdminSettings`, `UnblockUnsafeSenderPromptEnabled`, `UserOptions`, `ViewStateConfiguration`.

`owaUserConfig.SessionSettings` — плоский объект; для личности ящика нужны:

| Ключ | Тип (длина на сервере автора) | Что это |
|---|---|---|
| `UserEmailAddress` | строка (19) | основной SMTP-адрес ящика; клиент берёт его первым |
| `LogonEmailAddress` | строка (19) | адрес входа (у своего ящика тот же); запасной |
| `MailboxGuid` | строка (36) | GUID ящика: 8-4-4-4-12, с дефисами, без скобок; клиент хранит в нижнем регистре |
| `UserDisplayName` | строка (29) | отображаемое имя |
| `UserPrincipalName` | строка (22) | UPN учётной записи (не адрес почты) |
| `UserProxyAddresses` | массив (3) | все адреса ящика (`SMTP:`/`smtp:`…) |
| `UserLegacyExchangeDN` | строка (66) | X500-адрес |

Остальные ключи `SessionSettings`: `ArchiveDisplayName`, `BposCompanyName`, `CanActAsOwner`, `CompanyName`, `ConnectedAccountInfos` (массив), `DefaultFolderIds` и `DefaultFolderNames` (массивы по 197), `DefaultFromAddress`, `DefaultPublicFolderMailbox`, `EncryptedUserPuid`, `ExternalDirectoryTenantGuid`, `ExternalDirectoryUserGuid` (пустые на локальном Exchange), `HasArchive`, `HelpUrl`, `IsBposUser`, `IsExplicitLogon`, `IsExplicitLogonOthersMailbox`, `IsGallatin`, `IsMigratedConsumer`, `IsPremiumConsumerMailbox`, `IsProsumerConsumerMailbox`, `IsPublicComputerSession`, `IsPublicLogon`, `IsRequireProtectedPlayOnPhone`, `IsSPOnPrem`, `IsShadowMailbox`, `IsSubstrateSearchServiceAvailable`, `IsUMEnabled`, `IsUserCultureRightToLeft`, `IsUserCultureSpeechEnabled`, `MaxMessageSizeInKb`, `NotManagedEmailAddresses`, `OnlineBookingUserVoiceUrl`, `OrganizationDomain`, `PeopleConnectionsExist`, `PlayOnPhoneDialString`, `QuotaSend`, `QuotaUsed`, `QuotaWarning`, `SharePointTitle`, `SharePointUrl`, `TenantDomain`, `TenantGuid` (36 — GUID организации, не ящика), `UserCulture`, `UserLanguage`, `UserPuid`, `UserSipUri`, `UserVoiceId`, `UserVoiceUrl`.

**Запасной путь — `GetOwaUserConfiguration`.** `POST service.svc?action=GetOwaUserConfiguration` с заголовками обычной операции (§ 3.1, `Action`, `X-OWA-CANARY`) и телом `{}` — без конверта, параметров у операции нет — сервер принимает. Ответ — сам объект `owaUserConfig` (те же 29 ключей в корне, `SessionSettings` той же формы), без `Header`/`Body`. Клиент (`OwaTransport::mailbox_info`) идёт сюда, только если `sessiondata.ashx` не ответил или в нём нет адреса и GUID.

Зачем клиенту: одна база на ящик при любой форме логина (`user`, `user@domain`, `DOMAIN\user`). Окно записывает `MailboxGuid` и адрес в базу и при каждом входе сверяет их (`apps/desktop/src-tauri/src/mailbox.rs`); мастер ищет на диске базу этого ящика, новая база заводится в каталоге по адресу ящика.

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
| `FindItem` | при старте, календарь, напоминания | форма с `CalendarPageView` снята клиентом, § 5g; последние письма папки (`IndexedPageView`, сортировка по дате) — § 5m |
| `GetCalendarView` | календарь, листание месяца | `Body {CalendarId: TargetFolderId{BaseFolderId: FolderId{Id, ChangeKey}}, RangeStart, RangeEnd}` (локальное время без зоны, зона в `Header.TimeZoneContext`), `RequestServerVersion: V2017_08_18`; ответ `Body {ResponseClass, ResponseCode, Items[]}` |
| `GetCalendarFolders`, `GetReminders` | старт, календарь | формы не сняты |
| `FindPeople` | старт (избранные контакты), автодополнение | по справочнику (`directory`) снято клиентом, § 5l: записи в `Body.ResultSet` |
| `SubscribeToNotification` | старт и смена папки | `subscriptionData[{SubscriptionId, Parameters{NotificationType:"RowNotification", FolderId}}]`; ответ `[{SubscriptionId, SuccessfullyCreated, SubscriptionExists, ErrorInfo}]`. Типы: `RowNotification` (строки папки), `SuiteNotification` (не поддерживается без Broker), календарные. Точная форма и проверенные клиентом типы — § 5h, поток событий — § 4 |
| `UpdateUserConfiguration` | OWA сама пишет настройки (`FrequentlyUsedFolders`) | операция записи, клиенту не нужна |
| `GetExtensibilityContext` | старт | в ответе `EwsUrl: https://{host}/EWS/Exchange.asmx` и `RestUrl` — EWS доступен как резервный канал |
| `GetPresence`, `ResetPresence`, `InstantMessageSignIn` | Lync | клиенту не нужны |
| `GetFavorites`, `GetAllowedOptions`, `GetUserVoiceData`, `GetUnseenLightningIds` | старт | второстепенны |

Не наблюдалось в этой сессии (проверяется в Rust-спайке напрямую): `SyncFolderItems`, `SyncFolderHierarchy`, `MarkAllItemsAsRead`, `CreateItem`, `UpdateItem`, `GetAttachment`. Позже сняты клиентом: `SyncFolderItems`/`SyncFolderHierarchy`/`GetItem` (§ 5b'), `GetFileAttachment` (§ 5), `UpdateItem` (§ 5e), `CreateItem`/`CreateAttachment`/`SendItem`/`GetUserConfiguration` (§ 5f), `FindItem` с `CalendarView` и `GetItem` события (§ 5g), `CreateFolder`/`UpdateFolder`/`DeleteFolder` и `message:ReplyTo` поверх `ShapeName` (§ 5j), `FindPeople`/`ResolveNames` по справочнику (§ 5l; `ExpandDL` в форме EWS сервер отвергает), `FindItem` последних писем папки (§ 5m).

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

  `Id` письма не меняется (152 символа, как в базе). `ChangeKey` в ответе **короткий, 8 символов**, тогда как `SyncFolderItems`/`GetItem` отдают ключи по 40 символов: OWA JSON кладёт в ответ `UpdateItem` усечённую форму ключа. Второй вызов, отправленный с этим 8-символьным ключом, прошёл и вернул тот же 8-символьный ключ; ключ недельной давности из синхронизации с `AlwaysOverwrite` сервер тоже принял. Клиент записывает ключ из ответа в базу (`update_item_id`). **Поправка 30.10.2026:** это ошибка. Усечённый ключ — один тип элемента без номера версии (`CQAAAA==`, у полных ключей начало `CQAAABYA`). После пометки прочитанным `SyncFolderItems` присылает только `ReadFlagChange`, полного ключа не возвращает. `ReferenceItemId` ответа и пересылки с усечённым ключом сервер отвергает: `ErrorStaleObject` «The current ChangeKey is required for this operation» (docs/obkatka.md п. 19). Ключ из ответа `UpdateItem` для `ReferenceItemId` непригоден. Для `MoveItem` и `DeleteItem` ключ не нужен. **Клиент с 30.10.2026** ключ из ответа `UpdateItem` не записывает (хранилище не заменяет полный ключ усечённым), а перед `ReferenceItemId` перечитывает ключ оригинала запросом § 5n. Отказ по элементу приходит тем же сообщением с `ResponseClass: Error` и `ResponseCode` (например, `ErrorItemNotFound`) — в этой проверке не наблюдался.

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

**HTTP 500 при переносе писем из разных папок (снято 30.10.2026, живая проверка 0.2.1).** `MoveToDeletedItems` двух писем одной папки (два из «Входящих», два из «Отправленных») — успех, `done=2`. Две копии одного письма самому себе — из «Входящих» и из «Отправленных» — одним запросом: **HTTP 500 «Внутренняя ошибка сервера. Не удается выполнить операцию»**, дважды из двух; при этом сервер перенос **выполнил** (следующий `SyncFolderItems`: `Delete` в обеих папках, оба письма в «Удалённых»). Окно в таком случае откатывает оптимистичный перенос и показывает ошибку, а синхронизация следом всё же убирает письма. Какая из причин — разные папки или копии одного письма — не разделено. Запрос с 0.2.0 не менялся; встречается только при удалении одним действием писем из разных папок (выбор в результатах поиска). **Клиент после 0.2.1** шлёт все записи (`DeleteItem`, `MoveItem`, `UpdateItem`) пачками писем одной исходной папки (`write.rs`: `by_folder`, `batches`); проверено тем же случаем — обе копии письма себе одним действием: `done=2`, без отказа.

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
- **Место приходит только по свойству `calendar:EnhancedLocation`** (24.10.2026): по одному `calendar:Location` член `Location` не появлялся ни в `FindItem`, ни в `GetItem`, ни в `AllProperties` даже у событий с заполненным `PidLidLocation` (§ 5i) — строковое свойство EWS в JSON-контракт не попадает, член `Location` типизирован объектом `EnhancedLocation`. С `calendar:EnhancedLocation` в `AdditionalProperties` `FindItem` года вернул `Location` у всех 30 элементов — объект с ключами `DisplayName`, `IdType`, `LocationType`, `PostalAddress`; `DisplayName` непустой у 21 (ровно у тех же 21, у которых непустой `PidLidLocation`, запрошенный расширенным свойством в контрольном запросе). Явный `GetItem` с тем же свойством тоже отдаёт `Location`; `AllProperties` — нет. Клиент запрашивает оба свойства и принимает и строку, и объект с `DisplayName` (`protocol::location_of`).
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

## 5i. Календарь: создание, изменение, удаление события и письма о встречах (клиент `aiowa`, 24.10.2026)

Снято двумя запусками `aiowa calendar-check --yes-i-know` (одна сессия каждый: `CreateItem` → `FindItem` дня → `UpdateItem` → `FindItem` → `DeleteItem HardDelete` → `FindItem`; событие «[AIOWA] проверка календаря» без участников на завтра 09:00–09:30, напоминание 15 мин, занятость Tentative) и одним запуском `aiowa meeting-info --item-pk … --probe` (только `GetItem` по трём старым письмам о встречах: приглашение, отмена, ответ участника). Все операции — обычный `service.svc`, `__type` первым ключом, `RequestServerVersion: V2017_08_18`, `TimeZoneContext` = `UTC`. Ни одной рассылки участникам не было (участников нет, `SendToNone`). Вторая серия (24.10.2026, вечер): `aiowa calendar-check --date 2026-10-26 --all-day --probe --yes-i-know` с временным флагом подбора формы места — то же событие на весь день **в поясе пользователя** (`TimeZoneContext` этого запроса = `Russian Standard Time` из `OWA.UserOptions`), без места; после каждой записи `GetItem AllProperties` + расширенное свойство `PidLidLocation`; подбор формы места через `UpdateItem`; `HardDelete`. Четыре записи на сервер (`CreateItem`, два `UpdateItem`, `DeleteItem`), `FindItem` дня 2026-10-26 после проверки пуст. Чтение места проверено отдельной сессией только чтения (см. «Чтение места»).

**`CreateItem` события** — принят с первого раза (219–759 мс, ответ 574 байта):

```json
{ "__type": "CreateItemRequest:#Exchange",
  "SendMeetingInvitations": "SendToNone",
  "SavedItemFolderId": { "__type": "TargetFolderId:#Exchange",
      "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "calendar" } },
  "Items": [ { "__type": "CalendarItem:#Exchange",
      "Subject": "…",
      "Body": { "__type": "BodyContentType:#Exchange", "BodyType": "HTML", "Value": "<div>…</div>" },
      "Start": "2026-10-25T06:00:00Z", "End": "2026-10-25T06:30:00Z", "IsAllDayEvent": false,
      "Location": { "__type": "EnhancedLocation:#Exchange", "DisplayName": "…" },
      "RequiredAttendees": [ { "__type": "AttendeeType:#Exchange",
          "Mailbox": { "__type": "EmailAddress:#Exchange", "Name": "…", "EmailAddress": "…", "RoutingType": "SMTP" } } ],
      "OptionalAttendees": [ "…" ],
      "ReminderIsSet": true, "ReminderMinutesBeforeStart": 15,
      "FreeBusyType": "Tentative", "Sensitivity": "Normal" } ] }
```

- `SendMeetingInvitations`: `SendToAllAndSaveCopy` при участниках (тогда сервер сам рассылает приглашения), иначе `SendToNone`. Пустые списки участников, пустое тело и пустое место не передаются; `ReminderIsSet: false` без минут — без напоминания.
- **Часовой пояс пользователя.** В первой серии контекст запросов был `UTC`, и созданное событие получило `TimeZone`/`StartTimeZoneId`/`EndTimeZoneId = UTC` (видно в `GetItem AllProperties`): для обычного события момент верный, но пояс «чужой», а событие на весь день с полуночами `…T00:00:00Z` в OWA (пояс пользователя — Москва) съезжало бы на день. Браузерная OWA шлёт запросы в поясе пользователя, и клиент теперь делает так же для `CreateItem`/`UpdateItem` событий: пояс берётся из словаря `OWA.UserOptions` (§ 5f), ключ **`timezone`** (регистр смешанный, сравнение без учёта регистра), значение — Windows-идентификатор (`Russian Standard Time`; `MailTransport::user_time_zone`, кэш в `sync_state` под ключом `desktop.owa_timezone` на сутки — по истечении пояс перечитывается, и смена пояса в настройках OWA подхватывается без чистки базы), и подставляется в `Header.TimeZoneContext.TimeZoneDefinition.Id` **только этих запросов** (`OwaTransport::call_in_zone`; чтение календаря и всё остальное — по-прежнему `UTC`, даты ответов с `Z`). У события на весь день `Start`/`End` при этом — **местные полуночи календарных дат без зоны**: `"Start": "2026-10-26T00:00:00", "End": "2026-10-27T00:00:00", "IsAllDayEvent": true`; у обычного события — моменты UTC с `Z`, как раньше (сервер переводит их в пояс контекста сам). Проверено: `CreateItem` принят (354 мс), `GetItem AllProperties` созданного события — `"IsAllDayEvent": true, "Start": "2026-10-25T21:00:00Z", "End": "2026-10-26T21:00:00Z", "StartTimeZoneId": "Russian Standard Time", "EndTimeZoneId": "Russian Standard Time", "TimeZone": "(UTC+03:00) Москва, Санкт-Петербург", "StartWallClock": "2026-10-26T00:00:00+03:00", "EndWallClock": "2026-10-27T00:00:00+03:00"`; `FindItem` дня вернул событие как «весь день» с теми же моментами `…T21:00:00Z` (как у чужих всё-день событий, § 5g). В клиенте границы «весь день» по-прежнему кодируются полуночами UTC календарных дат (`aiowa_core::all_day_bound_utc`: так дата хранится моментом независимо от пояса машины), а транспорт при наличии пояса превращает их в дату (`all_day_civil_date`, ближайшая полночь UTC) и шлёт строкой без зоны; без пояса (в OWA не задан или не получен) — как раньше, полуночи UTC с `Z`, и событие ляжет в пояс UTC. При правке времени события на весь день вместе со `Start`/`End` обязательно идёт `IsAllDayEvent: true` — по нему транспорт выбирает форму границ; сравнение с кэшем — по датам (`all_day_date`), чтобы не переписывать чужие полуночи при смене темы.
- Занятость идёт под именем члена OWA JSON **`FreeBusyType`** (так сервер отдаёт `calendar:LegacyFreeBusyStatus` в ответах, § 5g; сериализация DataContract симметрична) — проверено: созданное с `Tentative` событие вернулось из `FindItem` с `FreeBusyType: "Tentative"`.
- Ответ HTTP 200, `Body.ResponseMessages.Items[0]` = `{ "__type": "ItemInfoResponseMessage:#Exchange", "ResponseCode": "NoError", "ResponseClass": "Success", "Items": [ { "__type": "CalendarItem:#Exchange", "ItemId": { "Id": "<152 символа>", "ChangeKey": "<40>" }, "IsAllDayEvent": false } ] }`. Следом `FindItem` окна дня (§ 5g) подтвердил время, занятость и напоминание.
- Место в `CreateItem` — тот же объект `EnhancedLocation` с одним `DisplayName`, что и в `UpdateItem` (см. «Место» ниже). Объект в форме браузерной OWA 2016 — с `"Annotation": ""` и `"PostalAddress": { "__type": "PersonaPostalAddress:#Exchange", "LocationSource": "None" }` — сервер в `CreateItem` **не десериализует**: HTTP 500 `Cannot deserialize object of type CreateItemJsonRequest` (24.10.2026, событие не создано).

**`FindItem` с `CalendarView`** теперь запрашивает ещё `item:ReminderIsSet` и `item:ReminderMinutesBeforeStart` — принято, в ответе появились `"ReminderIsSet": true, "ReminderMinutesBeforeStart": 15` (число) у каждого элемента; клиент пишет напоминание в кэш (`calendar_items.reminder_minutes`) при каждой загрузке окна, не дожидаясь `GetItem`. Из `CALENDAR_DETAIL_FIELDS` эти два поля ушли (они уже в общем списке).

**`UpdateItem` события** — форма, принятая сервером (237 мс, ответ 606 байт):

```json
{ "__type": "UpdateItemRequest:#Exchange",
  "ConflictResolution": "AlwaysOverwrite",
  "SendCalendarInvitationsOrCancellations": "SendToNone",
  "ItemChanges": [ { "__type": "ItemChange:#Exchange",
      "ItemId": { "__type": "ItemId:#Exchange", "Id": "…", "ChangeKey": "…" },
      "Updates": [
        { "__type": "SetItemField:#Exchange", "Path": { "__type": "PropertyUri:#Exchange", "FieldURI": "calendar:Start" },
          "Item": { "__type": "CalendarItem:#Exchange", "Start": "2026-10-25T07:00:00Z" } },
        { "__type": "SetItemField:#Exchange", "Path": { "__type": "PropertyUri:#Exchange", "FieldURI": "calendar:End" },
          "Item": { "__type": "CalendarItem:#Exchange", "End": "2026-10-25T07:45:00Z" } } ] } ] }
```

- Член рассылки называется в OWA JSON **`SendCalendarInvitationsOrCancellations`** (`SendToNone` | `SendToAllAndSaveCopy`), а не `SendMeetingInvitationsOrCancellations`, как атрибут в схеме EWS: имя схемы сервер молча игнорирует (неизвестные члены DataContract пропускает) и отвечает `ResponseClass: Error`, `ErrorSendMeetingInvitationsOrCancellationsRequired` «The SendMeetingInvitationsOrCancellations attribute is required for calendar items.» (433 байта); добавление `MessageDisposition: SaveOnly` не помогает. С правильным именем — `UpdateItemResponseMessage:#Exchange`, ключи `__type, ResponseCode, ResponseClass, Items, ConflictResults`, `Items[0].ItemId` с **полным `ChangeKey` (40 символов)** — в отличие от писем (§ 5e), где ключ в ответе усечён до 8. `FindItem` после этого подтвердил новое время (10:00–10:45), напоминание сохранилось.
- Остальные поля по той же схеме: `item:Subject` (`Subject`), `calendar:IsAllDayEvent`, `item:Body` (`Body: BodyContentType`), `calendar:RequiredAttendees`/`OptionalAttendees` (списки `AttendeeType`; опустевший — `DeleteItemField`), `item:ReminderIsSet` + `item:ReminderMinutesBeforeStart` (включить) или `item:ReminderIsSet: false` (выключить), `calendar:LegacyFreeBusyStatus` со значением под ключом `FreeBusyType`. Эти поля на сервере не проверялись (покрыты юнит-тестом точной формы).
- **Место.** Пробы `UpdateItem` на том же событии (первая серия — 1–3, вторая — 4–5; после каждой — `GetItem`):
  1. `Path calendar:Location`, `Item: { …, "Location": "строка" }` → **HTTP 500** `Cannot deserialize object of type UpdateItemJsonRequest` (та же ошибка валила весь первый запуск, где место шло вместе со временем): член `Location` у `CalendarItem` в OWA JSON — не строка.
  2. `Path calendar:EnhancedLocation`, `Item: { …, "EnhancedLocation": { "__type": "EnhancedLocation:#Exchange", "DisplayName": "…" } }` → `ErrorIncorrectUpdatePropertyCount` «An object within a change description must contain one and only one property to modify» (830 байт): члена `EnhancedLocation` сервер не знает, объект оказался без свойств.
  3. `Path calendar:Location`, `Item: { …, "Location": { "__type": "EnhancedLocation:#Exchange", "DisplayName": "…" } }` → десериализовано, но `ErrorUpdatePropertyMismatch` «Property for update does not match property in object» (745 байт): член `Location` (объект `EnhancedLocation`) соответствует свойству **`calendar:EnhancedLocation`**, а не `calendar:Location`.
  4. **`Path calendar:EnhancedLocation`, `Item: { "__type": "CalendarItem:#Exchange", "Location": { "__type": "EnhancedLocation:#Exchange", "DisplayName": "…" } }` → `Ok` (349 мс), и место сохранено**: `GetItem` с расширенным свойством `PidLidLocation` (набор `Appointment`, `PropertyId` 33288 = 0x8208, `String`) вернул `"ExtendedProperty": [ { "ExtendedFieldURI": { "DistinguishedPropertySetId": "Appointment", "PropertyId": 33288, "PropertyType": "String" }, "Value": "<то самое место>" } ]`. Ранний вывод «место не сохраняется» был ошибкой чтения: **`GetItem AllProperties` не отдаёт ключ `Location` вовсе**, даже когда `PidLidLocation` заполнен (см. ниже про чтение).
  5. Снятие места той же формой с `"DisplayName": ""` → `Ok` (3,4 с — заметно дольше остальных записей), `PidLidLocation` после этого — пустая строка (`"Value": ""`), в `FindItem` места нет.

  Форма клиента (`protocol::location_json`, `LOCATION_FIELD = calendar:EnhancedLocation`): `SetItemField` с `Path calendar:EnhancedLocation` и объектом из одного `DisplayName` под членом `Location`; снятие — то же с пустым `DisplayName` (так делает браузерная OWA), без `DeleteItemField`. В `CreateItem` член `Location` — тот же объект. Не пробовались (бюджет записей исчерпан): объект с `PostalAddress` только в `UpdateItem` (в `CreateItem` он даёт HTTP 500, см. выше), `Path calendar:Location` с `RequestServerVersion: V2016_02_03`, запись через `ExtendedPropertyUri PidLidLocation` — в них нет нужды.

  **Чтение места.** В ответах `FindItem`/`GetItem` члена `Location` не было ни у одного события (§ 5g), хотя у своего тестового события `PidLidLocation` заполнен: свойство `calendar:Location` (строка EWS) в JSON-контракт `CalendarItem` не попадает — его член `Location` типизирован как объект `EnhancedLocation` и заполняется только свойством **`calendar:EnhancedLocation`**, а `AllProperties` его не включает. Клиент запрашивает `calendar:EnhancedLocation` вместе с `calendar:Location` в `CALENDAR_FIELDS` (`FindItem` и явный `GetItem`) и в `MEETING_MESSAGE_FIELDS`; разбор принимает и строку, и объект с `DisplayName` (`protocol::location_of`). Проверено одной сессией только чтения (`aiowa calendar --from 2026-01-01 --to 2027-01-01 --details meeting --probe` плюс `GetItem` двух писем о встречах): `FindItem` года принял свойство (228 мс, 30 событий, у 21 место — столько же, сколько с непустым `PidLidLocation` в контрольном запросе с расширенным свойством; объект `Location` с ключами `DisplayName`, `IdType`, `LocationType`, `PostalAddress`), явный `GetItem` встречи вернул место (14 символов), `AllProperties` того же события — по-прежнему без `Location`; у приглашения и отмены (`MeetingRequestMessageType`, `MeetingCancellationMessageType`) явная форма с этим свойством принята и место пришло (17 и 11 символов), в `AllProperties` писем его тоже нет.

**`DeleteItem` события** (HardDelete 222–272 мс, ответ 279 байт):

```json
{ "__type": "DeleteItemRequest:#Exchange",
  "ItemIds": [ { "__type": "ItemId:#Exchange", "Id": "…", "ChangeKey": "…" } ],
  "DeleteType": "HardDelete", "SendMeetingCancellations": "SendToNone" }
```

Ответ `DeleteItemResponseMessage:#Exchange` с ключами `__type, ResponseCode, ResponseClass` (без `Items`); `FindItem` окна дня после удаления пуст. `SendMeetingCancellations` сервер читает под этим именем (без него EWS отвечает `ErrorSendMeetingCancellationsRequired`); `SendToAllAndSaveCopy` рассылает отмену участникам — у организатора; для приглашённого (не организатора) тот же `DeleteItem` с `SendToAllAndSaveCopy` по семантике EWS — не отмена, а ответ-отказ организатору, поэтому клиент (окно, `calendar_delete`, CLI) у чужих встреч шлёт только `SendToNone`. Трейтовый `delete_calendar_item` шлёт `MoveToDeletedItems` (на сервере не выполнялось; CLI и проверка удаляют навсегда через `OwaTransport::delete_calendar_item_with`).

**Ответ на приглашение** — `CreateItem` с `MessageDisposition: "SendAndSaveCopy"` (ответ уходит организатору) или `"SaveOnly"` (без ответа) и элементом `{ "__type": "AcceptItem:#Exchange" | "TentativelyAcceptItem:#Exchange" | "DeclineItem:#Exchange", "ReferenceItemId": { ItemId }, "Body": { BodyContentType }? }`; `ReferenceItemId` — письмо-приглашение или событие календаря. **На сервере не выполнялось** (чужие приглашения не трогались), покрыто юнит-тестом формы.

**`GetItem` письма о встрече** — явная форма `IdOnly` принята для всех трёх классов (1,0–1,6 КБ, 195–302 мс):

```json
{ "__type": "GetItemRequest:#Exchange",
  "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly", "BodyType": "HTML",
      "FilterHtmlContent": true, "BlockExternalImagesIfSenderUntrusted": true, "MaximumBodySize": 4096,
      "AdditionalProperties": [ "item:ItemClass, item:Subject, item:DateTimeReceived, message:From, calendar:Start, calendar:End, calendar:IsAllDayEvent,",
          "calendar:Location, calendar:Organizer, calendar:MyResponseType, calendar:CalendarItemType, calendar:Recurrence,",
          "meeting:IsOutOfDate, meeting:AssociatedCalendarItemId, meeting:ResponseType, meeting:HasBeenProcessed, meeting:IsDelegated" ] },
  "ItemIds": [ { "__type": "ItemId:#Exchange", "Id": "…", "ChangeKey": "…" } ],
  "ShapeName": null }
```

| Класс письма | `__type` элемента | Ключи ответа (явная форма) | `ResponseType` |
|---|---|---|---|
| `IPM.Schedule.Meeting.Request` | `MeetingRequestMessageType:#Exchange` | `ItemId, ItemClass, Subject, DateTimeReceived, From, AssociatedCalendarItemId, ResponseType, Start, End, CalendarItemType, Organizer` | `Organizer` (свой ответ: на проверенном письме владелец — организатор) |
| `IPM.Schedule.Meeting.Canceled` | `MeetingCancellationMessageType:#Exchange` | `ItemId, ItemClass, Subject, DateTimeReceived, From, ResponseType, Start, End, CalendarItemType` | `NoResponseReceived` |
| `IPM.Schedule.Meeting.Resp.Pos` | `MeetingResponseMessageType:#Exchange` | `ItemId, ItemClass, Subject, DateTimeReceived, From, AssociatedCalendarItemId, ResponseType, Start, End, CalendarItemType` | `Accept` (ответ отправителя) |

- `ResponseType` — одно имя на два смысла: у приглашения это свой ответ (`calendar:MyResponseType`, как у событий в § 5g), у письма-ответа — ответ отправителя; клиент различает по `ItemClass` (`…Resp.Pos`/`Neg`/`Tent` дают ответ и без поля). `MyResponseType`, `IsOutOfDate`, `HasBeenProcessed`, `IsDelegated`, `IsAllDayEvent`, `Recurrence` в ответах не пришли ни в явной форме, ни в `AllProperties` — булевы, похоже, опускаются при `false` (у проверенных писем встречи не устаревали), повторения у них нет; клиент читает их терпимо (отсутствие = `false`/нет). `Location` по `calendar:Location` не приходит и у писем о встречах — нужно `calendar:EnhancedLocation` (теперь в `MEETING_MESSAGE_FIELDS`; проверено 24.10.2026 на приглашении и отмене: место пришло объектом `EnhancedLocation`, см. «Чтение места» выше). `Organizer` есть только у приглашения; у отмены организатор берётся из `From`, а у письма-ответа `From` — отвечающий участник, и организатор остаётся пустым (окно строку «Организатор» не показывает). `AssociatedCalendarItemId` (Id 152, ChangeKey 40) есть у приглашения и ответа, у отмены отсутствует (событие уже убрано из календаря).
- `AllProperties` (4,5–7,7 КБ, 200–414 мс) отдаёт сверх того обычные поля письма (`Body`, `Sender`, `ToRecipients`/`CcRecipients`, `DisplayTo`, `ConversationId`, `Flag`, `IsRead`, `EffectiveRights`, …) и поля встречи: `UID`, `DateTimeStamp`, `IsOrganizer`, `ReminderIsSet`/`ReminderMinutesBeforeStart`/`ReminderDueBy`/`ReminderNextTime`, `ResponseObjects` (3 элемента с `__type`), а у приглашения ещё `MeetingRequestType`, `IntendedFreeBusyStatus`, `IsMeeting`, `IsCancelled`, `IsRecurring`, `MeetingRequestWasSent`, `RequiredAttendees`/`OptionalAttendees` (с `ResponseType` у каждого), `ConflictingMeetingCount`, `AdjacentMeetingCount`, `Duration`, `TimeZone`, `AppointmentSequenceNumber`, `AppointmentState`, `ConferenceType`, `IsOnlineMeeting`, `StartWallClock`/`EndWallClock`, `StartTimeZoneId`/`EndTimeZoneId`. Клиент при отказе явной формы (HTTP 500 или ошибка по свойству) переходит на `AllProperties` и запоминает форму на сессию (`OwaTransport::meeting_request_item`).

Хранилище: `calendar_items.reminder_minutes` заполняется из `FindItem`; `Store::upsert_calendar_event` (после `CreateItem`/`UpdateItem`, чтобы кэш не ждал перечитывания окна), `remove_calendar_event` (после `DeleteItem`), `calendar_events_with_reminders(account, from_ms, to_ms)` — события с напоминанием (не отменённые, не отклонённые), у которых момент `start_at − reminder_minutes` лежит в окне; закрытые и отложенные напоминания — в `sync_state` под ключами `desktop.reminder.<server_id>` со значениями `dismissed:<момент напоминания>` или `snoozed_until:<мс>:<момент напоминания>` (`reminder_state`/`set_reminder_state`/`reminder_states`/`prune_reminder_states`; состояние действует только для того момента напоминания, при котором записано, — перенос события или смена напоминания взводят его заново); пояс пользователя — в `sync_state` под ключом `desktop.owa_timezone` (`aiowa_store::OWA_TIME_ZONE_KEY`, `Store::owa_time_zone`/`set_owa_time_zone`; окно кладёт его при обращении к календарю или записи, CLI — при команде записи, когда записи нет или она старше суток (`OWA_TIME_ZONE_TTL_MS`); «в OWA пояс не задан» кэшируется пустой строкой на тот же срок, чтобы не спрашивать сервер на каждом окне; при отказе сервера остаётся устаревший пояс из кэша); `ui::MessageHead.item_class` — чтобы окно узнавало письма о встречах. CLI: `aiowa calendar-create`, `calendar-update --calendar-pk`, `calendar-delete --calendar-pk [--permanent]`, `calendar-check [--date] [--all-day] [--probe]` (вся живая проверка одной сессией, четыре записи: создать с местом → сменить место и время или тему → снять место → удалить навсегда; с `--probe` после каждой записи `GetItem AllProperties` + `PidLidLocation` своего события), `meeting-info --item-pk a,b,c [--probe]` (только чтение); все команды записи печатают план и без `--yes-i-know` ничего не делают. Подбор формы места 24.10.2026 делался временным флагом `calendar-check --location-forms` (формы по одной через сырой `UpdateItem`, принятая форма оказалась первой же — текущей), после проверки флаг убран.

## 5j. Папки: `CreateFolder`, `UpdateFolder`, `DeleteFolder` и поле `Reply-To` письма (клиент `aiowa`, 24.10.2026)

Снято одним запуском `aiowa folder-check --probe-reply-to 25 --yes-i-know` на рабочем ящике (одна сессия, `logoff` в конце): `SyncFolderHierarchy` (инкрементально, по состоянию из базы) → `CreateFolder` «AIOWA тест» в корне → `SyncFolderHierarchy` → `UpdateFolder` «AIOWA тест 2» → `SyncFolderHierarchy` → `DeleteFolder HardDelete` → `SyncFolderHierarchy` → один `GetItem` по 25 свежим письмам «Входящих» (только чтение) → `get_items` по пяти из них. Три записи на сервер, все по папкам; письма не трогались. Все операции — обычный `service.svc`, `__type` первым ключом, `RequestServerVersion: V2017_08_18` (как у операций записи писем § 5e; запасная `Exchange2013` в клиенте не понадобилась). Ответы 180–350 мс.

**`CreateFolder`** (184 мс, ответ 519 байт):

```json
{ "__type": "CreateFolderRequest:#Exchange",
  "ParentFolderId": { "__type": "TargetFolderId:#Exchange",
      "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "msgfolderroot" } },
  "Folders": [ { "__type": "Folder:#Exchange", "DisplayName": "AIOWA тест", "FolderClass": "IPF.Note" } ] }
```

`BaseFolderId` для подпапки — `{ "__type": "FolderId:#Exchange", "Id": "…" }` (форма та же, что у `MoveItem`). Ответ `Body.ResponseMessages.Items[0]`:

```json
{ "__type": "FolderInfoResponseMessage:#Exchange", "ResponseCode": "NoError", "ResponseClass": "Success",
  "Folders": [ { "__type": "Folder:#Exchange", "FolderId": { "Id": "<120 символов>", "ChangeKey": "<40 символов>" } } ] }
```

`FolderId.Id` папки — 120 символов (у писем 152), `ChangeKey` полный, 40. Следующий инкрементальный `SyncFolderHierarchy` (228 мс, 2 802 байта) вернул ровно одно изменение — `Create` с тем же `Id` и `DisplayName` «AIOWA тест». Клиент сразу после ответа вставляет строку в `folders` (`Store::insert_folder`: `server_id` из ответа, `parent_server_id` родителя — для корня это `Store::msgfolderroot_server_id`, родитель «Входящих», так как сам корень в иерархии не приходит; класс `IPF.Note`, счётчики нули), а синхронизация иерархии потом обновляет ту же строку по `UNIQUE(account_id, server_id)`. Отказ ожидается тем же сообщением с `ResponseClass: Error` (например, `ErrorFolderExists` при занятом имени в том же родителе) — в проверке не наблюдался.

**`UpdateFolder`** (переименование, 348 мс, ответ 519 байт):

```json
{ "__type": "UpdateFolderRequest:#Exchange",
  "FolderChanges": [ { "__type": "FolderChange:#Exchange",
      "FolderId": { "__type": "FolderId:#Exchange", "Id": "…", "ChangeKey": "…" },
      "Updates": [ { "__type": "SetFolderField:#Exchange",
          "Path": { "__type": "PropertyUri:#Exchange", "FieldURI": "folder:DisplayName" },
          "Folder": { "__type": "Folder:#Exchange", "DisplayName": "AIOWA тест 2" } } ] } ] }
```

Ответ — тот же `FolderInfoResponseMessage` с `Folders[0].FolderId`: **`Id` тот же** (120 символов), `ChangeKey` новый, полный (40 символов, без усечения до 8, как у `UpdateItem` писем). `SyncFolderHierarchy` после этого (168 мс) вернул одно изменение `Update` с новым `DisplayName`. Клиент правит имя и ключ локально (`Store::rename_folder_local`).

**`DeleteFolder`** (`HardDelete`, 179 мс, ответ 232 байта):

```json
{ "__type": "DeleteFolderRequest:#Exchange",
  "FolderIds": [ { "__type": "FolderId:#Exchange", "Id": "…", "ChangeKey": "…" } ],
  "DeleteType": "HardDelete" }
```

Сообщение ответа **без `__type`** и без `Folders`: `{ "ResponseCode": "NoError", "ResponseClass": "Success" }` (как у `SendItem` § 5f). `SyncFolderHierarchy` после удаления (168 мс, 2 276 байт) вернул одно изменение `Delete` с `FolderId` папки. `DeleteType: "MoveToDeletedItems"` — папка становится подпапкой «Удалённых» вместе с содержимым; **на сервере не проверялось** (бюджет записей — одна тестовая папка), форма по схеме EWS покрыта юнит-тестом. Клиент при `HardDelete` помечает папку, подпапки и их письма удалёнными (`Store::delete_folder_local(pk, None)`: письма снимаются с индекса, агрегаты бесед пересчитываются), при `MoveToDeletedItems` — только меняет родителя папки на «Удалённые» (`delete_folder_local(pk, Some(trash_pk))`), письма остаются в ней; следующая синхронизация иерархии сверяет. Служебные папки (`distinguished`) клиент не переименовывает и не удаляет; имя — 1–255 символов без управляющих и `\ / : * ? " < > |` (`aiowa_core::validate_folder_name`).

**`MoveFolder`** (для перетаскивания папки) реализован по схеме EWS — `{ "__type": "MoveFolderRequest:#Exchange", "ToFolderId": { TargetFolderId{BaseFolderId} }, "FolderIds": [ FolderId ] }`, ответ ожидается `FolderInfoResponseMessage` с `Folders[0].FolderId` — **на сервере не выполнялся**, покрыт юнит-тестом формы.

Разбор ответов (`protocol::parse_written_folder_id`): идентификатор из `Folders[0].FolderId`; его нет — у `UpdateFolder`/`MoveFolder` остаётся исходный. Версия схемы подбирается один раз на сессию (`OwaTransport::folder_call`): `V2017_08_18`, при HTTP 500 на форму — `Exchange2013`. CLI: `aiowa folder-create --name … [--parent …]`, `folder-rename --folder-pk … --name …`, `folder-delete --folder-pk … [--permanent]`, `folder-check [--probe-reply-to N]` (вся проверка одной сессией); все команды записи без `--yes-i-know` только печатают план, после записи выполняют инкрементальный `SyncFolderHierarchy` и печатают, упомянута ли папка в изменениях.

**`Reply-To` письма.** В именованных формах `GetItem` (`ItemPart`, § 5b') поля `ReplyTo` нет. Клиент добавляет к `ItemShape` формы `AdditionalProperties: [ { "__type": "PropertyUri:#Exchange", "FieldURI": "message:ReplyTo" } ]` рядом с `ShapeName: "ItemPart"` — **сервер принимает `AdditionalProperties` поверх `ShapeName`**: `GetItem` по 25 письмам прошёл (16 с первым запросом свежей сессии после долгой паузы, 197 КБ; следующий по пяти письмам — 277 мс), у элементов те же 43 ключа `ItemPart`, у трёх из 25 — ещё ключ `ReplyTo`:

```json
"ReplyTo": [ { "Name": "…", "EmailAddress": "…", "RoutingType": "SMTP", "MailboxType": "OneOff", "Submitted": false, "RelevanceScore": 0 } ]
```

Массив адресов той же формы, что получатели (`EmailAddress`-подобный объект с `MailboxType`, плюс поля OWA `Submitted` и `RelevanceScore`). **Без заголовка `Reply-To` ключа `ReplyTo` в элементе нет** (поле опускается, пустой массив не приходит). У двух из трёх писем `Reply-To` совпадал с `From` (рассылки, подставляющие себя же), у одного — отличался; клиент заменяет отправителя в «Кому» ответа только при отличии (`aiowa_core::effective_reply_to`, сравнение адресов без учёта регистра). Отказ сервера по форме с `message:ReplyTo` — HTTP 500 на запрос или `ErrorInvalidProperty*`/`*Shape*` в **каждом** `ResponseMessage` при HTTP 200 (так OWA JSON отвергает форму по элементам; смешанные отказы вроде `ErrorItemNotFound` формой не считаются) — клиент обрабатывает повтором пачки без свойства и запоминает отказ на сессию (`OwaTransport::get_item_chunk`, предикат `protocol::all_property_rejections`); на этом сервере не понадобилось. У формы списка `MailListItem` свойство не запрашивается.

Хранилище: `Reply-To` лежит в `recipients` с **`kind = 4`** (`aiowa_core::RecipientKind::ReplyTo`) рядом с «Кому» (1), «Копией» (2) и «Скрытой копией» (3): миграции схемы не нужно (идемпотентно по построению), представление индекса `items_fts_src` берёт только `kind IN (1, 2)`, так что адрес `Reply-To` не ищется оператором `кому:` и не считается получателем; `Store::recipients` отдаёт его строкой с `kind = 4`, `ui::MessageHead.reply_to` и `MessageFull.reply_to` — списком участников. CLI `aiowa reply-to-probe [--folder …] [--limit N]` — та же проверка без записи.

## 5k. Черновики: `UpdateItem`, `DeleteAttachment`, `GetItem` черновика (клиент `aiowa`, 24.10.2026)

Проверено одной сессией `aiowa draft-check --sync-drafts --yes-i-know` на тестовом черновике «[AIOWA] черновик» без отправки; черновик в конце удалён навсегда.

| Шаг | Время | Результат |
|---|---|---|
| `CreateItem` черновика без получателей (§ 5f) | 216 мс | `ItemId` 152 символа, `ChangeKey` 40 |
| `CreateAttachment` текстового файла 47 байт | 178 мс | `RootItemId` тот же, `RootItemChangeKey` новый |
| `GetItem` черновика (`AllProperties`) | 167 мс | тема, тело, 0 получателей, 1 файловое вложение |
| `UpdateItem` темы, тела и «Кому» | 208 мс | `Id` тот же, `ChangeKey` новый |
| `GetItem` после обновления | 166 мс | тема новая, «Кому» 1, вложение на месте |
| `SyncFolderItems` «Черновиков» + тела + вложения | 1,6 с | строка в базе: `is_draft`, получатель, вложение скачано и проиндексировано |
| `DeleteAttachment` | 173 мс | `RootItemId` тот же, `RootItemChangeKey` новый |
| `GetItem` после удаления вложения | 159 мс | вложений 0 |
| `DeleteItem` `HardDelete` | 159 мс | `GetItem` по нему — `ErrorItemNotFound`; синхронизация — одно `Delete` |

**`UpdateItem` черновика** (`protocol::update_draft_body`): `ConflictResolution: AlwaysOverwrite`, **`MessageDisposition: SaveOnly`** (обязателен для писем, § 5e), `SuppressReadReceipts: true`, по `SetItemField` на `item:Subject`, `item:Body` (`BodyContentType` HTML) и `message:ToRecipients` / `message:CcRecipients` / `message:BccRecipients`; пустой список снимается `DeleteItemField` по тому же пути (иначе сервер оставил бы прежних получателей). `item:Importance` пишется, только когда важность не `Normal` (то же правило, что у ответов): выбора важности в композере нет, и обычная важность значит «не трогать» — иначе важность, заданная черновику в OWA, сбрасывалась бы при каждом сохранении. `UpdateItem` заменяет тело целиком: цитату оригинала у ответа сервер добавляет только при `CreateItem` с `ReplyToItem`, поэтому клиент при обновлении черновика ответа или пересылки дописывает цитату сам (`compose.rs`, `quote_html`). Тело оригинала для неё берётся **с сервера** (`GetItem` с `ShapeName: ItemNormalizedBody`, § 5b'), а не из базы: синхронизация хранит только `UniqueBody`, и цитата из него потеряла бы историю переписки. Ссылки `cid:` в цитате остаются как есть (без подстановки `data:`): они указывают на инлайновые вложения, которые сервер скопировал в черновик при `CreateItem`. Нет полного тела или оно обрезано — сохранение черновика отказывает, а не уходит без цитаты. **Не проверено на сервере:** копирует ли `ReplyToItem` инлайновые картинки оригинала в черновик ответа с теми же `ContentId`.

**`DeleteAttachment`** (`protocol::delete_attachment_body`): `{ "__type": "DeleteAttachmentRequest:#Exchange", "AttachmentIds": [ { "__type": "AttachmentId:#Exchange", "Id": "…" } ] }` — несколько вложений одним запросом, по сообщению ответа на каждое в порядке запроса; у каждого `RootItemId` с `RootItemId` и `RootItemChangeKey` письма-родителя (разбор терпим и к форме `{ Id, ChangeKey }`). Новый ключ нужен следующему `SendItem`.

**`GetItem` черновика** (`protocol::get_draft_body`): основная форма — `BaseShape: AllProperties`, `BodyType: HTML`, `MaximumBodySize`, `MaximumRecipientsToReturn: 500`, `ShapeName: null`; сервер вернул 47 ключей с полным телом в члене **`Body`** (не `UniqueBody`), получателями и `Attachments`. Запасная форма — именованная `ItemNormalizedBody` (§ 5b'), не понадобилась. Клиент берёт черновик с сервера только когда локального тела нет (черновик создан в OWA и ещё не синхронизирован). `IsTruncated` тела переносится в `DraftContent::is_truncated`: обрезанный черновик композер не открывает (сохранение записало бы обрезанное тело обратно).

**Инлайновые картинки в черновике ответа.** Проверено черновиком `ReplyToItem` (без отправки, затем удалён навсегда) на письмо с одной картинкой `cid:` в теле: сервер **копирует инлайновые вложения оригинала в черновик ответа с теми же `ContentId`** (после синхронизации «Черновиков» у черновика одно инлайновое вложение, `ContentId` совпал, картинка подставилась в тело). Поэтому клиентская цитата при `UpdateItem` черновика ответа ссылается на `cid:` оригинала и картинки остаются на месте; цитата берётся из полного тела оригинала (`GetItem` формой `ItemNormalizedBody`, кэш в `ComposeState`), без полного или с обрезанным телом обновление черновика ответа отказывает, а не уходит без истории.

«Черновики» входят в набор синхронизации по умолчанию (`drafts` убран из `SERVICE_FOLDERS`): черновики со вложениями проходят обычный путь `SyncFolderItems` → `GetItem` тел → вложения, строка получает `is_draft = 1`.

## 5l. Справочник организации: `FindPeople`, `ResolveNames`, `ExpandDL` (клиент `aiowa`, 24.10.2026)

Снято тремя запусками `aiowa directory` на рабочем ящике (только чтение, каждый — одна сессия с `logoff`). Запросы — фамилия владельца ящика (слово имени из «Отправленных» локальной базы, 7 символов; выбрано по совпадению латиницы с адресом) и её первые три буквы; списки рассылки искались запросами FindPeople по общим словам и среди частых получателей локальной базы. В вывод не печатались значения: только ключи, счётчики, коды, длины строк и длительности. Все операции — обычный `service.svc`, `__type` первым ключом, `RequestServerVersion: V2017_08_18`; ответы 160–330 мс.

| Запрос | Время | Ответ | Итог |
|---|---|---|---|
| `FindPeople` форма `Default`, фамилия, max 25 | 160–200 мс | 2,8 КБ | 2 записи |
| `FindPeople` форма `Default`, три буквы | 161–164 мс | 5,2 КБ | 4 записи |
| `FindPeople` с `AdditionalProperties` (по одному, вместе) | 157–171 мс | — | см. таблицу свойств |
| `ResolveNames` фамилия, `AllProperties` | 196–325 мс | 7,3 КБ | 2 записи, `Success`/`NoError` |
| `ResolveNames` фамилия, без данных контакта | 162–163 мс | 0,7 КБ | 2 записи (только `Mailbox`) |
| `ResolveNames` три буквы, `AllProperties` / `Default` | 204–218 / 173 мс | 19,4 / 5,4 КБ | 4 записи |
| `ResolveNames` по SMTP-адресу записи (как карточка контакта) | 180–204 мс | — | 1 запись, все поля |
| `ResolveNames` по адресу кандидата в списки | 158–196 мс | — | список (`MailboxType` `PublicDL` или `PrivateDL`) — 12-й из кандидатов, у остальных `Mailbox` или 0 записей |
| `ExpandDL` по адресу списка | 318 мс | — | **HTTP 500** (обе версии схемы) |

**`FindPeople` по справочнику** (`protocol::find_people_body`):

```json
{ "__type": "FindPeopleRequest:#Exchange",
  "IndexedPageItemView": { "__type": "IndexedPageView:#Exchange", "BasePoint": "Beginning", "Offset": 0, "MaxEntriesReturned": 25 },
  "QueryString": "…",
  "ParentFolderId": { "__type": "TargetFolderId:#Exchange",
      "BaseFolderId": { "__type": "DistinguishedFolderId:#Exchange", "Id": "directory" } },
  "PersonaShape": { "__type": "PersonaResponseShape:#Exchange", "BaseShape": "Default" },
  "ShouldResolveOneOffEmailAddress": false, "SearchPeopleSuggestionIndex": false }
```

Ответ **без `ResponseMessages`** (как у `FindConversation`): `Body { ResponseCode, ResponseClass, FirstLoadedRowIndex, FirstMatchingRowIndex, ResultSet[], TotalNumberOfFavoritesInView, TotalNumberOfPeopleInView }`. Записи лежат в **`ResultSet`** (не `People`); **`TotalNumberOfPeopleInView` равен 0 и при непустом `ResultSet`** — на счётчик полагаться нельзя. Поиск идёт по началу слов имени: три буквы фамилии нашли те же записи, что `ResolveNames` по ним же. Запрос по общему слову без совпадений — `Success` с пустым `ResultSet`.

Запись (`Persona`) формы `Default` — 27 ключей: `PersonaId { Id }`, **`PersonaTypeString`** (`Person`; ключа `PersonaType` нет), `CreationTimeString`, `DisplayName`, `DisplayNameFirstLast`, `DisplayNameLastFirst`, `FileAs` (пусто), `GivenName`, `Surname`, `CompanyName`, `EmailAddress { Name, EmailAddress, RoutingType: "SMTP", MailboxType: "Mailbox" }`, `EmailAddresses [ { Name, EmailAddress, RoutingType, MailboxType } ]`, `ImAddress`, `WorkCity`, `RelevanceScore` (число), `ADObjectId` (guid), `MapiSendRichInfo` (признак) и всегда `null`: `ItemLinkIds`, `IsBusinessContact`, `AttributedIsBusinessContact`, `SourceMailboxGuids`, `LastContactedDate`, `MapiEntryId`, `MapiEmailAddress`, `MapiAddressType`, `MapiSearchKey`, `MapiTransmittableDisplayName`. **Должности, отдела, офиса и телефонов в форме `Default` нет.**

`AdditionalProperties` в `PersonaShape` (`[ { "__type": "PropertyUri:#Exchange", "FieldURI": "persona:…" } ]`, проверено по одному на трёх буквах):

| Свойство | Итог | Ключ в записи |
|---|---|---|
| `persona:Title` | принято | `Title` — строка, у всех 4 записей |
| `persona:Department` | **отказ** `ErrorInvalidArgument` в `Body` (HTTP 200) | — |
| `persona:OfficeLocations` | принято | `OfficeLocationsArray: [ { Value, Attributions: [ "…" ] } ]` |
| `persona:BusinessPhoneNumbers` | принято | `BusinessPhoneNumbersArray: [ { Value: { Number, Type: "Business", NormalizedNumber }, Attributions } ]` |
| `persona:MobilePhones` | принято | ключа нет (у найденных мобильных нет); ожидается `MobilePhonesArray` той же формы |
| `persona:Alias`, `persona:PersonaType` | приняты | новых ключей нет |

Все шесть принятых вместе — принято (171 мс), три новых ключа у всех записей. Клиент поэтому ищет с `AdditionalProperties` = `persona:Title`, `persona:OfficeLocations`, `persona:BusinessPhoneNumbers`, `persona:MobilePhones` (`protocol::PERSONA_EXTRA_FIELDS`, `find_people_body_with`); именно эта четвёрка отдельно не отправлялась (это подмножество принятой шестёрки). Отказ по свойствам (`ErrorInvalidArgument` или код свойства в `Body`, HTTP 500) — повтор формой `Default`, отказ запоминается в транспорте (`OwaTransport::find_people_value`). Отдела у записей `FindPeople` нет: его даёт `ResolveNames` (карточка контакта кладёт полную запись в кэш, а слияние в кэше не стирает отдел при следующем поиске).

Разбор (`protocol::parse_persona`, терпимый): массив — `ResultSet`, затем `People`, `Personas`, первое сообщение `ResponseMessages`; адрес — `EmailAddress.EmailAddress`, при адресе `EX` — первый SMTP из `EmailAddresses`; вид — `PersonaType`/`PersonaTypeString` (`DistributionList` — список, `Room`/`Equipment` — ресурс), иначе `EmailAddress.MailboxType` (`PublicDL`/`PrivateDL` — список, `Contact`/`OneOff` — внешний), иначе человек с адресом — ящик; поля — строкой, атрибутированным `{ Value }` или массивом таких значений (`*Array`). `WorkCity` не считается офисом. Записи без имени и адреса пропускаются. Списков рассылки и переговорных в выдаче `FindPeople` на этом ящике не встретилось: значения `PersonaTypeString` для них (ожидаются `DistributionList`, `Room`) не проверены.

**`ResolveNames`** (`protocol::resolve_names_body`; без полных данных — `ReturnFullContactData: false` и без `ContactDataShape`):

```json
{ "__type": "ResolveNamesRequest:#Exchange", "UnresolvedEntry": "…",
  "ReturnFullContactData": true, "SearchScope": "ActiveDirectory", "ContactDataShape": "AllProperties" }
```

Ответ — обычный `ResponseMessages`, сообщение `{ "__type": "ResolveNamesResponseMessage:#Exchange", ResponseCode, ResponseClass, ResolutionSet: { IncludesLastItemInRange, TotalItemsInView, Resolutions: [ { Mailbox, Contact } ] } }`. **Несколько совпадений (2 и 4) пришли как `Success`/`NoError`**, а не `Warning`/`ErrorNameResolutionMultipleResults`, как описывает EWS; клиент принимает обе формы, `ErrorNameResolutionNoResults` — пустой список (`protocol::parse_resolve_names`). `Mailbox { Name, EmailAddress, RoutingType: "SMTP", MailboxType: "Mailbox" }`.

`Contact` с `AllProperties` — 24 ключа: `Alias`, `AssistantName`, `CompanyName`, `ContactSource` (`ActiveDirectory`), `Culture`, `Department`, `DirectReports` (у части: массив адресов `{ Name, EmailAddress, RoutingType, MailboxType }`), `DirectoryId`, `DisplayName`, `EmailAddresses`, `GivenName`, `HasPicture`, `Initials`, `JobTitle`, `MSExchangeCertificate`, `Manager` (строка — имя), `ManagerMailbox`, `Notes`, `OfficeLocation`, `PhoneNumbers`, `Photo`, `PhysicalAddresses`, `Surname`, `UserSMIMECertificate`. Словари — массивами пар:

```json
"PhoneNumbers":   [ { "Key": "AssistantPhone", "PhoneNumber": "" }, … ],
"EmailAddresses": [ { "Key": "EmailAddress1", "EmailAddress": "…" }, … ],
"PhysicalAddresses": [ { "Key": "Business", "Street": "", "City": "…", "State": "", "CountryOrRegion": "…", "PostalCode": "" } ]
```

`PhoneNumbers` — всегда шесть видов (`AssistantPhone`, `BusinessFax`, `BusinessPhone`, `HomePhone`, `MobilePhone`, `Pager`), пустой — пустой строкой: **значение под ключом `PhoneNumber`, а не `Value`**. Заполненность на 4 записях: `BusinessPhone` 4, `MobilePhone` и остальные 0; `JobTitle`, `Department`, `CompanyName`, `OfficeLocation`, `Alias` — у всех, `Manager` — у трёх, `Notes` — ни у кого. `EmailAddresses` — три пары; значения длиннее адреса (с префиксом вида адреса), клиент берёт из них только `SMTP:` (не `SIP:`/`X500:`), и только если адрес `Mailbox` не SMTP. Большая часть объёма `AllProperties` — сертификаты и фото: та же выдача с **`ContactDataShape: "Default"`** — 5,4 КБ против 19,4 КБ, 15 ключей (нет `Alias`, `Notes`, `Photo`, `HasPicture`, `DirectoryId`, `DirectReports`, `ManagerMailbox`, сертификатов), должность, отдел, офис и телефоны на месте (`protocol::resolve_names_body_shaped`). Карточка контакта запрашивает одну запись, поэтому клиент оставляет `AllProperties`.

Разбор (`protocol::parse_resolution`): имя — `Contact.DisplayName`, иначе `Mailbox.Name`; вид — по `Mailbox.MailboxType`; должность `JobTitle`, отдел `Department`, компания `CompanyName`, офис `OfficeLocation`, телефоны — `PhoneNumbers` по ключам `BusinessPhone`/`MobilePhone` (пустые пропускаются), псевдоним `Alias`.

**`ExpandDL`** (`protocol::expand_dl_body`):

```json
{ "__type": "ExpandDLRequest:#Exchange",
  "Mailbox": { "__type": "EmailAddress:#Exchange", "EmailAddress": "…", "RoutingType": "SMTP" } }
```

По адресу списка (`ResolveNames` по нему дал одну запись с `MailboxType` списка) сервер ответил **HTTP 500 и на `V2017_08_18`, и на повтор с `Exchange2013`** (318 мс на оба). Текст отказа (`FaultMessage`) в том запуске не печатался; теперь `aiowa directory --expand first` печатает его с маской на адресах. **Форма `ExpandDL` для OWA JSON не найдена — открытый вопрос**: трейтовый `expand_dl` отдаёт `Protocol` (HTTP 500), интерфейс должен показать ошибку. Ожидаемый ответ по схеме EWS — сообщение с `DLExpansion { Mailbox: [ { Name, EmailAddress, RoutingType, MailboxType } ], TotalItemsInView, IncludesLastItemInRange }`; разбор (`protocol::parse_expand_dl`) терпим к `Mailboxes`/`Items` и покрыт юнит-тестом.

**Кэш справочника** (схема версии 5, `crates/aiowa-store/src/directory.rs`): таблица `directory_people (account_id, email PK — SMTP в нижнем регистре, display_name, kind, json, fetched_at)`, `kind` — `mailbox`/`list`/`contact`/`room`/`other`, остальные поля — объектом JSON. `Store::upsert_directory_people` сливает запись со старой (заполненные поля новой побеждают, пустые берутся из старой: выдача `FindPeople` без отдела не стирает отдел из `ResolveNames`), `replace_directory_person` заменяет целиком; `directory_person` — по адресу без учёта регистра; `directory_search_cached` — по началу слов имени и адреса без учёта регистра и ё (0,2–0,8 мс). Миграция добавляет и индексы `items(account_id, lower(from_email))` и `recipients(lower(email))` для счётчиков переписки карточки (`Store::contact_local_stats`: писем от адреса, писем владельца ему из «Отправленных» или с собственного адреса, последнее); на тестовой базе (154 тыс. писем) миграция заняла 0,38 с.

CLI: `aiowa directory [--query Q] [--resolve] [--prefix3] [--expand first|АДРЕС] [--max N] [--probe-shapes] [--dl-candidates N] [--dl-query Q …]` — без `--query` ищет фамилию владельца; печатает только счётчики, ключи, коды и длительности и пишет найденное в кэш справочника.

## 5m. Последние письма папки: `FindItem` по дате по убыванию (клиент `aiowa`, 25.10.2026)

Зачем: первая синхронизация большой папки (`SyncFolderItems`, письма в порядке сервера) идёт десятки минут, и свежие письма не обязательно приходят первыми. Клиент сначала берёт последнюю страницу писем папки по дате получения и показывает её, а полную выгрузку ведёт потом. Снято командой `aiowa recent-probe --folders inbox,sentitems --limit 50` (только чтение, один вход и `logoff`, в выводе только ключи, счётчики и длины).

**Запрос** (`protocol::find_recent_items_body`, `RequestServerVersion: V2017_08_18`):

```json
{ "__type": "FindItemRequest:#Exchange",
  "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly",
                 "AdditionalProperties": [ { "__type": "PropertyUri:#Exchange", "FieldURI": "item:Subject" }, "…те же 18 свойств, что у SyncFolderItems (§ 5b')…" ] },
  "ParentFolderIds": [ { "__type": "FolderId:#Exchange", "Id": "…" } ],
  "Traversal": "Shallow",
  "Paging": { "__type": "IndexedPageView:#Exchange", "BasePoint": "Beginning", "Offset": 0, "MaxEntriesReturned": 50 },
  "SortOrder": [ { "__type": "SortResults:#Exchange", "Order": "Descending",
                   "Path": { "__type": "PropertyUri:#Exchange", "FieldURI": "item:DateTimeReceived" } } ] }
```

Сервер принял все четыре проверенные формы с одинаковой выдачей: `V2017_08_18` и `Exchange2013`; `FieldURI` сортировки с префиксом (`item:DateTimeReceived`) и без (`DateTimeReceived`); `ShapeName: "MailListItem"` вместо `AdditionalProperties`. Страница из 50 писем — 160–300 мс («Входящие» 77 267 писем, «Отправленные» 25 043).

**Ответ** — `Body.ResponseMessages.Items[0]`:

```json
{ "__type": "FindItemResponseMessage:#Exchange", "ResponseCode": "NoError", "ResponseClass": "Success", "HighlightTerms": "…",
  "RootFolder": { "IncludesLastItemInRange": false, "IndexedPagingOffset": 50, "TotalItemsInView": 77267, "Groups": "…",
                  "Items": [ { "__type": "Message:#Exchange", "ItemId": { "Id": "…", "ChangeKey": "…" }, "...": "запрошенные свойства" } ] } }
```

- `IndexedPagingOffset` — смещение **следующей** страницы (`Offset` + число писем); `TotalItemsInView` — писем в папке, `IncludesLastItemInRange: false`, пока страница не последняя.
- У каждого элемента ровно запрошенное: 18 свойств, `ItemId` и `__type` — те же ключи, что у `Create` в `SyncFolderItems`, все 18 заполнены у всех 50 писем. Разбор — тот же `protocol::parse_message_meta` (`protocol::parse_find_items`).
- **Идентификаторы те же, что у `SyncFolderItems`**: `Id` 152 символа, `ChangeKey` 40. Письма страницы, уже лежавшие в базе (копия, синхронизированная `SyncFolderItems` за несколько дней до пробы): 10 из 50 второй страницы «Входящих» и 18 из 50 первой и 50 из 50 второй страницы «Отправленных» нашлись по `Id`, у всех совпали прочитанность, флаг и папка, `ChangeKey` — у 77 из 78. Значит, `Create` полной выгрузки ложится на письмо свежей страницы обычным upsert по идентификатору, без дублей.
- Порядок строго по убыванию `DateTimeReceived` (нарушений 0), вторая страница (`Offset: 50`) продолжает первую без пересечения.
- `ShapeName: "MailListItem"` даёт 27 ключей (лишние `InstanceKey`, `ExtendedProperty`, `ReceivedOrRenewTime`, `IconIndex`, `Sensitivity`, `IsSubmitted`, `IsReadReceiptRequested`, `IsDeliveryReceiptRequested`, `DateTimeCreated`), но **без `InternetMessageId`**, который нужен базе для «усыновления» перенесённых писем. Клиент запрашивает свойства явно.

Как этим пользуется движок (`aiowa-sync`, `Engine::run`): после дерева папок у каждой непустой папки без состояния `SyncFolderItems` (в порядке плана, «Входящие» первыми) берётся страница из `SyncOptions::recent_first` писем (по умолчанию 100) и записывается в базу как обычные заголовки без состояния папки; затем идут полные выгрузки. Идентификаторы страницы хранятся в `sync_state` (`recent_pending.<folder_pk>`) и снимаются по мере того, как их присылает `SyncFolderItems`. Что осталось к концу выгрузки и всё ещё лежит в этой папке, сверяется с сервером одним `GetItem` (`MailListItem`): помечается удалённым только то, на что сервер ответил `ErrorItemNotFound` (письмо удалено или перенесено за время выгрузки — у перенесённого новый идентификатор). Не ответил — сверка повторяется при следующей синхронизации. Отказ `FindItem` синхронизацию не останавливает: папка выгружается обычным порядком.

## 5l'. Фото и оргструктура из `ResolveNames` с полными данными, GET фото (клиент, 30.10.2026)

В разрешении `ResolveNames` с `ReturnFullContactData` (`AllProperties`) у записи справочника есть
`Contact.HasPicture` и `Contact.Photo` — base64 JPEG. Проба `aiowa directory --probe-photo` по
25 адресам ящика (первая запись поиска и частые отправители домена, только чтение): фото у 9,
1 435–4 864 байт; у записей без фото ключей `Photo` и `HasPicture` нет вовсе. Клиент
(`protocol::parse_resolution_photo`, `MailTransport::resolve_names_full`) декодирует его, кладёт в
кэш справочника (`directory_people.photo`, `photo_at` — и как отметку «фото нет») и показывает как
`data:`-адрес; отдельного запроса фото нет.

Там же оргструктура: `Manager` — строка с именем (у 7 из 25), `ManagerMailbox: { "Mailbox": {
"Name", "EmailAddress", "RoutingType", "MailboxType" } }`, `DirectReports: [ { "Name",
"EmailAddress", "RoutingType", "MailboxType" } ]` (пустой массив, если подчинённых нет; у одного —
11 записей), `PhysicalAddresses: [ { "Key", "Street", "City", "State", "CountryOrRegion",
"PostalCode" } ]`, `Notes` — пустые строки. Разбор — `protocol::parse_resolution` (`manager`,
`direct_reports`, `address`, `notes` в `DirectoryPerson`; `direct_reports = None` у записи без
полных данных отличает её от «подчинённых нет»); карточка контакта показывает руководителя и
подчинённых с переходом к их карточкам, адрес и заметки.

GET фото (снято той же пробой): `GET /owa/service.svc/s/GetUserPhoto?email=<адрес>&size=HR96x96`
с cookies сессии и `X-OWA-CANARY` (в строке запроса и заголовком) → `200 image/jpeg`
(HR48x48 — 1 155 байт, столько же без `size`; HR96x96 — 2 145; HR240x240 — 6 636; 85–110 мс),
`Cache-Control: private`, без `ETag`; `UA=0` ни на что не влияет. Нет фото или нет такого адреса
— `404` с пустым телом и тем же `Cache-Control` (удобная отметка «фото нет»).
`GetPersonaPhoto?email=…&size=…` → `400 text/html` «Неправильный запрос» (ждёт, видимо,
PersonaId) — не использовать. Клиент этот GET пока не вызывает: фото приходит с записью
`ResolveNames`; GET пригодится для крупного фото (240×240) и адресов без записи — основа в
транспорте: `OwaTransport::service_get_probe` (сырой GET `service.svc/s/<op>` под семафором).

## 5n. Текущий ключ изменений: `GetItem` только с идентификаторами (клиент, 30.10.2026)

Нужен перед `ReferenceItemId` ответа, пересылки и ответа на приглашение (§ 5e: ключ из ответа
`UpdateItem` усечённый) и для починки таких ключей в базе. Форма — `GetItem` без именованной
формы и без тела, `RequestServerVersion: V2017_08_18`:

```json
{ "__type": "GetItemRequest:#Exchange",
  "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "IdOnly" },
  "ItemIds": [ { "__type": "ItemId:#Exchange", "Id": "<Id>" } ],
  "ShapeName": null }
```

`ChangeKey` в запрос не кладётся (сервер его не сверяет, а усечённый — лишний повод для отказа).
Ответ — по одному `GetItemResponseMessage` на идентификатор в порядке запроса, `Items[0].ItemId`
с **полным** ключом (40 символов); отсутствующее письмо — `ResponseClass: Error`,
`ErrorItemNotFound`. Разбирается тем же разбором, что операции записи (`parse_write_results`).
Снято 30.10.2026: пачки по 25 идентификаторов, 207 ключей за ~2 с (9 запросов), один ключ перед
ответом — ~170 мс. Транспорт: `MailTransport::refresh_item_ids`, форма `protocol::get_item_ids_body`.

## 5o. Таймауты и повторы клиента (30.10.2026)

Замеры журнала 24–28.10 (docs/obkatka.md п. 22): почти все сбои «error sending request» к серверу
почты длились ровно 10,0 с — это был `connect_timeout` клиента, а Windows повторяет SYN на 1, 3, 7
и 15 с. Теперь: `connect_timeout` 30 с, `pool_idle_timeout` 105 с (больше паузы таймеров
календаря и синхронизации, чтобы не открывать соединение заново на каждый запрос),
`tcp_keepalive` 30 с, таймаут ответа прежний (`OwaConfig::timeout`, 90 с).

Повторы (`send_retrying`): соединение не установилось (reqwest `is_connect`: DNS, отказ порта,
таймаут соединения, TLS) — запрос не ушёл, повтор до двух раз с паузой 1 и 2 с для любой операции;
обрыв после отправки — один повтор только для идемпотентных операций (`is_idempotent`: всё, кроме
`CreateItem`, `SendItem`, `CreateAttachment`, `DeleteAttachment`, `MoveItem`, `DeleteItem`,
`CopyItem`, `CreateFolder`, `DeleteFolder`, `MoveFolder`, `UpdateFolder`, `EmptyFolder`,
`SubscribeToNotification`, `UnsubscribeFromNotification`; `UpdateItem` события с рассылкой
участникам (`SendToAllAndSaveCopy`) тоже не повторяется, с поясом пользователя или без — повтор
разослал бы обновление дважды; без рассылки (`SendToNone`, `AlwaysOverwrite`) — повторяется, как
запись писем); таймаут ответа не повторяется. Вход (`auth.owa`) повторяется только при
неустановленном соединении: ушедший вход мог создать сессию. В ошибке транспорта эти случаи
различимы: `Connect` (запрос не ушёл) и `Network` (исход неизвестен). Если после обрыва после
отправки следующая попытка не соединилась, ошибка — `Network`, а не `Connect`: первая попытка могла
дойти до сервера (поправка по ревью 30.10.2026).

## 5p. Последнее действие с письмом: `IconIndex` и `PR_LAST_VERB_*` (клиент, 30.10.2026)

Для пометок «ответил» и «переслал» (docs/obkatka.md п. 20) к свойствам списка (`META_FIELDS`,
§ 5b') добавлены `item:IconIndex` и два расширенных свойства:

```json
{ "__type": "ExtendedPropertyUri:#Exchange", "PropertyTag": "0x1081", "PropertyType": "Integer" },
{ "__type": "ExtendedPropertyUri:#Exchange", "PropertyTag": "0x1082", "PropertyType": "SystemTime" }
```

Они идут в `AdditionalProperties` `SyncFolderItems` (`IdOnly`), `FindItem` последних писем (§ 5m) и
`GetItem` формы списка (`ShapeName: MailListItem` + `AdditionalProperties`). Сервер принял все три
(снято 30.10.2026: пачки `SyncFolderItems` прошли, `GetItem` по 25 писем — 638 писем за 6,2 с).
Ответ: у элемента `IconIndex` (`MailReplied`, `MailForwarded`, у непрочитанных/прочитанных без
действия — другие значения) и `ExtendedProperty: [ { "ExtendedFieldURI": { "PropertyTag": "0x1081",
"PropertyType": "Integer" }, "Value": "104" }, { … "0x1082" …, "Value": "2026-10-29T10:46:03Z" } ]`;
`Value` — строка. Коды: 102 ответ отправителю, 103 ответ всем, 104 пересылка. Разбор —
`protocol::parse_message_meta` (`last_verb`, `last_verb_at`; расширенное свойство важнее значка,
значок — запасной путь без времени). Фейковый сервер тестов принимает `ExtendedPropertyUri`, но
не отдаёт значений.

## 5q. Личные контакты: `SyncFolderItems` и `GetItem` формой `AllProperties` (клиент, 30.10.2026)

Личные контакты (docs/obkatka.md п. 8) читаются из папок класса ровно `IPF.Contact` (стандартная
`contacts` и созданные пользователем; служебные `IPF.Contact.RecipientCache`, `IPF.Contact.MOC.*` —
нет) теми же операциями, что почта, но формой `AllProperties`: именованной формы списка у
контактов нет.

Запрос `SyncFolderItems` (протокол `V2013`, `SyncState` — как у писем, § 5b'):

```json
{ "__type": "SyncFolderItemsRequest:#Exchange",
  "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "AllProperties", "BodyType": "Text" },
  "SyncFolderId": { "__type": "FolderId:#Exchange", "Id": "…" },
  "MaxChangesReturned": 100, "SyncScope": "NormalItems", "SyncState": "…" }
```

Снято 30.10.2026 с папки «Контакты» (одна запись — группа `IPM.DistList`): сервер принял форму, но
в этой операции `AllProperties` отдаёт только базовые поля элемента и три контактных. Ключи
элемента: `__type, ContactSource, ConversationId, Culture, DateTimeCreated, DateTimeReceived,
DateTimeSent, DisplayCc, DisplayName, DisplayTo, EffectiveRights, FileAs, Flag, HasAttachments,
Importance, InstanceKey, IsAssociated, IsDocumentCentricMessage, IsDocumentCentricNotification,
IsDraft, IsFromMe, IsLocked, IsResend, IsSubmitted, IsUnmodified, ItemClass, ItemId,
LastModifiedName, LastModifiedTime, ParentFolderId, ReminderIsSet, ReminderMinutesBeforeStart,
Sensitivity, Size, Subject, SupportsSideConversation, WebClientReadFormQueryString`. Ни адресов,
ни телефонов, ни участников группы. `ChangeType` (`Create`, `Update`, `Delete`) и
`IncludesLastItemInRange` — как у писем; `ReadFlagChange` у контактов не бывает.

Поэтому по созданным и изменённым клиент тут же читает `GetItem` (протокол `V2017`) той же
формой, без ключа изменений, пачками по 25 (отказ всего вызова — страница не применяется и
состояние не продвигается; отказ по одному элементу оставляет базовые поля):

```json
{ "__type": "GetItemRequest:#Exchange",
  "ItemShape": { "__type": "ItemResponseShape:#Exchange", "BaseShape": "AllProperties", "BodyType": "Text" },
  "ItemIds": [ { "__type": "ItemId:#Exchange", "Id": "…" } ], "ShapeName": null }
```

Снято по группе (126 мс): к ключам выше добавляются `Body: { "BodyType", "IsTruncated", "Value" }`,
`DisplayBcc`, `EntityExtractionResult`, `Likes`, `LikesPreview` и `Members: [ { "Key", "Mailbox":
{ "Name", "EmailAddress", "RoutingType", "MailboxType", "SipUri" }, "Status" } ]`; `__type`
элемента — `DistributionList:#Exchange`. У контакта (`IPM.Contact`, `Contact:#Exchange`) по схеме
EWS ожидаются `GivenName`, `Surname`, `MiddleName`, `Nickname`, `CompanyName`, `JobTitle`,
`Department`, `OfficeLocation`, `Birthday`, `HasPicture` и словари `EmailAddresses: [ { "Key":
"EmailAddress1", "Name", "RoutingType", "MailboxType", "EmailAddress" } ]`, `PhoneNumbers: [ {
"Key": "MobilePhone", "PhoneNumber" } ]`, `PhysicalAddresses: [ { "Key": "Business", "Street",
"City", "State", "CountryOrRegion", "PostalCode" } ]`, `ImAddresses: [ { "Key", "ImAddress" } ]` —
в ящике автора контактов-людей на 30.10.2026 нет, эта форма не снята. Разбор
(`protocol::parse_contact_item`) терпим: словари принимаются и массивами записей с `Key`, и
объектами «ключ → значение», тело — объектом или строкой, участники — `Members[].Mailbox` или
`Members[].Member.Mailbox`. Имена ключей ответа клиент пишет в журнал на уровне `debug`
(`contact_items_keys`), значения — никогда. Фейковый сервер тестов контакты не моделирует
(`SyncFolderItems` только с `IdOnly`); разбор покрыт тестами протокола и хранилища.

Не снято: `CreateItem`/`UpdateItem`/`DeleteItem` контакта и группы (запись — после HAR автора,
docs/patch2-people.md шаг 0), `FindPeople` по папке `contacts`, фото личного контакта
(`GetUserPhoto` или вложение `ContactPicture.jpg`).

## 6. Что это значит для клиента

1. Вход: POST формы, затем `sessiondata.ashx` даёт профиль, дерево папок и первую страницу входящих.
2. Все операции чтения — EWS-схема в JSON; документацию EWS можно использовать как спецификацию полей, имена типов строятся по правилу `<Имя>:#Exchange`.
3. Именованные `ShapeName` (`ItemPart`, `Conversation*ListView`) дают готовые наборы полей без перечисления свойств; `UniqueBody` приходит в `ItemPart`.
4. `SyncState` у `GetConversationItems` работает инкрементально; `SyncFolderItems` для папок проверить отдельно.
5. Истечение сессии: HTTP 440 или 401 на любом запросе → повторный вход через форму.
6. Клиент должен слать заголовки конверта (как минимум `Action`, `X-OWA-CANARY`, `X-Requested-With`, `Content-Type`); какие из остальных обязательны, выясняется в спайке убавлением по одному.
