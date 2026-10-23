//! Канал мгновенных уведомлений OWA в окне: фоновая задача держит открытым поток
//! `PendingNotificationRequest` на транспорте процесса и по событиям сервера сама запускает
//! синхронизацию затронутых папок, показывает тосты Windows о новых письмах и ведёт счётчик
//! непрочитанных в заголовке окна и на значке панели задач.
//!
//! Жизненный цикл: задача стартует после первой удачной синхронизации (транспорт уже есть) и
//! крутит цикл «подписаться → читать поток до конца цикла → `FinishNotificationRequest` → снова
//! поток». Подписки: `RowNotification` на все папки из `folders_for_sync`, плюс `Hierarchy`,
//! `NewMail`, `Unseen` и `Calendar`, если сервер их принимает (отвергнутый вид больше не просим).
//! Ошибки — экспоненциальная пауза 5 с → 10 с → … → 5 мин; мёртвая сессия сбрасывает транспорт
//! через `SyncState::invalidate_on`, следующий цикл входит заново и подписывается с нуля.
//!
//! События потока уходят в отдельную задачу-диспетчер: изменения строк папок копятся 1,5 с и
//! уходят одной синхронизацией (`SyncState::request`: если синхронизация уже идёт, папки ждут её
//! конца); новая почта — тост (не чаще раза в 5 с, при четырёх и более письмах за минуту — одна
//! сводка) и событие окну `new-mail`; календарь — событие `calendar-changed`.
//!
//! В журнал попадают только виды событий, счётчики и длительности: ни тем, ни адресов.

use crate::ondemand::{describe_transport, timed_out};
use crate::sync::SyncState;
use crate::CmdResult;
use aiowa_core::{AccountId, FolderId, ItemId, Participant};
use aiowa_store::Store;
use aiowa_transport::{new_guid, MailTransport, NotificationEvent, NotificationKind, SubscriptionRequest, TransportError};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Ключ в `sync_state`: показывать ли тосты о новых письмах (`1`/`0`; нет ключа — показывать).
const TOASTS_KEY: &str = "desktop.notify_toasts";
/// Окно объединения событий по папкам перед запуском синхронизации.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(1500);
/// Пауза после ошибки канала: от первой до предельной, удваивается.
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// Поток, продержавшийся столько, считается здоровым: пауза после следующей ошибки снова
/// минимальная, а его обрыв с ошибкой запускает догоняющую синхронизацию (пока соединение
/// было мёртвым, события могли пропасть, а таймер окна при открытом канале редкий).
const HEALTHY_STREAM: Duration = Duration::from_secs(60);
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(60);
const FINISH_TIMEOUT: Duration = Duration::from_secs(15);
/// Сколько ждать штатного конца задачи при выходе.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// Не больше одного тоста за это время.
pub(crate) const TOAST_MIN_GAP: Duration = Duration::from_secs(5);
/// Окно и порог «лавины»: столько писем за окно — вместо отдельных тостов одна сводка.
pub(crate) const BURST_WINDOW: Duration = Duration::from_secs(60);
pub(crate) const BURST_COUNT: usize = 4;
/// Предел длины текста тоста (тема и начало превью), символов.
const TOAST_BODY_CHARS: usize = 140;
/// Страховка от неожиданно большого дерева: больше подписок на строки папок не создаём.
const MAX_FOLDER_SUBSCRIPTIONS: usize = 100;
const WINDOW_LABEL: &str = "main";
const APP_TITLE: &str = "AIOWA";
/// Сторона значка-наклейки на панели задач (Windows сам уменьшит до размера наложения).
const BADGE_SIZE: u32 = 32;

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

// --- DTO ------------------------------------------------------------------------------------

/// Состояние канала для окна. То же приходит событием `notify-status` при каждом изменении
/// и ответом `notify_status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NotifyStatusDto {
    /// Поток открыт: изменения приходят от сервера сразу, таймер синхронизации — страховка.
    pub connected: bool,
    /// С какого времени канал держится; `None` — не подключён.
    pub since_ms: Option<i64>,
    pub last_event_ms: Option<i64>,
    /// Подписок, принятых сервером в текущем цикле.
    pub subscriptions: usize,
    /// Последняя ошибка канала (текст для пользователя); при ней задача переподключается с паузой.
    pub error: Option<String>,
    pub toasts_enabled: bool,
}

/// Событие окну `new-mail`. Приходит до синхронизации папки, поэтому `item_pk` пока не
/// заполняется (письма в базе ещё нет); окно находит письмо по теме после синхронизации.
#[derive(Debug, Clone, Serialize)]
pub struct NewMailDto {
    pub from: Option<Participant>,
    pub subject: Option<String>,
    pub preview: Option<String>,
    pub item_id: Option<ItemId>,
    pub item_pk: Option<i64>,
    pub folder_pk: Option<i64>,
}

// --- состояние ------------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    status: NotifyStatusDto,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
    /// `cid` открытого цикла потока: по нему при выходе шлётся `FinishNotificationRequest`.
    cid: Option<String>,
    /// Виды подписок без папки, которые сервер отверг в этом процессе: больше не просим.
    rejected: HashSet<&'static str>,
    /// `SubscriptionId` по виду подписки и папке, постоянные на время жизни процесса: подписки
    /// живут в серверной сессии, и повторная подписка после обрыва потока с тем же
    /// идентификатором возвращает `SubscriptionExists`, а с новым — дубль, по которому каждое
    /// событие приходило бы ещё раз.
    ids: HashMap<String, String>,
    /// Последнее показанное число непрочитанных: заголовок и значок не трогаем без изменений.
    shown_unread: Option<i64>,
}

