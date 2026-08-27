use arc_swap::ArcSwapOption;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, OnceLock,
};
use std::time::SystemTime;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    Interactive,
    Timer,
    Subtask,
    Memory,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Sse,
    NonStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderEventKind {
    SendTimeout,
    RecoveredAfterTimeout,
    RetryExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEvent {
    pub occurred_at: SystemTime,
    pub call_id: u64,
    pub kind: ProviderEventKind,
    pub transport: Option<TransportKind>,
    pub attempt: Option<usize>,
    pub model: String,
    pub provider_origin: String,
    pub workload: WorkloadKind,
}

#[derive(Debug)]
pub(crate) struct LlmCallContext {
    pub(crate) id: u64,
    saw_timeout: AtomicBool,
}

impl LlmCallContext {
    pub(crate) fn mark_timeout(&self) {
        self.saw_timeout.store(true, Ordering::Relaxed);
    }

    pub(crate) fn saw_timeout(&self) -> bool {
        self.saw_timeout.load(Ordering::Relaxed)
    }
}

tokio::task_local! {
    pub static CURRENT_WORKLOAD_KIND: WorkloadKind;
    pub(crate) static CURRENT_LLM_CALL: Arc<LlmCallContext>;
}

#[derive(Clone)]
pub struct ProviderEventSink {
    sender: mpsc::Sender<ProviderEvent>,
    dropped: Arc<AtomicU64>,
}

impl ProviderEventSink {
    pub fn new(sender: mpsc::Sender<ProviderEvent>, dropped: Arc<AtomicU64>) -> Self {
        Self { sender, dropped }
    }

    pub fn try_emit(&self, event: ProviderEvent) {
        if self.sender.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);
static EVENT_SINK: OnceLock<ArcSwapOption<ProviderEventSink>> = OnceLock::new();
#[cfg(test)]
pub(crate) static EVENT_SINK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn event_sink() -> &'static ArcSwapOption<ProviderEventSink> {
    EVENT_SINK.get_or_init(ArcSwapOption::empty)
}

pub fn install_provider_event_sink(sink: Option<Arc<ProviderEventSink>>) {
    event_sink().store(sink);
}

pub fn provider_event_dropped_count() -> u64 {
    event_sink()
        .load_full()
        .as_deref()
        .map_or(0, ProviderEventSink::dropped_count)
}

pub(crate) fn next_call_context() -> Arc<LlmCallContext> {
    Arc::new(LlmCallContext {
        id: NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed),
        saw_timeout: AtomicBool::new(false),
    })
}

fn workload_kind() -> WorkloadKind {
    CURRENT_WORKLOAD_KIND
        .try_with(|kind| *kind)
        .unwrap_or(WorkloadKind::Unknown)
}

fn current_or_fallback_context() -> Arc<LlmCallContext> {
    CURRENT_LLM_CALL
        .try_with(Arc::clone)
        .unwrap_or_else(|_| next_call_context())
}

pub(crate) fn emit_send_timeout(
    transport: TransportKind,
    attempt: usize,
    model: &str,
    endpoint: &str,
) {
    let context = current_or_fallback_context();
    context.mark_timeout();
    emit(ProviderEvent {
        occurred_at: SystemTime::now(),
        call_id: context.id,
        kind: ProviderEventKind::SendTimeout,
        transport: Some(transport),
        attempt: Some(attempt),
        model: model.to_string(),
        provider_origin: sanitize_endpoint(endpoint),
        workload: workload_kind(),
    });
}

pub(crate) fn emit_call_outcome(kind: ProviderEventKind, model: &str, endpoint: &str) {
    let context = current_or_fallback_context();
    emit(ProviderEvent {
        occurred_at: SystemTime::now(),
        call_id: context.id,
        kind,
        transport: None,
        attempt: None,
        model: model.to_string(),
        provider_origin: sanitize_endpoint(endpoint),
        workload: workload_kind(),
    });
}

fn emit(event: ProviderEvent) {
    if let Some(sink) = event_sink().load_full() {
        sink.try_emit(event);
    }
}

pub fn sanitize_endpoint(endpoint: &str) -> String {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return "invalid-endpoint".to_string();
    };
    let Some(host) = url.host_str() else {
        return "invalid-endpoint".to_string();
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc, MutexGuard,
    };

    struct SinkResetGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl SinkResetGuard {
        fn acquire() -> Self {
            let lock = EVENT_SINK_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            install_provider_event_sink(None);
            Self { _lock: lock }
        }
    }

    impl Drop for SinkResetGuard {
        fn drop(&mut self) {
            install_provider_event_sink(None);
        }
    }

    fn timeout_event(call_id: u64) -> ProviderEvent {
        ProviderEvent {
            occurred_at: std::time::SystemTime::now(),
            call_id,
            kind: ProviderEventKind::SendTimeout,
            transport: Some(TransportKind::Sse),
            attempt: Some(1),
            model: "model".into(),
            provider_origin: sanitize_endpoint(
                "https://user:pass@example.com:8990/v1/chat?key=secret",
            ),
            workload: WorkloadKind::Unknown,
        }
    }

    #[test]
    fn endpoint_keeps_only_origin() {
        assert_eq!(
            sanitize_endpoint("https://user:pass@example.com:8990/v1/chat?key=secret"),
            "https://example.com:8990"
        );
        assert_eq!(sanitize_endpoint("not a url"), "invalid-endpoint");
    }

    #[test]
    fn event_debug_output_excludes_endpoint_secrets() {
        let output = format!("{:?}", timeout_event(1));
        assert!(!output.contains("secret"));
        assert!(!output.contains("pass"));
        assert!(!output.contains("/v1/chat"));
    }

    #[tokio::test]
    async fn full_or_closed_sink_never_blocks_or_panics() {
        let _reset = SinkResetGuard::acquire();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let sink = ProviderEventSink::new(tx, dropped.clone());
        let event = timeout_event(1);

        sink.try_emit(event.clone());
        sink.try_emit(event.clone());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(rx.recv().await.unwrap().call_id, 1);

        drop(rx);
        sink.try_emit(event);
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn full_sink_drops_ten_thousand_events_without_waiting_for_capacity() {
        let _reset = SinkResetGuard::acquire();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let sink = ProviderEventSink::new(tx, dropped.clone());
        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            for call_id in 0..10_000 {
                sink.try_emit(timeout_event(call_id));
            }
        })
        .await
        .expect("try_emit must not wait for channel capacity");
        assert_eq!(dropped.load(Ordering::Relaxed), 9_999);
    }
}
