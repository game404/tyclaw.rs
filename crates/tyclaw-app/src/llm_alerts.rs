use parking_lot::RwLock;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tyclaw_provider::{ProviderEvent, ProviderEventKind, WorkloadKind};

fn default_warning_window() -> u64 {
    900
}
fn default_warning_count() -> usize {
    15
}
fn default_cooldown() -> u64 {
    7200
}
fn default_interactive_count() -> usize {
    1
}
fn default_background_count() -> usize {
    2
}
fn default_background_window() -> u64 {
    3600
}
fn default_quiet_window() -> u64 {
    1800
}
fn default_recovery_timeouts() -> usize {
    2
}
fn default_true() -> bool {
    true
}
fn default_channel() -> String {
    "dingtalk".into()
}
fn default_max_recipients() -> usize {
    20
}
fn default_notification_timeout() -> u64 {
    15
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct LlmAlertsConfig {
    pub enabled: bool,
    pub warning: WarningConfig,
    pub critical: CriticalConfig,
    pub recovery: RecoveryConfig,
    pub notification: NotificationConfig,
}

impl Default for LlmAlertsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            warning: WarningConfig::default(),
            critical: CriticalConfig::default(),
            recovery: RecoveryConfig::default(),
            notification: NotificationConfig::default(),
        }
    }
}

impl LlmAlertsConfig {
    #[cfg(test)]
    fn enabled_defaults() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    pub(crate) fn validated(mut self) -> Result<Self, &'static str> {
        if self.warning.window_secs == 0 {
            return Err("llm_alerts.warning.window_secs: must_be_positive");
        }
        if self.warning.min_send_timeouts == 0 {
            return Err("llm_alerts.warning.min_send_timeouts: must_be_positive");
        }
        if self.warning.cooldown_secs == 0 {
            return Err("llm_alerts.warning.cooldown_secs: must_be_positive");
        }
        if self.critical.interactive_exhausted == 0 {
            return Err("llm_alerts.critical.interactive_exhausted: must_be_positive");
        }
        if self.critical.background_exhausted == 0 {
            return Err("llm_alerts.critical.background_exhausted: must_be_positive");
        }
        if self.critical.background_window_secs == 0 {
            return Err("llm_alerts.critical.background_window_secs: must_be_positive");
        }
        if self.critical.cooldown_secs == 0 {
            return Err("llm_alerts.critical.cooldown_secs: must_be_positive");
        }
        if self.recovery.quiet_window_secs == 0 {
            return Err("llm_alerts.recovery.quiet_window_secs: must_be_positive");
        }
        if self.recovery.max_send_timeouts > self.warning.min_send_timeouts {
            return Err("llm_alerts.recovery.max_send_timeouts: exceeds_warning_threshold");
        }
        if self.notification.channel != "dingtalk" {
            return Err("llm_alerts.notification.channel: unsupported_channel");
        }
        if self.notification.max_recipients == 0 {
            return Err("llm_alerts.notification.max_recipients: must_be_positive");
        }
        if self.notification.timeout_secs == 0 {
            return Err("llm_alerts.notification.timeout_secs: must_be_positive");
        }