impl Inner {
    /// Идентификатор подписки для вида и папки: создаётся один раз, дальше переиспользуется.
    fn subscription_id(&mut self, kind: &NotificationKind) -> String {
        let key = match kind {
            NotificationKind::FolderRows { folder } | NotificationKind::Calendar { folder } => format!("{}:{}", kind.name(), folder.id),
            _ => kind.name().to_string(),
        };
        self.ids.entry(key).or_insert_with(new_guid).clone()
    }
}

pub struct NotifyState {
    inner: Mutex<Inner>,
    /// Задача запускается один раз — после первой удачной синхронизации.
    started: AtomicBool,
    /// Приложение закрывается: задача завершает цикл и выходит.
    stopping: AtomicBool,
    stop: Notify,
    sync: Arc<SyncState>,
    store: Arc<Store>,
    account: AccountId,
}

/// Что отправитель потока передаёт диспетчеру.
enum Msg {
    /// Новый набор подписок: `SubscriptionId` → ключ папки в базе (только для строк папок).
    Subscribed(HashMap<String, i64>),
    Event(NotificationEvent),
}

/// Чем закончился один цикл «подписки → потоки».
enum CycleEnd {
    /// Приложение закрывается.
    Stop,
    /// Сервер просит переподписаться (та же сессия): без паузы.
    Resubscribe,
    /// Ошибка: пауза и новый цикл. Текст — для строки состояния.
    Failed(String),
}

