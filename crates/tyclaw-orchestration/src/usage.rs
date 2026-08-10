//! 编排层内部使用统计收集器，不进入公开请求或工具协议。

use std::cell::RefCell;
use std::future::Future;

use tyclaw_agent::runtime::ToolExecutionEvent;
use tyclaw_control::UsageToolEvent;

tokio::task_local! {
    static TOOL_EVENTS: RefCell<Vec<UsageToolEvent>>;
}

pub(crate) async fn collect_request_tools<F>(future: F) -> (F::Output, Vec<UsageToolEvent>)
where
    F: Future,
{
    TOOL_EVENTS
        .scope(RefCell::new(Vec::new()), async move {
            let output = future.await;
            let events = TOOL_EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()));
            (output, events)
        })
        .await
}

pub(crate) fn collect_tool_events(scope: &str, events: &[ToolExecutionEvent]) {
    if events.is_empty() {
        return;
    }
    let scope = scope.to_string();
    let _ = TOOL_EVENTS.try_with(|collector| {
        collector
            .borrow_mut()
            .extend(events.iter().map(|event| UsageToolEvent {
                scope: scope.clone(),
                name: event.tool_name.clone(),
                status: event.status.clone(),
                route: event.route.clone(),
                risk_level: event.risk_level.clone(),
                duration_ms: event.duration_ms,
            }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn collector_keeps_only_tool_summary_fields_and_scope() {
        let event = ToolExecutionEvent {
            tool_name: "read_file".into(),
            status: "ok".into(),
            route: "sandbox".into(),
            risk_level: "read".into(),
            duration_ms: 9,
            result_preview: "sensitive result".into(),
            ..ToolExecutionEvent::default()
        };
        let (value, events) = collect_request_tools(async {
            collect_tool_events("sub:lookup", &[event]);
            42
        })
        .await;
        assert_eq!(value, 42);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scope, "sub:lookup");
        assert_eq!(events[0].name, "read_file");
        assert_eq!(events[0].duration_ms, 9);
        let json = serde_json::to_string(&events).unwrap();
        assert!(!json.contains("sensitive result"));
    }
}