        let mut normalized = Vec::new();
        for id in &self.notification.admin_user_ids {
            let id = id.trim();
            if id.is_empty() {
                return Err("llm_alerts.notification.admin_user_ids: contains_empty_value");
            }
            if !normalized.iter().any(|existing| existing == id) {
                normalized.push(id.to_string());
            }
        }
        if normalized.len() > self.notification.max_recipients {
            return Err("llm_alerts.notification.admin_user_ids: too_many_recipients");
        }
        self.notification.admin_user_ids = normalized;
        Ok(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct WarningConfig {
    #[serde(default = "default_warning_window")]
    pub window_secs: u64,
    #[serde(default = "default_warning_count")]
    pub min_send_timeouts: usize,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
}

impl Default for WarningConfig {
    fn default() -> Self {
        Self {
            window_secs: default_warning_window(),
            min_send_timeouts: default_warning_count(),
            cooldown_secs: default_cooldown(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct CriticalConfig {
    #[serde(default = "default_interactive_count")]
    pub interactive_exhausted: usize,
    #[serde(default = "default_background_count")]
    pub background_exhausted: usize,
    #[serde(default = "default_background_window")]
    pub background_window_secs: u64,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
}

impl Default for CriticalConfig {
    fn default() -> Self {
        Self {
            interactive_exhausted: default_interactive_count(),
            background_exhausted: default_background_count(),
            background_window_secs: default_background_window(),
            cooldown_secs: default_cooldown(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct RecoveryConfig {
    #[serde(default = "default_quiet_window")]
    pub quiet_window_secs: u64,
    #[serde(default = "default_recovery_timeouts")]
    pub max_send_timeouts: usize,
    #[serde(default = "default_true")]
    pub notify: bool,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            quiet_window_secs: default_quiet_window(),
            max_send_timeouts: default_recovery_timeouts(),
            notify: true,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub(crate) struct NotificationConfig {
    #[serde(default = "default_channel")]
    pub channel: String,
    pub admin_user_ids: Vec<String>,
    #[serde(default = "default_max_recipients")]
    pub max_recipients: usize,
    #[serde(default = "default_notification_timeout")]
    pub timeout_secs: u64,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            channel: default_channel(),
            admin_user_ids: Vec::new(),
            max_recipients: default_max_recipients(),
            timeout_secs: default_notification_timeout(),
        }
    }
}

impl fmt::Debug for NotificationConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NotificationConfig")
            .field("channel", &self.channel)
            .field("admin_count", &self.admin_user_ids.len())
            .field("max_recipients", &self.max_recipients)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AlertLevel {
    Healthy,
    Warning,
    Critical,
}

#[derive(Debug, Clone)]
pub(crate) struct AlertReport {
    pub level: AlertLevel,
    pub send_timeouts: usize,
    pub exhausted_calls: usize,
}

#[derive(Debug, Clone)]
pub(crate) enum AlertAction {
    NotifyWarning(AlertReport),
    NotifyCritical(AlertReport),
    NotifyRecovery(AlertReport),
}

#[derive(Debug, Clone)]
pub(crate) struct AlertSnapshot {
    pub level: AlertLevel,
    pub available: bool,
    pub reason: String,
    pub send_timeouts: usize,
    pub exhausted_calls: usize,
    pub dropped_events: u64,
    pub state_evictions: u64,
    pub notification_status: String,
    pub last_error_kind: Option<String>,
}

impl AlertSnapshot {
    pub(crate) fn disabled() -> Self {
        Self {
            level: AlertLevel::Healthy,
            available: false,
            reason: "disabled".into(),
            send_timeouts: 0,
            exhausted_calls: 0,
            dropped_events: 0,
            state_evictions: 0,
            notification_status: "disabled".into(),
            last_error_kind: None,
        }
    }

    pub(crate) fn config_invalid() -> Self {
        Self {
            reason: "config_invalid".into(),
            notification_status: "disabled".into(),
            ..Self::disabled()
        }
    }
}

pub(crate) struct AlertRuntime {
    config: Option<LlmAlertsConfig>,
    receiver: Option<mpsc::Receiver<ProviderEvent>>,
    pub(crate) snapshot: Arc<RwLock<AlertSnapshot>>,
}

impl AlertRuntime {
    pub(crate) fn prepare(config: LlmAlertsConfig) -> Self {
        if !config.enabled {
            tyclaw_provider::install_provider_event_sink(None);
            return Self {
                config: None,
                receiver: None,
                snapshot: Arc::new(RwLock::new(AlertSnapshot::disabled())),
            };
        }
        let config = match config.validated() {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(
                    error,
                    "LLM alerting disabled because configuration is invalid"
                );
                tyclaw_provider::install_provider_event_sink(None);
                return Self {
                    config: None,
                    receiver: None,
                    snapshot: Arc::new(RwLock::new(AlertSnapshot::config_invalid())),
                };
            }
        };
        let (sender, receiver) = mpsc::channel(1024);
        tyclaw_provider::install_provider_event_sink(Some(Arc::new(
            tyclaw_provider::ProviderEventSink::new(
                sender,
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ),
        )));
        let mut snapshot = AlertStateMachine::new(config.clone()).snapshot();
        if config.notification.admin_user_ids.is_empty() {
            snapshot.notification_status = "unavailable".into();
            snapshot.last_error_kind = Some("notification_unavailable".into());
        }
        Self {
            config: Some(config),
            receiver: Some(receiver),
            snapshot: Arc::new(RwLock::new(snapshot)),
        }
    }

    pub(crate) fn notification_config(&self) -> Option<&NotificationConfig> {
        self.config.as_ref().map(|config| &config.notification)
    }

    pub(crate) fn start(
        &mut self,
        notifier: Option<Arc<dyn AlertNotifier>>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let config = self.config.take()?;
        let receiver = self.receiver.take()?;
        let snapshot = Arc::clone(&self.snapshot);
        Some(tokio::spawn(run_alert_manager(
            config, receiver, snapshot, notifier,
        )))
    }
}

pub(crate) async fn shutdown_alert_manager(handle: Option<tokio::task::JoinHandle<()>>) {
    tyclaw_provider::install_provider_event_sink(None);
    if let Some(mut handle) = handle {
        if tokio::time::timeout(Duration::from_secs(1), &mut handle)
            .await
            .is_err()
        {
            handle.abort();
        }
    }
}

pub(crate) struct AlertStateMachine {
    config: LlmAlertsConfig,
    level: AlertLevel,
    state_since: Option<Instant>,
    send_timeouts: VecDeque<Instant>,
    background_exhausted: VecDeque<Instant>,
    all_exhausted: VecDeque<Instant>,
    seen_exhausted: HashMap<u64, Instant>,
    last_warning_notification: Option<Instant>,
    last_critical_notification: Option<Instant>,
    state_evictions: u64,
}

impl AlertStateMachine {
    pub(crate) fn new(config: LlmAlertsConfig) -> Self {
        Self {
            config,
            level: AlertLevel::Healthy,
            state_since: None,
            send_timeouts: VecDeque::new(),
            background_exhausted: VecDeque::new(),
            all_exhausted: VecDeque::new(),
            seen_exhausted: HashMap::new(),
            last_warning_notification: None,
            last_critical_notification: None,
            state_evictions: 0,
        }
    }

    pub(crate) fn on_event(&mut self, now: Instant, event: ProviderEvent) -> Vec<AlertAction> {
        self.prune(now);
        match event.kind {
            ProviderEventKind::SendTimeout => {
                self.send_timeouts.push_back(now);
                self.maybe_warning(now)
            }
            ProviderEventKind::RetryExhausted => {
                if self.seen_exhausted.insert(event.call_id, now).is_some() {
                    return Vec::new();
                }
                self.all_exhausted.push_back(now);
                let critical = match event.workload {
                    WorkloadKind::Interactive => self.config.critical.interactive_exhausted <= 1,
                    _ => {
                        self.background_exhausted.push_back(now);
                        self.background_exhausted.len() >= self.config.critical.background_exhausted
                    }
                };
                if critical {
                    self.maybe_critical(now)
                } else {
                    Vec::new()
                }
            }
            ProviderEventKind::RecoveredAfterTimeout => Vec::new(),
        }
    }

    pub(crate) fn on_tick(&mut self, now: Instant) -> Vec<AlertAction> {
        self.prune(now);
        if self.level == AlertLevel::Healthy {
            return Vec::new();
        }
        let Some(since) = self.state_since else {
            return Vec::new();
        };
        if now.duration_since(since) < Duration::from_secs(self.config.recovery.quiet_window_secs)
            || self.send_timeouts.len() > self.config.recovery.max_send_timeouts
            || !self.all_exhausted.is_empty()
        {
            return Vec::new();
        }
        self.level = AlertLevel::Healthy;
        self.state_since = None;
        self.last_warning_notification = None;
        self.last_critical_notification = None;
        if self.config.recovery.notify {
            vec![AlertAction::NotifyRecovery(self.report())]
        } else {
            Vec::new()
        }
    }

    pub(crate) fn snapshot(&self) -> AlertSnapshot {
        AlertSnapshot {
            level: self.level,
            available: true,
            reason: "active".into(),
            send_timeouts: self.warning_timeout_count(Instant::now()),
            exhausted_calls: self.all_exhausted.len(),
            dropped_events: tyclaw_provider::provider_event_dropped_count(),
            state_evictions: self.state_evictions,
            notification_status: "idle".into(),
            last_error_kind: None,
        }
    }

    fn maybe_warning(&mut self, now: Instant) -> Vec<AlertAction> {
        let warning_timeouts = self.warning_timeout_count(now);
        if self.level == AlertLevel::Critical
            || warning_timeouts < self.config.warning.min_send_timeouts
        {
            return Vec::new();
        }
        let may_notify = self.level == AlertLevel::Healthy
            || self.last_warning_notification.is_none_or(|last| {
                now.duration_since(last) >= Duration::from_secs(self.config.warning.cooldown_secs)
            });
        if !may_notify {
            return Vec::new();
        }
        if self.level == AlertLevel::Healthy {
            self.state_since = Some(now);
        }
        self.level = AlertLevel::Warning;
        self.last_warning_notification = Some(now);
        vec![AlertAction::NotifyWarning(self.report())]
    }

    fn maybe_critical(&mut self, now: Instant) -> Vec<AlertAction> {
        let may_notify = self.level != AlertLevel::Critical
            || self.last_critical_notification.is_none_or(|last| {
                now.duration_since(last) >= Duration::from_secs(self.config.critical.cooldown_secs)
            });
        if !may_notify {
            return Vec::new();
        }
        if self.level == AlertLevel::Healthy {
            self.state_since = Some(now);
        }
        self.level = AlertLevel::Critical;
        self.last_critical_notification = Some(now);
        vec![AlertAction::NotifyCritical(self.report())]
    }

    fn report(&self) -> AlertReport {
        AlertReport {
            level: self.level,
            send_timeouts: self.warning_timeout_count(Instant::now()),
            exhausted_calls: self.all_exhausted.len(),
        }
    }

    fn warning_timeout_count(&self, now: Instant) -> usize {
        let cutoff = now - Duration::from_secs(self.config.warning.window_secs);
        self.send_timeouts
            .iter()
            .filter(|time| **time > cutoff)
            .count()
    }

    fn prune(&mut self, now: Instant) {
        prune_queue(
            &mut self.send_timeouts,
            now,
            self.config
                .warning
                .window_secs
                .max(self.config.recovery.quiet_window_secs),
        );
        prune_queue(
            &mut self.background_exhausted,
            now,
            self.config.critical.background_window_secs,
        );
        prune_queue(
            &mut self.all_exhausted,
            now,
            self.config.recovery.quiet_window_secs,
        );
        let retention = Duration::from_secs(
            self.config
                .critical
                .background_window_secs
                .max(self.config.recovery.quiet_window_secs)
                + 300,
        );
        self.seen_exhausted
            .retain(|_, seen| now.duration_since(*seen) < retention);
        const MAX_CALL_STATES: usize = 100_000;
        while self.seen_exhausted.len() > MAX_CALL_STATES {
            if let Some(oldest) = self
                .seen_exhausted
                .iter()
                .min_by_key(|(_, seen)| **seen)
                .map(|(id, _)| *id)
            {
                self.seen_exhausted.remove(&oldest);
                self.state_evictions += 1;
            }
        }
    }
}

fn prune_queue(queue: &mut VecDeque<Instant>, now: Instant, window_secs: u64) {
    let cutoff = now - Duration::from_secs(window_secs);
    while queue.front().is_some_and(|time| *time <= cutoff) {
        queue.pop_front();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationErrorKind {
    Token,
    Unauthorized,
    RemoteStatus,
    Transport,
}

#[derive(Debug)]
pub(crate) struct NotificationError {
    kind: NotificationErrorKind,
}

#[async_trait::async_trait]
pub(crate) trait AlertNotifier: Send + Sync {
    async fn notify(&self, action: &AlertAction) -> Result<(), NotificationError>;
}

pub(crate) struct DingTalkNotifier {
    sender: tyclaw_channel::dingtalk::AdminMarkdownSender,
    token_manager: tyclaw_channel::dingtalk::TokenManager,
    robot_code: String,
    admin_user_ids: Vec<String>,
    timeout: Duration,
    instance: String,
}

impl DingTalkNotifier {
    pub(crate) fn new(
        token_manager: tyclaw_channel::dingtalk::TokenManager,
        robot_code: String,
        config: &NotificationConfig,
        instance: String,
    ) -> Self {
        Self {
            sender: tyclaw_channel::dingtalk::AdminMarkdownSender::new(),
            token_manager,
            robot_code,
            admin_user_ids: config.admin_user_ids.clone(),
            timeout: Duration::from_secs(config.timeout_secs),
            instance,
        }
    }

    async fn send(&self, title: &str, text: &str) -> Result<(), NotificationError> {
        let token = self
            .token_manager
            .get_token()
            .await
            .map_err(|_| NotificationError {
                kind: NotificationErrorKind::Token,
            })?;
        let result = self
            .sender
            .send(
                &token,
                &self.robot_code,
                &self.admin_user_ids,
                title,
                text,
                self.timeout,
            )
            .await;
        if matches!(
            result.as_ref().err().map(|e| e.kind()),
            Some(tyclaw_channel::dingtalk::AdminSendErrorKind::Unauthorized)
        ) {
            self.token_manager.reset().await;
            let token = self
                .token_manager
                .get_token()
                .await
                .map_err(|_| NotificationError {
                    kind: NotificationErrorKind::Token,
                })?;
            return self
                .sender
                .send(
                    &token,
                    &self.robot_code,
                    &self.admin_user_ids,
                    title,
                    text,
                    self.timeout,
                )
                .await
                .map_err(map_send_error);
        }
        result.map_err(map_send_error)
    }
}

fn map_send_error(error: tyclaw_channel::dingtalk::AdminSendError) -> NotificationError {
    use tyclaw_channel::dingtalk::AdminSendErrorKind;
    let kind = match error.kind() {
        AdminSendErrorKind::Unauthorized => NotificationErrorKind::Unauthorized,
        AdminSendErrorKind::RemoteStatus => NotificationErrorKind::RemoteStatus,
        AdminSendErrorKind::Transport | AdminSendErrorKind::InvalidInput => {
            NotificationErrorKind::Transport
        }
    };
    NotificationError { kind }
}

#[async_trait::async_trait]
impl AlertNotifier for DingTalkNotifier {
    async fn notify(&self, action: &AlertAction) -> Result<(), NotificationError> {
        let (title, report) = match action {
            AlertAction::NotifyWarning(report) => ("[TyClaw][LLM告警] Provider 超时升高", report),
            AlertAction::NotifyCritical(report) => {
                ("[TyClaw][LLM严重告警] Provider 重试耗尽", report)
            }
            AlertAction::NotifyRecovery(report) => ("[TyClaw][LLM恢复] Provider 链路恢复", report),
        };
        let text = format!(
            "### {title}\n- 实例：{}\n- 级别：{:?}\n- 当前窗口发送超时：{}\n- 当前窗口最终耗尽：{}",
            self.instance, report.level, report.send_timeouts, report.exhausted_calls
        );
        self.send(title, &text).await
    }
}

pub(crate) async fn run_alert_manager(
    config: LlmAlertsConfig,
    mut receiver: mpsc::Receiver<ProviderEvent>,
    snapshot: Arc<RwLock<AlertSnapshot>>,
    notifier: Option<Arc<dyn AlertNotifier>>,
) {
    let mut state = AlertStateMachine::new(config.clone());
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        let actions = tokio::select! {
            _ = interval.tick() => state.on_tick(Instant::now()),
            event = receiver.recv() => match event {
                Some(event) => state.on_event(Instant::now(), event),
                None => {
                    let mut current = state.snapshot();
                    current.available = false;
                    current.reason = "event_source_closed".into();
                    current.notification_status = snapshot.read().notification_status.clone();
                    *snapshot.write() = current;
                    return;
                }
            },
        };
        let previous_notification = snapshot.read().notification_status.clone();
        let previous_error = snapshot.read().last_error_kind.clone();
        let mut current = state.snapshot();
        current.notification_status = previous_notification;
        current.last_error_kind = previous_error;
        *snapshot.write() = current;

        for action in actions {
            let Some(notifier) = notifier.as_ref() else {
                let mut snap = snapshot.write();
                snap.notification_status = "unavailable".into();
                snap.last_error_kind = Some("notification_unavailable".into());
                continue;
            };
            let result = tokio::time::timeout(
                Duration::from_secs(config.notification.timeout_secs),
                notifier.notify(&action),
            )
            .await;
            let mut snap = snapshot.write();
            match result {
                Ok(Ok(())) => {
                    snap.notification_status = "sent".into();
                    snap.last_error_kind = None;
                }
                Ok(Err(error)) => {
                    snap.notification_status = "failed".into();
                    snap.last_error_kind = Some(format!("{:?}", error.kind).to_ascii_lowercase());
                }
                Err(_) => {
                    snap.notification_status = "failed".into();
                    snap.last_error_kind = Some("timeout".into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use tyclaw_provider::{ProviderEvent, ProviderEventKind, TransportKind, WorkloadKind};

    struct HangingNotifier;

    #[async_trait::async_trait]
    impl AlertNotifier for HangingNotifier {
        async fn notify(&self, _action: &AlertAction) -> Result<(), NotificationError> {
            std::future::pending().await
        }
    }

    fn event(call_id: u64, kind: ProviderEventKind, workload: WorkloadKind) -> ProviderEvent {
        ProviderEvent {
            occurred_at: SystemTime::now(),
            call_id,
            kind,
            transport: (kind == ProviderEventKind::SendTimeout).then_some(TransportKind::Sse),
            attempt: (kind == ProviderEventKind::SendTimeout).then_some(1),
            model: "test-model".into(),
            provider_origin: "https://provider.test".into(),
            workload,
        }
    }

    fn machine() -> AlertStateMachine {
        AlertStateMachine::new(LlmAlertsConfig::enabled_defaults())
    }

    #[test]
    fn defaults_are_disabled_and_match_approved_thresholds() {
        let cfg: LlmAlertsConfig = serde_yaml::from_str("{}").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.warning.window_secs, 900);
        assert_eq!(cfg.warning.min_send_timeouts, 15);
        assert_eq!(cfg.critical.background_exhausted, 2);
    }

    #[test]
    fn invalid_config_does_not_expose_admin_ids() {
        let cfg: LlmAlertsConfig = serde_yaml::from_str(
            "enabled: true\nwarning: { window_secs: 0 }\nnotification: { admin_user_ids: [secret-admin-id] }",
        )
        .unwrap();
        assert!(cfg.clone().validated().is_err());
        assert!(!format!("{cfg:?}").contains("secret-admin-id"));
    }

    #[test]
    fn warning_threshold_and_cooldown_are_enforced() {
        let start = tokio::time::Instant::now();
        let mut state = machine();
        for id in 1..15 {
            assert!(state
                .on_event(
                    start,
                    event(
                        id,
                        ProviderEventKind::SendTimeout,
                        WorkloadKind::Interactive
                    )
                )
                .is_empty());
        }
        assert!(matches!(
            state
                .on_event(
                    start,
                    event(
                        15,
                        ProviderEventKind::SendTimeout,
                        WorkloadKind::Interactive
                    )
                )
                .as_slice(),
            [AlertAction::NotifyWarning(_)]
        ));
        assert!(state
            .on_event(
                start + std::time::Duration::from_secs(7199),
                event(
                    16,
                    ProviderEventKind::SendTimeout,
                    WorkloadKind::Interactive
                )
            )
            .is_empty());
        let later = start + std::time::Duration::from_secs(7200);
        let mut warned = false;
        for id in 17..=31 {
            warned |= state
                .on_event(
                    later,
                    event(
                        id,
                        ProviderEventKind::SendTimeout,
                        WorkloadKind::Interactive,
                    ),
                )
                .iter()
                .any(|action| matches!(action, AlertAction::NotifyWarning(_)));
        }
        assert!(warned);
    }

    #[test]
    fn warning_counts_only_the_fifteen_minute_window() {
        let start = tokio::time::Instant::now();
        let mut state = machine();
        for id in 1..=14 {
            state.on_event(
                start,
                event(
                    id,
                    ProviderEventKind::SendTimeout,
                    WorkloadKind::Interactive,
                ),
            );
        }
        let actions = state.on_event(
            start + Duration::from_secs(901),
            event(
                15,
                ProviderEventKind::SendTimeout,
                WorkloadKind::Interactive,
            ),
        );
        assert!(actions.is_empty());
        assert_eq!(state.snapshot().level, AlertLevel::Healthy);
    }

    #[test]
    fn interactive_is_immediate_and_background_is_windowed_and_deduplicated() {
        let start = tokio::time::Instant::now();
        let mut interactive = machine();
        assert!(matches!(
            interactive
                .on_event(
                    start,
                    event(
                        1,
                        ProviderEventKind::RetryExhausted,
                        WorkloadKind::Interactive
                    )
                )
                .as_slice(),
            [AlertAction::NotifyCritical(_)]
        ));

        let mut background = machine();
        assert!(background
            .on_event(
                start,
                event(2, ProviderEventKind::RetryExhausted, WorkloadKind::Memory)
            )
            .is_empty());
        assert!(background
            .on_event(
                start,
                event(2, ProviderEventKind::RetryExhausted, WorkloadKind::Memory)
            )
            .is_empty());
        assert!(matches!(
            background
                .on_event(
                    start + std::time::Duration::from_secs(3599),
                    event(3, ProviderEventKind::RetryExhausted, WorkloadKind::Timer)
                )
                .as_slice(),
            [AlertAction::NotifyCritical(_)]
        ));

        let mut outside = machine();
        assert!(outside
            .on_event(
                start,
                event(4, ProviderEventKind::RetryExhausted, WorkloadKind::Memory)
            )
            .is_empty());
        assert!(outside
            .on_event(
                start + std::time::Duration::from_secs(3601),
                event(5, ProviderEventKind::RetryExhausted, WorkloadKind::Memory)
            )
            .is_empty());
    }

    #[test]
    fn warning_escalates_immediately_and_critical_never_downgrades() {
        let start = tokio::time::Instant::now();
        let mut state = machine();
        for id in 1..=15 {
            state.on_event(
                start,
                event(
                    id,
                    ProviderEventKind::SendTimeout,
                    WorkloadKind::Interactive,
                ),
            );
        }
        assert_eq!(state.snapshot().level, AlertLevel::Warning);
        assert!(matches!(
            state
                .on_event(
                    start,
                    event(
                        20,
                        ProviderEventKind::RetryExhausted,
                        WorkloadKind::Interactive
                    )
                )
                .as_slice(),
            [AlertAction::NotifyCritical(_)]
        ));
        state.on_tick(start + std::time::Duration::from_secs(901));
        assert_eq!(state.snapshot().level, AlertLevel::Critical);
    }

    #[test]
    fn recovery_requires_full_quiet_window_and_resets_cooldowns() {
        let start = tokio::time::Instant::now();
        let mut state = machine();
        state.on_event(
            start,
            event(
                1,
                ProviderEventKind::RetryExhausted,
                WorkloadKind::Interactive,
            ),
        );
        assert!(state
            .on_tick(start + std::time::Duration::from_secs(1799))
            .is_empty());
        assert!(matches!(
            state
                .on_tick(start + std::time::Duration::from_secs(1800))
                .as_slice(),
            [AlertAction::NotifyRecovery(_)]
        ));
        assert_eq!(state.snapshot().level, AlertLevel::Healthy);
        assert!(matches!(
            state
                .on_event(
                    start + std::time::Duration::from_secs(1801),
                    event(
                        2,
                        ProviderEventKind::RetryExhausted,
                        WorkloadKind::Interactive
                    )
                )
                .as_slice(),
            [AlertAction::NotifyCritical(_)]
        ));
    }

    #[test]
    fn historical_baseline_only_warns_on_strong_windows() {
        let peaks = [
            (12, false),
            (13, false),
            (12, false),
            (6, false),
            (13, false),
            (18, true),
            (29, true),
        ];
        for (count, should_warn) in peaks {
            let start = tokio::time::Instant::now();
            let mut state = machine();
            let mut warned = false;
            for id in 0..count {
                warned |= state
                    .on_event(
                        start,
                        event(id, ProviderEventKind::SendTimeout, WorkloadKind::Memory),
                    )
                    .iter()
                    .any(|action| matches!(action, AlertAction::NotifyWarning(_)));
            }
            assert_eq!(warned, should_warn, "peak={count}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn notification_timeout_is_failed_open_and_manager_keeps_running() {
        let mut config = LlmAlertsConfig::enabled_defaults();
        config.notification.timeout_secs = 5;
        let (tx, rx) = mpsc::channel(64);
        let snapshot = Arc::new(RwLock::new(AlertSnapshot::disabled()));
        let task = tokio::spawn(run_alert_manager(
            config,
            rx,
            snapshot.clone(),
            Some(Arc::new(HangingNotifier)),
        ));
        for id in 1..=15 {
            tx.send(event(
                id,
                ProviderEventKind::SendTimeout,
                WorkloadKind::Interactive,
            ))
            .await
            .unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert_eq!(snapshot.read().notification_status, "failed");
        assert_eq!(snapshot.read().last_error_kind.as_deref(), Some("timeout"));
        assert!(!task.is_finished());
        tx.send(event(
            16,
            ProviderEventKind::SendTimeout,
            WorkloadKind::Interactive,
        ))
        .await
        .unwrap();
        task.abort();
    }

    #[test]
    fn disabled_invalid_and_missing_admin_config_are_non_fatal() {
        let disabled = AlertRuntime::prepare(LlmAlertsConfig::default());
        assert_eq!(disabled.snapshot.read().reason, "disabled");

        let mut invalid_config = LlmAlertsConfig::enabled_defaults();
        invalid_config.warning.window_secs = 0;
        let invalid = AlertRuntime::prepare(invalid_config);
        assert_eq!(invalid.snapshot.read().reason, "config_invalid");

        let missing_admin = AlertRuntime::prepare(LlmAlertsConfig::enabled_defaults());
        assert_eq!(missing_admin.snapshot.read().reason, "active");
        assert_eq!(
            missing_admin.snapshot.read().notification_status,
            "unavailable"
        );
        tyclaw_provider::install_provider_event_sink(None);
    }
}