impl NotifyState {
    pub fn new(store: Arc<Store>, account: AccountId, sync: Arc<SyncState>) -> Self {
        let toasts_enabled = store.get_state(account, TOASTS_KEY).ok().flatten().is_none_or(|v| v != "0");
        let status = NotifyStatusDto { toasts_enabled, ..NotifyStatusDto::default() };
        Self { inner: Mutex::new(Inner { status, ..Inner::default() }), started: AtomicBool::new(false), stopping: AtomicBool::new(false), stop: Notify::new(), sync, store, account }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn status(&self) -> NotifyStatusDto {
        self.lock().status.clone()
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Ждёт сигнала остановки. Флаг проверяется до ожидания, а `notify_one` хранит разрешение,
    /// поэтому сигнал между проверкой и ожиданием не теряется.
    async fn wait_stop(&self) {
        loop {
            if self.stopping() {
                return;
            }
            self.stop.notified().await;
        }
    }

    /// Меняет статус и рассылает его в окно.
    fn update(&self, app: &AppHandle, f: impl FnOnce(&mut NotifyStatusDto)) {
        let snapshot = {
            let mut g = self.lock();
            f(&mut g.status);
            g.status.clone()
        };
        if let Err(e) = app.emit("notify-status", snapshot) {
            warn!(error = %e, "не удалось отправить событие notify-status");
        }
    }

    /// После удачной синхронизации: обновить счётчик непрочитанных и один раз запустить канал.
    pub(crate) fn on_sync_ok(self: &Arc<Self>, app: &AppHandle) {
        self.refresh_unread(app);
        if self.stopping() || self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        let app = app.clone();
        let handle = tauri::async_runtime::spawn(async move {
            this.run(app).await;
        });
        self.lock().task = Some(handle);
        info!("канал уведомлений: задача запущена");
    }

    /// Включить или выключить тосты; состояние хранится в базе и уходит окну событием.
    pub(crate) fn set_toasts(&self, app: &AppHandle, enabled: bool) -> CmdResult<()> {
        self.store.set_state(self.account, TOASTS_KEY, Some(if enabled { "1" } else { "0" })).map_err(|e| format!("Не удалось сохранить настройку: {e}"))?;
        self.update(app, |s| s.toasts_enabled = enabled);
        Ok(())
    }

    fn toasts_enabled(&self) -> bool {
        self.lock().status.toasts_enabled
    }

    // --- задача канала ---------------------------------------------------------------------

    async fn run(self: Arc<Self>, app: AppHandle) {
        let (tx, rx) = mpsc::unbounded_channel::<Msg>();
        let reload = Arc::new(Notify::new());
        let dispatcher = {
            let this = self.clone();
            let app = app.clone();
            let reload = reload.clone();
            tauri::async_runtime::spawn(async move { Dispatcher::new(&this, &app, &reload).run(rx).await })
        };
        let mut backoff = BACKOFF_MIN;
        while !self.stopping() {
            match self.cycle(&app, &tx, &reload, &mut backoff).await {
                CycleEnd::Stop => break,
                CycleEnd::Resubscribe => continue,
                CycleEnd::Failed(message) => {
                    warn!(error = %message, pause_s = backoff.as_secs(), "канал уведомлений: ошибка, пауза");
                    self.update(&app, |s| {
                        s.connected = false;
                        s.since_ms = None;
                        s.subscriptions = 0;
                        s.error = Some(message);
                    });
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = self.wait_stop() => {}
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
        drop(tx);
        let _ = dispatcher.await;
        self.update(&app, |s| {
            s.connected = false;
            s.since_ms = None;
            s.subscriptions = 0;
        });
        info!("канал уведомлений: задача завершена");
    }

    /// Подписки для одного цикла и соответствие `SubscriptionId` → ключ папки для строк папок.
    /// Идентификаторы постоянны в пределах процесса (`Inner::subscription_id`): в той же сессии
    /// сервер отвечает на них `SubscriptionExists`, после повторного входа создаёт заново.
    fn build_subscriptions(&self) -> Result<(Vec<SubscriptionRequest>, HashMap<String, i64>), String> {
        let folders = self.store.folders_for_sync(self.account).map_err(|e| format!("Ошибка локальной базы: {e}"))?;
        let rejected = self.lock().rejected.clone();
        if folders.len() > MAX_FOLDER_SUBSCRIPTIONS {
            warn!(folders = folders.len(), limit = MAX_FOLDER_SUBSCRIPTIONS, "канал: папок больше предела подписок, остальные только по таймеру");
        }
        // Вид подписки и ключ папки в базе (только у строк папок).
        let mut kinds: Vec<(NotificationKind, Option<i64>)> = folders.iter().take(MAX_FOLDER_SUBSCRIPTIONS).map(|f| (NotificationKind::FolderRows { folder: FolderId { id: f.server_id.clone(), change_key: None } }, Some(f.folder_pk))).collect();
        for kind in [NotificationKind::Hierarchy, NotificationKind::NewMail, NotificationKind::Unseen] {
            if !rejected.contains(kind.name()) {
                kinds.push((kind, None));
            }
        }
        let calendar_kind = NotificationKind::Calendar { folder: FolderId { id: String::new(), change_key: None } }.name();
        if !rejected.contains(calendar_kind) {
            match self.store.folder_by_distinguished(self.account, "calendar") {
                Ok(Some((_, server_id))) => kinds.push((NotificationKind::Calendar { folder: FolderId { id: server_id, change_key: None } }, None)),
                Ok(None) => debug!("канал: папки календаря в базе нет, подписки на календарь не будет"),
                Err(e) => warn!(error = %e, "канал: не удалось найти папку календаря"),
            }
        }
        let mut g = self.lock();
        let mut subs = Vec::with_capacity(kinds.len());
        let mut targets = HashMap::new();
        for (kind, folder_pk) in kinds {
            let id = g.subscription_id(&kind);
            if let Some(pk) = folder_pk {
                targets.insert(id.clone(), pk);
            }
            subs.push(SubscriptionRequest { id, kind });
        }
        Ok((subs, targets))
    }

    /// Один цикл: вход (если транспорта нет), подписки, потоки до ошибки, просьбы
    /// переподписаться или остановки.
    async fn cycle(&self, app: &AppHandle, tx: &UnboundedSender<Msg>, reload: &Notify, backoff: &mut Duration) -> CycleEnd {
        // Поколение транспорта: по ошибке мёртвой сессии сбрасываем только его, а не сессию,
        // которую синхронизация или команда окна уже успели открыть заново.
        let (transport, generation) = match self.sync.transport_with_generation().await {
            Ok(t) => t,
            Err(crate::sync::ConnectError::Closing) => return CycleEnd::Stop,
            Err(e) => return CycleEnd::Failed(e.to_string()),
        };
        let (subs, targets) = match self.build_subscriptions() {
            Ok(v) => v,
            Err(e) => return CycleEnd::Failed(e),
        };
        if subs.is_empty() {
            return CycleEnd::Failed("Нет папок для подписки".into());
        }
        let results = match tokio::time::timeout(SUBSCRIBE_TIMEOUT, transport.subscribe_notifications(&subs)).await {
            Err(_) => return CycleEnd::Failed(timed_out(SUBSCRIBE_TIMEOUT)),
            Ok(Err(e)) => {
                self.sync.invalidate_on_generation(&e, Some(generation)).await;
                return CycleEnd::Failed(describe_transport(&e));
            }
            Ok(Ok(r)) => r,
        };
        // Ни одной созданной — отказ всему запросу (временный сбой сервера, ответ не той формы):
        // пауза и новый цикл, виды подписок при этом не помечаются отвергнутыми. Отказ по виду
        // различим только рядом с принятыми подписками того же ответа.
        let created = results.iter().filter(|r| r.created).count();
        if created == 0 {
            return CycleEnd::Failed("Сервер не принял ни одной подписки".into());
        }
        let mut rows_rejected = 0usize;
        let mut accepted_targets = HashMap::new();
        for (req, res) in subs.iter().zip(&results) {
            if res.created {
                if let Some(pk) = targets.get(&req.id) {
                    accepted_targets.insert(req.id.clone(), *pk);
                }
                continue;
            }
            match req.kind {
                NotificationKind::FolderRows { .. } => rows_rejected += 1,
                _ => {
                    info!(kind = req.kind.name(), "канал: сервер отверг вид подписки, больше не просим");
                    self.lock().rejected.insert(req.kind.name());
                }
            }
        }
        if rows_rejected > 0 {
            warn!(rejected = rows_rejected, "канал: подписки на строки части папок отвергнуты");
        }
        let _ = tx.send(Msg::Subscribed(accepted_targets));
        let cid = new_guid();
        self.update(app, |s| {
            s.connected = true;
            s.since_ms = Some(now_ms());
            s.subscriptions = created;
            s.error = None;
        });
        info!(subscriptions = created, requested = subs.len(), "канал уведомлений открыт");

        let sink = |ev: NotificationEvent| {
            let _ = tx.send(Msg::Event(ev));
        };
        loop {
            enum StreamOutcome {
                Ended(aiowa_transport::Result<aiowa_transport::StreamEnd>),
                Reload,
                Stop,
            }
            // Открытый цикл потока: по этому `cid` выход завершит его, если задача не успеет сама.
            self.lock().cid = Some(cid.clone());
            let stream_started = Instant::now();
            let outcome = tokio::select! {
                r = transport.notification_stream(&cid, &sink) => StreamOutcome::Ended(r),
                _ = reload.notified() => StreamOutcome::Reload,
                _ = self.wait_stop() => StreamOutcome::Stop,
            };
            match outcome {
                StreamOutcome::Ended(Ok(end)) => {
                    debug!(events = end.events, elapsed_s = end.elapsed.as_secs(), finished_by_server = end.finished_by_server, "канал: цикл потока завершён");
                    if end.finished_by_server || end.elapsed >= HEALTHY_STREAM {
                        *backoff = BACKOFF_MIN;
                    }
                    if let Err(e) = self.finish(&transport, generation, &cid).await {
                        return CycleEnd::Failed(e);
                    }
                }
                StreamOutcome::Ended(Err(e)) => {
                    self.lock().cid = None;
                    self.sync.invalidate_on_generation(&e, Some(generation)).await;
                    // Долго живший поток оборвался с ошибкой (чаще всего мёртвое соединение после
                    // сна или смены сети): что произошло за это время, неизвестно — догоняющая
                    // синхронизация всех папок. Короткий поток ничего пропустить не успел.
                    if stream_started.elapsed() >= HEALTHY_STREAM && !self.stopping() {
                        info!(stream_s = stream_started.elapsed().as_secs(), "канал: поток оборвался, догоняющая синхронизация");
                        self.sync.request(app.clone(), None);
                    }
                    return CycleEnd::Failed(describe_transport(&e));
                }
                StreamOutcome::Reload => {
                    info!("канал: подписки потеряны, подписываюсь заново");
                    let _ = self.finish(&transport, generation, &cid).await;
                    return CycleEnd::Resubscribe;
                }
                StreamOutcome::Stop => {
                    let _ = self.finish(&transport, generation, &cid).await;
                    return CycleEnd::Stop;
                }
            }
        }
    }

    /// `FinishNotificationRequest` после конца цикла. Ошибка мёртвой сессии сбрасывает транспорт;
    /// остальные только в журнал: следующий `PendingNotificationRequest` сервер всё равно примет.
    async fn finish(&self, transport: &aiowa_transport_owa::OwaTransport, generation: u64, cid: &str) -> Result<(), String> {
        let r = tokio::time::timeout(FINISH_TIMEOUT, transport.finish_notification_stream(cid)).await;
        self.lock().cid = None;
        match r {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) if e.needs_reauth() || matches!(e, TransportError::AuthFailed(_)) => {
                self.sync.invalidate_on_generation(&e, Some(generation)).await;
                Err(describe_transport(&e))
            }
            Ok(Err(e)) => {
                warn!(error = %e, "канал: FinishNotificationRequest не удался");
                Ok(())
            }
            Err(_) => {
                warn!(timeout_s = FINISH_TIMEOUT.as_secs(), "канал: FinishNotificationRequest не уложился в таймаут");
                Ok(())
            }
        }
    }

    // --- счётчик непрочитанных -------------------------------------------------------------

    /// Заголовок окна и значок на панели задач по непрочитанным «Входящих» из базы.
    pub(crate) fn refresh_unread(&self, app: &AppHandle) {
        let unread = match self.store.list_folders(self.account) {
            Ok(list) => list.into_iter().find(|(_, f)| f.distinguished.as_deref() == Some("inbox")).map(|(_, f)| f.unread_count).unwrap_or(0),
            Err(e) => {
                warn!(error = %e, "не удалось прочитать счётчики папок");
                return;
            }
        };
        if self.lock().shown_unread == Some(unread) {
            return;
        }
        // Без окна число не запоминаем: иначе следующий вызов с тем же числом ничего бы не показал.
        if apply_unread(app, unread) {
            self.lock().shown_unread = Some(unread);
        }
    }

    // --- выход -----------------------------------------------------------------------------

    /// При выходе: остановить задачу (она сама завершит цикл `FinishNotificationRequest`), а если
    /// не успела — завершить цикл отсюда. Вызывается с главного потока до `SyncState::shutdown`,
    /// пока транспорт ещё жив.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.stop.notify_one();
        let task = self.lock().task.take();
        let Some(mut task) = task else { return };
        tauri::async_runtime::block_on(async move {
            if tokio::time::timeout(EXIT_GRACE, &mut task).await.is_err() {
                warn!("канал уведомлений не остановился за {} с, прерываю", EXIT_GRACE.as_secs());
                task.abort();
            }
            let cid = self.lock().cid.take();
            if let (Some(cid), Some(t)) = (cid, self.sync.current_transport()) {
                match tokio::time::timeout(EXIT_GRACE, t.finish_notification_stream(&cid)).await {
                    Ok(Ok(())) => info!("канал уведомлений: цикл завершён при выходе"),
                    Ok(Err(e)) => warn!(error = %e, "канал уведомлений: не удалось завершить цикл при выходе"),
                    Err(_) => warn!("канал уведомлений: завершение цикла не уложилось в {} с", EXIT_GRACE.as_secs()),
                }
            }
        });
    }
}

// --- диспетчер событий ----------------------------------------------------------------------

/// Обработчик событий потока: дебаунс папок, политика тостов, события окну.
struct Dispatcher<'a> {
    state: &'a NotifyState,
    app: &'a AppHandle,
    reload: &'a Notify,
    /// `SubscriptionId` подписки на строки → ключ папки.
    targets: HashMap<String, i64>,
    debounce: Debounce,
    toasts: ToastPolicy,
    /// Текст последнего письма без тоста: показывается, когда политика разрешит одиночный тост.
    last_mail: Option<ToastText>,
}

/// Ждёт до момента или вечно, если момента нет (ветка `select!`, которой нечего ждать).
async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending::<()>().await,
    }
}

impl<'a> Dispatcher<'a> {
    fn new(state: &'a NotifyState, app: &'a AppHandle, reload: &'a Notify) -> Self {
        Self { state, app, reload, targets: HashMap::new(), debounce: Debounce::default(), toasts: ToastPolicy::default(), last_mail: None }
    }

    async fn run(mut self, mut rx: UnboundedReceiver<Msg>) {
        loop {
            let folders_due = self.debounce.deadline();
            let toast_due = self.toasts.next_due(Instant::now());
            tokio::select! {
                msg = rx.recv() => match msg {
                    None => break,
                    Some(Msg::Subscribed(targets)) => self.targets = targets,
                    Some(Msg::Event(ev)) => self.on_event(ev),
                },
                _ = sleep_until_opt(folders_due) => {
                    if let Some(batch) = self.debounce.take_due(Instant::now()) {
                        self.apply_batch(batch);
                    }
                }
                _ = sleep_until_opt(toast_due) => {
                    if let Some(t) = self.toasts.flush(Instant::now()) {
                        self.show_toast(t);
                    }
                }
            }
        }
        // Канал закрывается только при выходе из приложения: накопленные папки не досинхронизируем,
        // иначе в момент закрытия стартовал бы новый прогон (а при сброшенном транспорте — вход).
    }

    fn on_event(&mut self, ev: NotificationEvent) {
        let now = Instant::now();
        debug!(kind = ev.kind_name(), "канал: событие");
        self.state.update(self.app, |s| s.last_event_ms = Some(now_ms()));
        match ev {
            NotificationEvent::FolderRows { subscription, folder, .. } => match self.folder_pk(&subscription, folder.as_ref()) {
                Some(pk) => self.debounce.folder(pk, now),
                None => self.debounce.all(now),
            },
            NotificationEvent::Hierarchy { folder, event_type, .. } => {
                // Изменение одной известной папки (так OWA сообщает и о смене её счётчиков, то есть
                // вместе с каждым письмом) — синхронизация только её: прогон и так начинается с
                // дерева папок. Создание, удаление, перенос, событие без папки или с неизвестной —
                // полный прогон.
                let modified = event_type.to_ascii_lowercase().contains("modif");
                match folder.as_ref().filter(|_| modified).and_then(|f| self.lookup_folder(f)) {
                    Some(pk) => self.debounce.folder(pk, now),
                    None => self.debounce.all(now),
                }
            }
            NotificationEvent::NewMail { item_id, folder, from, subject, preview, is_read, .. } => {
                let folder_pk = folder.as_ref().and_then(|f| self.lookup_folder(f));
                if let Some(pk) = folder_pk {
                    self.debounce.folder(pk, now);
                }
                if is_read == Some(true) {
                    return;
                }
                let dto = NewMailDto { from, subject, preview, item_id, item_pk: None, folder_pk };
                if self.state.toasts_enabled() {
                    self.last_mail = Some(ToastText::for_mail(dto.from.as_ref(), dto.subject.as_deref(), dto.preview.as_deref()));
                    if let Some(t) = self.toasts.on_mail(now) {
                        self.show_toast(t);
                    }
                }
                if let Err(e) = self.app.emit("new-mail", dto) {
                    warn!(error = %e, "не удалось отправить событие new-mail");
                }
            }
            NotificationEvent::Unseen { .. } => self.state.refresh_unread(self.app),
            NotificationEvent::Calendar { .. } => self.debounce.calendar(now),
            NotificationEvent::KeepAlive => {}
            NotificationEvent::Reload => self.reload.notify_one(),
            NotificationEvent::Unknown { raw_kind } => debug!(raw_kind, "канал: блок неизвестного вида"),
        }
    }

    /// Ключ папки по подписке, а если подписка неизвестна — по идентификатору папки из события.
    fn folder_pk(&self, subscription: &str, folder: Option<&FolderId>) -> Option<i64> {
        self.targets.get(subscription).copied().or_else(|| folder.and_then(|f| self.lookup_folder(f)))
    }

    fn lookup_folder(&self, folder: &FolderId) -> Option<i64> {
        match self.state.store.folder_pk(self.state.account, &folder.id) {
            Ok(pk) => pk,
            Err(e) => {
                warn!(error = %e, "канал: не удалось найти папку из события");
                None
            }
        }
    }

    fn apply_batch(&self, batch: Batch) {
        if self.state.stopping() {
            debug!("канал: приложение закрывается, синхронизацию по уведомлению не запускаю");
            return;
        }
        if batch.calendar
            && let Err(e) = self.app.emit("calendar-changed", ())
        {
            warn!(error = %e, "не удалось отправить событие calendar-changed");
        }
        match batch.folders {
            Some(pks) if pks.is_empty() => {}
            Some(pks) => {
                info!(folders = pks.len(), "канал: синхронизация папок по уведомлению");
                self.state.sync.request(self.app.clone(), Some(pks));
            }
            None => {
                info!("канал: полная синхронизация по уведомлению об иерархии");
                self.state.sync.request(self.app.clone(), None);
            }
        }
    }

    fn show_toast(&self, toast: Toast) {
        if !self.state.toasts_enabled() {
            return;
        }
        let text = match toast {
            Toast::Single => match &self.last_mail {
                Some(t) => t.clone(),
                None => return,
            },
            Toast::Summary(n) => ToastText { title: APP_TITLE.to_string(), body: summary_text(n) },
        };
        debug!(summary = matches!(toast, Toast::Summary(_)), "канал: тост");
        if let Err(e) = self.app.notification().builder().title(text.title).body(text.body).show() {
            warn!(error = %e, "тост не показан");
        }
    }
}

// --- чистая логика: дебаунс папок --------------------------------------------------------------

/// Что запустить по итогам окна дебаунса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Batch {
    /// `None` — все папки (иерархия изменилась или папка события неизвестна); иначе список
    /// ключей папок в порядке первого упоминания, без повторов.
    pub folders: Option<Vec<i64>>,
    pub calendar: bool,
}

/// Копилка событий на окно `DEBOUNCE` от первого события: несколько событий по одной папке и по
/// разным папкам уходят одной синхронизацией. Окно не продлевается новыми событиями, иначе при
/// постоянном потоке синхронизация не началась бы никогда.
#[derive(Debug, Default)]
pub(crate) struct Debounce {
    folders: Vec<i64>,
    all: bool,
    calendar: bool,
    deadline: Option<Instant>,
}

impl Debounce {
    fn arm(&mut self, now: Instant) {
        if self.deadline.is_none() {
            self.deadline = Some(now + DEBOUNCE);
        }
    }

    pub(crate) fn folder(&mut self, folder_pk: i64, now: Instant) {
        if !self.folders.contains(&folder_pk) {
            self.folders.push(folder_pk);
        }
        self.arm(now);
    }

    pub(crate) fn all(&mut self, now: Instant) {
        self.all = true;
        self.arm(now);
    }

    pub(crate) fn calendar(&mut self, now: Instant) {
        self.calendar = true;
        self.arm(now);
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Пачка, если окно истекло; иначе `None` и ничего не меняется.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<Batch> {
        if self.deadline.is_some_and(|d| now >= d) { self.take_all() } else { None }
    }

    /// Пачка без ожидания; `None`, если ничего не накопилось.
    pub(crate) fn take_all(&mut self) -> Option<Batch> {
        self.deadline?;
        let folders = if self.all { None } else { Some(std::mem::take(&mut self.folders)) };
        let batch = Batch { folders, calendar: self.calendar };
        *self = Debounce::default();
        Some(batch)
    }
}

// --- чистая логика: политика тостов -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Toast {
    /// Тост об одном письме: отправитель, тема, начало превью.
    Single,
    /// Сводка «N новых писем».
    Summary(usize),
}

/// Когда показывать тост: не чаще одного за `TOAST_MIN_GAP`; письма, пришедшие в паузу, копятся
/// и выходят одной сводкой; при `BURST_COUNT` и более письмах за `BURST_WINDOW` сводка —
/// не чаще раза за окно.
#[derive(Debug, Default)]
pub(crate) struct ToastPolicy {
    /// Моменты писем за последнее окно.
    recent: VecDeque<Instant>,
    last_toast: Option<Instant>,
    /// Последний тост, показанный в режиме «лавины».
    burst_toast: Option<Instant>,
    /// Писем без тоста с момента последнего тоста.
    pending: usize,
}

impl ToastPolicy {
    fn prune(&mut self, now: Instant) {
        while self.recent.front().is_some_and(|t| now.duration_since(*t) >= BURST_WINDOW) {
            self.recent.pop_front();
        }
    }

    fn burst(&self) -> bool {
        self.recent.len() >= BURST_COUNT
    }

    /// Пришло письмо: тост сейчас или `None` (письмо подождёт `flush`).
    pub(crate) fn on_mail(&mut self, now: Instant) -> Option<Toast> {
        self.prune(now);
        self.recent.push_back(now);
        self.pending += 1;
        self.flush(now)
    }

    /// Показать накопленное, если политика уже разрешает.
    pub(crate) fn flush(&mut self, now: Instant) -> Option<Toast> {
        if self.pending == 0 {
            return None;
        }
        self.prune(now);
        if self.last_toast.is_some_and(|t| now.duration_since(t) < TOAST_MIN_GAP) {
            return None;
        }
        let burst = self.burst();
        if burst && self.burst_toast.is_some_and(|t| now.duration_since(t) < BURST_WINDOW) {
            return None;
        }
        self.last_toast = Some(now);
        if burst {
            self.burst_toast = Some(now);
        }
        let n = std::mem::take(&mut self.pending);
        Some(if n == 1 { Toast::Single } else { Toast::Summary(n) })
    }

    /// Когда накопленное можно будет показать; `None`, если показывать нечего.
    pub(crate) fn next_due(&self, now: Instant) -> Option<Instant> {
        if self.pending == 0 {
            return None;
        }
        let mut due = self.last_toast.map_or(now, |t| t + TOAST_MIN_GAP);
        if self.burst()
            && let Some(b) = self.burst_toast
        {
            due = due.max(b + BURST_WINDOW);
        }
        Some(due)
    }
}

/// Текст тоста. Содержимое письма здесь есть, в журнал оно не пишется.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToastText {
    title: String,
    body: String,
}

impl ToastText {
    fn for_mail(from: Option<&Participant>, subject: Option<&str>, preview: Option<&str>) -> Self {
        Self { title: sender_label(from), body: toast_body(subject, preview) }
    }
}

/// Отправитель для заголовка тоста: имя, иначе адрес, иначе общая фраза.
pub(crate) fn sender_label(from: Option<&Participant>) -> String {
    from.and_then(|p| p.name.as_deref().filter(|s| !s.trim().is_empty()).or(p.email.as_deref().filter(|s| !s.trim().is_empty()))).map(|s| s.trim().to_string()).unwrap_or_else(|| "Новое письмо".to_string())
}

/// Тема и начало превью одной строкой, не длиннее `TOAST_BODY_CHARS` символов.
pub(crate) fn toast_body(subject: Option<&str>, preview: Option<&str>) -> String {
    let subject = subject.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("(без темы)");
    let mut out = subject.to_string();
    if let Some(p) = preview.map(|p| p.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|p| !p.is_empty()) {
        out.push_str(" — ");
        out.push_str(&p);
    }
    truncate_chars(&out, TOAST_BODY_CHARS)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// «N новых писем» со склонением по правилам русского языка.
pub(crate) fn summary_text(n: usize) -> String {
    let (adj, noun) = match (n % 100, n % 10) {
        (11..=14, _) => ("новых", "писем"),
        (_, 1) => ("новое", "письмо"),
        (_, 2..=4) => ("новых", "письма"),
        _ => ("новых", "писем"),
    };
    format!("{n} {adj} {noun}")
}

// --- чистая логика: заголовок окна и значок ---------------------------------------------------

/// Заголовок окна: «AIOWA» или «AIOWA (N)».
pub(crate) fn title_text(unread: i64) -> String {
    if unread > 0 { format!("{APP_TITLE} ({unread})") } else { APP_TITLE.to_string() }
}

/// Текст на значке: до двух цифр, дальше «99+».
pub(crate) fn badge_text(unread: i64) -> String {
    if unread > 99 { "99+".to_string() } else { unread.max(0).to_string() }
}

/// Глиф 3×5: пять строк по три бита, старший бит — левый пиксель.
fn glyph(c: char) -> [u8; 5] {
    match c {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '+' => [0b000, 0b010, 0b111, 0b010, 0b000],
        _ => [0; 5],
    }
}

/// Значок-наклейка для панели задач: красный круг с числом непрочитанных, RGBA
/// `BADGE_SIZE`×`BADGE_SIZE`. Рисуется без внешних крейтов: шрифт 3×5 с масштабом.
pub(crate) fn badge_rgba(unread: i64) -> Vec<u8> {
    const RED: [u8; 4] = [217, 48, 37, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    let size = BADGE_SIZE as usize;
    let mut px = vec![0u8; size * size * 4];
    let center = size as f32 / 2.0;
    let radius = center - 0.5;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            if dx * dx + dy * dy <= radius * radius {
                px[(y * size + x) * 4..][..4].copy_from_slice(&RED);
            }
        }
    }
    let text = badge_text(unread);
    let chars: Vec<char> = text.chars().collect();
    let (scale, gap) = if chars.len() <= 2 { (3usize, 2usize) } else { (2, 1) };
    let (gw, gh) = (3 * scale, 5 * scale);
    let width = chars.len() * gw + chars.len().saturating_sub(1) * gap;
    let x0 = (size.saturating_sub(width)) / 2;
    let y0 = (size - gh) / 2;
    for (i, c) in chars.iter().enumerate() {
        let rows = glyph(*c);
        let gx = x0 + i * (gw + gap);
        for (r, bits) in rows.iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = gx + col * scale + sx;
                        let y = y0 + r * scale + sy;
                        if x < size && y < size {
                            px[(y * size + x) * 4..][..4].copy_from_slice(&WHITE);
                        }
                    }
                }
            }
        }
    }
    px
}

/// Заголовок окна и (на Windows) значок-наклейка на панели задач. `false` — окна ещё или уже нет.
fn apply_unread(app: &AppHandle, unread: i64) -> bool {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else { return false };
    if let Err(e) = window.set_title(&title_text(unread)) {
        warn!(error = %e, "не удалось сменить заголовок окна");
    }
    #[cfg(target_os = "windows")]
    {
        let icon = (unread > 0).then(|| tauri::image::Image::new_owned(badge_rgba(unread), BADGE_SIZE, BADGE_SIZE));
        if let Err(e) = window.set_overlay_icon(icon) {
            warn!(error = %e, "не удалось поставить значок непрочитанных на панель задач");
        }
    }
    true
}

// --- команды Tauri -----------------------------------------------------------

#[tauri::command]
pub fn notify_status(state: State<'_, Arc<NotifyState>>) -> NotifyStatusDto {
    state.status()
}

#[tauri::command]
pub fn notify_set_toasts(app: AppHandle, state: State<'_, Arc<NotifyState>>, enabled: bool) -> CmdResult<()> {
    state.set_toasts(&app, enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs_x10: u64) -> Instant {
        base + Duration::from_millis(secs_x10 * 100)
    }

    #[test]
    fn debounce_merges_folders_in_one_window() {
        let base = Instant::now();
        let mut d = Debounce::default();
        assert_eq!(d.deadline(), None);
        assert_eq!(d.take_due(base), None, "пусто — нечего отдавать");
        d.folder(5, base);
        d.folder(7, at(base, 5));
        d.folder(5, at(base, 10));
        assert_eq!(d.deadline(), Some(base + DEBOUNCE), "окно отсчитывается от первого события и не продлевается");
        assert_eq!(d.take_due(at(base, 10)), None, "до конца окна пачки нет");
        assert_eq!(d.take_due(base + DEBOUNCE), Some(Batch { folders: Some(vec![5, 7]), calendar: false }), "папки без повторов в порядке первого упоминания");
        assert_eq!(d.deadline(), None, "после выдачи копилка пуста");
        assert_eq!(d.take_all(), None);
    }

    #[test]
    fn debounce_hierarchy_turns_batch_into_full_sync() {
        let base = Instant::now();
        let mut d = Debounce::default();
        d.folder(1, base);
        d.all(at(base, 1));
        d.folder(2, at(base, 2));
        d.calendar(at(base, 3));
        assert_eq!(d.take_all(), Some(Batch { folders: None, calendar: true }));
        let mut only_calendar = Debounce::default();
        only_calendar.calendar(base);
        assert_eq!(only_calendar.take_due(base + DEBOUNCE), Some(Batch { folders: Some(vec![]), calendar: true }), "календарь без папок — синхронизации нет");
    }

    #[test]
    fn toast_policy_limits_rate_and_summarizes_bursts() {
        let base = Instant::now();
        let mut p = ToastPolicy::default();
        assert_eq!(p.next_due(base), None);
        assert_eq!(p.on_mail(base), Some(Toast::Single), "первое письмо — сразу");
        assert_eq!(p.on_mail(at(base, 10)), None, "второе через 1 с — ждёт");
        assert_eq!(p.on_mail(at(base, 20)), None);
        assert_eq!(p.next_due(at(base, 20)), Some(base + TOAST_MIN_GAP), "показ разрешится через 5 с после первого тоста");
        assert_eq!(p.flush(at(base, 40)), None, "раньше срока — нет");
        assert_eq!(p.flush(base + TOAST_MIN_GAP), Some(Toast::Summary(2)), "накопленные два письма — одной сводкой");
        assert_eq!(p.next_due(base + TOAST_MIN_GAP), None, "показывать больше нечего");

        // Лавина: четыре письма за минуту — после сводки следующая не раньше чем через окно.
        let mut p = ToastPolicy::default();
        assert_eq!(p.on_mail(base), Some(Toast::Single));
        for i in 1..=3 {
            assert_eq!(p.on_mail(at(base, i)), None);
        }
        let t = base + TOAST_MIN_GAP;
        assert_eq!(p.flush(t), Some(Toast::Summary(3)), "четвёртое письмо за минуту включает режим сводок");
        assert_eq!(p.on_mail(t + Duration::from_secs(10)), None, "в лавине отдельных тостов нет");
        assert_eq!(p.on_mail(t + Duration::from_secs(20)), None);
        assert_eq!(p.next_due(t + Duration::from_secs(20)), Some(t + BURST_WINDOW), "следующая сводка — через окно после предыдущей");
        assert_eq!(p.flush(t + Duration::from_secs(30)), None);
        assert_eq!(p.flush(t + BURST_WINDOW), Some(Toast::Summary(2)));
        // Лавина утихла: через минуту тишины одиночные тосты возвращаются.
        let quiet = t + BURST_WINDOW * 2 + Duration::from_secs(1);
        assert_eq!(p.on_mail(quiet), Some(Toast::Single));
    }

    #[test]
    fn toast_texts() {
        assert_eq!(summary_text(1), "1 новое письмо");
        assert_eq!(summary_text(2), "2 новых письма");
        assert_eq!(summary_text(5), "5 новых писем");
        assert_eq!(summary_text(11), "11 новых писем");
        assert_eq!(summary_text(21), "21 новое письмо");
        assert_eq!(summary_text(104), "104 новых письма");
        let p = Participant { name: Some(" Иван Петров ".into()), email: Some("ivan@example.org".into()), routing_type: None };
        assert_eq!(sender_label(Some(&p)), "Иван Петров");
        let only_email = Participant { name: Some("".into()), email: Some("ivan@example.org".into()), routing_type: None };
        assert_eq!(sender_label(Some(&only_email)), "ivan@example.org");
        assert_eq!(sender_label(None), "Новое письмо");
        assert_eq!(toast_body(Some("Тема"), Some("  первая   строка\nвторая ")), "Тема — первая строка вторая");
        assert_eq!(toast_body(None, None), "(без темы)");
        assert_eq!(toast_body(Some("  "), Some("")), "(без темы)");
        let long = toast_body(Some("Т"), Some(&"ы".repeat(500)));
        assert_eq!(long.chars().count(), TOAST_BODY_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn window_title_and_badge() {
        assert_eq!(title_text(0), "AIOWA");
        assert_eq!(title_text(-3), "AIOWA");
        assert_eq!(title_text(7), "AIOWA (7)");
        assert_eq!(title_text(1234), "AIOWA (1234)");
        assert_eq!(badge_text(7), "7");
        assert_eq!(badge_text(99), "99");
        assert_eq!(badge_text(100), "99+");
        for n in [1, 42, 150] {
            let px = badge_rgba(n);
            assert_eq!(px.len(), (BADGE_SIZE * BADGE_SIZE * 4) as usize);
            assert_eq!(&px[..4], &[0, 0, 0, 0], "угол прозрачный");
            let center = ((BADGE_SIZE / 2 * BADGE_SIZE + BADGE_SIZE / 2) * 4) as usize;
            assert_eq!(px[center + 3], 255, "центр непрозрачный");
            let white = px.chunks(4).filter(|p| p == &[255, 255, 255, 255]).count();
            let red = px.chunks(4).filter(|p| p == &[217, 48, 37, 255]).count();
            assert!(white > 0 && red > white, "n={n}: белых {white}, красных {red}");
        }
    }

    #[test]
    fn subscription_ids_are_stable_per_kind_and_folder() {
        let mut inner = Inner::default();
        let rows_a = NotificationKind::FolderRows { folder: FolderId { id: "A".into(), change_key: None } };
        let rows_b = NotificationKind::FolderRows { folder: FolderId { id: "B".into(), change_key: None } };
        let cal_a = NotificationKind::Calendar { folder: FolderId { id: "A".into(), change_key: None } };
        let first = inner.subscription_id(&rows_a);
        assert_eq!(first.len(), 36, "guid");
        assert_eq!(inner.subscription_id(&rows_a), first, "тот же вид и папка — тот же идентификатор в следующем цикле");
        assert_ne!(inner.subscription_id(&rows_b), first, "другая папка — другой");
        assert_ne!(inner.subscription_id(&cal_a), first, "та же папка, другой вид — другой");
        let mail = inner.subscription_id(&NotificationKind::NewMail);
        assert_eq!(inner.subscription_id(&NotificationKind::NewMail), mail);
        assert_ne!(inner.subscription_id(&NotificationKind::Hierarchy), mail);
        assert_eq!(inner.ids.len(), 5, "по записи на каждую пару вид+папка");
    }

    #[test]
    fn toasts_default_on_and_parsed_from_state() {
        let on = |v: Option<&str>| v.is_none_or(|v| v != "0");
        assert!(on(None));
        assert!(on(Some("1")));
        assert!(!on(Some("0")));
    }
}
