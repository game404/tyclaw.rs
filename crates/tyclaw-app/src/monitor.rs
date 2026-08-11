//! 轻量监控 HTTP 服务 —— 可配置 bind/port，可选 Basic 认证。
//!
//! 端点：GET / HTML；GET /api/stats、GET /api/analytics JSON。

use chrono::{Datelike, Duration, NaiveDate};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tyclaw_control::{AnalyticsQuery, AuditEntry, UsageSource, WorkspaceKeyStrategy};
use tyclaw_orchestration::Orchestrator;

#[derive(Clone)]
pub struct MonitorOptions {
    pub bind: String,
    pub port: u16,
    pub basic_auth: Option<(String, String)>,
    pub hide_content: bool,
}

pub fn spawn_monitor(orchestrator: Arc<Orchestrator>, options: Option<MonitorOptions>) {
    let Some(opts) = options else { return };
    let addr = format!("{}:{}", opts.bind.trim(), opts.port);
    let basic = opts.basic_auth.clone();
    let hide_content = opts.hide_content;
    tokio::spawn(async move {
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => {
                tracing::info!(addr = %addr, "Monitor HTTP server started");
                l
            }
            Err(e) => {
                tracing::warn!(error = %e, addr = %addr, "Failed to start monitor server");
                return;
            }
        };
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let orch = Arc::clone(&orchestrator);
            let basic = basic.clone();
            let hide_content = hide_content;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                let n = match stream.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let Some((method, target, headers)) = parse_http_request_headers(&request) else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = stream.shutdown().await;
                    return;
                };
                let need_auth = basic.is_some();
                let authorized = if let Some((ref u, ref p)) = basic {
                    headers
                        .get("authorization")
                        .map(|v| check_basic_auth(v, u, p))
                        .unwrap_or(false)
                } else {
                    true
                };
                if need_auth && !authorized {
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"TyClaw Monitor\"\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                        )
                        .await;
                    let _ = stream.shutdown().await;
                    return;
                }
                let (path, raw_query) = target
                    .split_once('?')
                    .unwrap_or((target.as_str(), ""));
                let response = match (method.as_str(), path) {
                    ("GET", "/api/stats") => http_response(
                        "200 OK",
                        "application/json; charset=utf-8",
                        &build_stats_json(&orch, hide_content),
                    ),
                    ("GET", "/api/analytics") => {
                        build_analytics_response(&orch, raw_query, hide_content).await
                    }
                    ("GET", "/") => {
                        http_response("200 OK", "text/html; charset=utf-8", &build_html_page())
                    }
                    ("GET", _) => http_json_error("404 Not Found", "not_found"),
                    _ => http_json_error("405 Method Not Allowed", "method_not_allowed"),
                };
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
}

fn parse_http_request_headers(raw: &str) -> Option<(String, String, HashMap<String, String>)> {
    let head_end = raw.find("\r\n\r\n").or_else(|| raw.find("\n\n"))?;
    let head = &raw[..head_end];
    let mut lines = head.lines();
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Some((method, path, headers))
}

fn http_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn http_json_error(status: &str, code: &str) -> String {
    let body = serde_json::json!({ "error": code }).to_string();
    http_response(status, "application/json; charset=utf-8", &body)
}

fn check_basic_auth(header_value: &str, expect_user: &str, expect_password: &str) -> bool {
    check_basic_auth_inner(header_value, expect_user, expect_password).unwrap_or(false)
}

fn check_basic_auth_inner(
    header_value: &str,
    expect_user: &str,
    expect_password: &str,
) -> Option<bool> {
    let rest = header_value
        .strip_prefix("Basic ")
        .or_else(|| header_value.strip_prefix("basic "))?;
    let decoded = base64_decode(rest.trim())?;
    let decoded = String::from_utf8_lossy(&decoded);
    let (user, password) = split_basic_credentials(&decoded)?;
    Some(ct_eq_str(user, expect_user) && ct_eq_str(password, expect_password))
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .ok()
}

fn split_basic_credentials(s: &str) -> Option<(&str, &str)> {
    let idx = s.find(':')?;
    Some((&s[..idx], &s[idx + 1..]))
}

fn ct_eq_str(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        d |= x ^ y;
    }
    d == 0
}

async fn build_analytics_response(
    orch: &Orchestrator,
    raw_query: &str,
    hide_content: bool,
) -> String {
    let Some(analytics) = orch.analytics().cloned() else {
        return http_json_error("503 Service Unavailable", "analytics_not_configured");
    };
    let query = match parse_analytics_query(
        raw_query,
        analytics.today(),
        analytics.aggregate_retention_days(),
    ) {
        Ok(query) => query,
        Err(error) => return http_json_error("400 Bad Request", error),
    };
    match tokio::task::spawn_blocking(move || analytics.query(&query)).await {
        Ok(Ok(report)) => match serialize_analytics_report(&report, hide_content) {
            Ok(body) => http_response("200 OK", "application/json; charset=utf-8", &body),
            Err(_) => http_json_error("500 Internal Server Error", "analytics_encode_failed"),
        },
        Ok(Err(error)) if error == "analytics_disabled" || error.contains("unavailable") => {
            http_json_error("503 Service Unavailable", &error)
        }
        Ok(Err(error)) => http_json_error("500 Internal Server Error", &error),
        Err(_) => http_json_error("500 Internal Server Error", "analytics_query_task_failed"),
    }
}

fn serialize_analytics_report<T: serde::Serialize>(
    report: &T,
    hide_content: bool,
) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(report)?;
    if let Some(root) = value.as_object_mut() {
        root.insert("privacy_mode".into(), serde_json::json!(hide_content));
        if hide_content {
            if let Some(recent) = root.get_mut("recent").and_then(|rows| rows.as_array_mut()) {
                for row in recent {
                    if let Some(fields) = row.as_object_mut() {
                        fields.remove("request_preview");
                        fields.remove("response_preview");
                    }
                }
            }
        }
    }
    serde_json::to_string(&value)
}

fn parse_analytics_query(
    raw_query: &str,
    today: NaiveDate,
    retention_days: u32,
) -> Result<AnalyticsQuery, &'static str> {
    if raw_query.len() > 2048 {
        return Err("analytics_query_too_long");
    }
    let mut params = HashMap::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(raw_key).ok_or("analytics_invalid_query_encoding")?;
        let value = percent_decode(raw_value).ok_or("analytics_invalid_query_encoding")?;
        if !matches!(
            key.as_str(),
            "range" | "from" | "to" | "workspace" | "channel" | "source"
        ) {
            return Err("analytics_unknown_query_parameter");
        }
        if params.insert(key, value).is_some() {
            return Err("analytics_duplicate_query_parameter");
        }
    }

    let range = params.get("range").map(String::as_str);
    if range.is_some() && (params.contains_key("from") || params.contains_key("to")) {
        return Err("analytics_ambiguous_date_range");
    }
    let (from, to) = match range {
        Some("today") => (today, today),
        Some("week") => (
            today - Duration::days(today.weekday().num_days_from_monday() as i64),
            today,
        ),
        Some("month") | None if !params.contains_key("from") && !params.contains_key("to") => (
            today.with_day(1).ok_or("analytics_invalid_date_range")?,
            today,
        ),
        Some(_) => return Err("analytics_invalid_range"),
        None => {
            let to = params
                .get("to")
                .map(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d"))
                .transpose()
                .map_err(|_| "analytics_invalid_to_date")?
                .unwrap_or(today);
            let from = params
                .get("from")
                .map(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d"))
                .transpose()
                .map_err(|_| "analytics_invalid_from_date")?
                .unwrap_or_else(|| to.with_day(1).unwrap_or(to));
            (from, to)
        }
    };
    let requested_days = (to - from).num_days() + 1;
    if from > to || requested_days > retention_days.min(400) as i64 {
        return Err("analytics_invalid_date_range");
    }

    let optional_filter = |name: &str| -> Result<Option<String>, &'static str> {
        match params.get(name).map(|value| value.trim()) {
            Some(value) if value.is_empty() || value.chars().count() > 128 => {
                Err("analytics_invalid_filter")
            }
            Some(value) => Ok(Some(value.to_string())),
            None => Ok(None),
        }
    };
    let source = params
        .get("source")
        .map(|value| value.parse::<UsageSource>())
        .transpose()
        .map_err(|_| "analytics_invalid_source")?;

    Ok(AnalyticsQuery {
        from,
        to,
        workspace: optional_filter("workspace")?,
        channel: optional_filter("channel")?,
        source,
    })
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value(bytes[index + 1])?;
                let low = hex_value(bytes[index + 2])?;
                decoded.push(high * 16 + low);
                index += 2;
            }
            b'%' => return None,
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn build_stats_json(orch: &Orchestrator, hide_content: bool) -> String {
    let active_tasks = {
        let tasks = orch.active_tasks().lock();
        tasks
            .iter()
            .map(|(k, v)| {
                serde_json::json!({
                    "workspace": k,
                    "user_id": v.user_id,
                    "summary": v.summary,
                    "elapsed_secs": v.started_at.elapsed().as_secs(),
                })
            })
            .collect::<Vec<_>>()
    };
    let audit_entries = orch
        .persistence()
        .audit
        .query(None, None, None, 20)
        .iter()
        .map(|e| audit_entry_json(e, hide_content))
        .collect::<Vec<_>>();
    let skills = {
        let metas = orch.persistence().skills.scan_builtin();
        metas
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "category": s.category,
                    "description": s.description,
                    "status": s.status,
                })
            })
            .collect::<Vec<_>>()
    };
    let works_stats = build_works_stats(orch);
    let app = orch.app();
    serde_json::json!({
        "model": app.model,
        "workspace": app.workspace.display().to_string(),
        "context_window": app.context_window_tokens,
        "active_tasks": active_tasks,
        "active_task_count": active_tasks.len(),
        "audit_recent": audit_entries,
        "privacy_mode": hide_content,
        "skills": skills,
        "skill_count": skills.len(),
        "works_stats": works_stats,
    })
    .to_string()
}

fn audit_entry_json(entry: &AuditEntry, hide_content: bool) -> serde_json::Value {
    let mut value = serde_json::json!({
        "time": entry.timestamp.format("%H:%M:%S").to_string(),
        "user": entry.user_name,
        "channel": entry.channel,
        "tools": entry.tool_calls.len(),
        "duration": entry.total_duration.map(|duration| format!("{duration:.1}s")),
    });
    if !hide_content {
        if let Some(fields) = value.as_object_mut() {
            fields.insert(
                "request".into(),
                serde_json::json!(truncate(&entry.request, 80)),
            );
            fields.insert(
                "response".into(),
                serde_json::json!(entry
                    .final_response
                    .as_deref()
                    .map(|response| truncate(response, 100))),
            );
        }
    }
    value
}

fn build_works_stats(orch: &Orchestrator) -> serde_json::Value {
    // 注意：`list_workspace_keys()` 返回的是 **磁盘 leaf**（含 `/ \ : + =` 字符已被
    // `filesystem_workspace_leaf` 替换为 `_`），不再等于原始钉钉 conversation_id。
    // 下面的 classify 函数对 `_` 容忍：含 `:` / `cid` 前缀的判定不受清洗影响；
    // 历史/手工目录可能出现「全 `_` 的 Base64 衍生 leaf」，会被归为 other。
    let keys = orch.persistence().workspace_mgr.list_workspace_keys();
    let total = keys.len();
    let strategy = orch.persistence().workspace_mgr.key_strategy();
    let strategy_s = match strategy {
        WorkspaceKeyStrategy::UserId => "user_id",
        WorkspaceKeyStrategy::Conversation => "conversation",
    };
    match strategy {
        WorkspaceKeyStrategy::UserId => serde_json::json!({
            "workspaces_total": total,
            "workspace_key_strategy": strategy_s,
            "buckets": {},
            "note": "UserId 策略：无法从目录名区分钉钉群/私；含历史遗留目录。"
        }),
        WorkspaceKeyStrategy::Conversation => {
            let mut counts: HashMap<&'static str, usize> = HashMap::new();
            for k in keys {
                let cat = classify_conversation_workspace_key(&k);
                *counts.entry(cat).or_default() += 1;
            }
            let buckets: serde_json::Map<String, serde_json::Value> = counts
                .into_iter()
                .map(|(name, count)| (name.to_string(), serde_json::json!(count)))
                .collect();
            serde_json::json!({
                "workspaces_total": total,
                "workspace_key_strategy": strategy_s,
                "buckets": buckets,
                "note": "按目录名启发式分类；历史/手工目录可能偏差；短 Base64 群 id 多在 other。"
            })
        }
    }
}

fn classify_conversation_workspace_key(key: &str) -> &'static str {
    // 注意：参数 `key` 实际是 **磁盘 leaf**——`/ \ : + =` 已被替换为 `_`。
    // 旧数据（迁移前）可能仍含原始 `:`；新数据只能凭 `cid` 前缀与下划线模式识别。
    if key.contains(':') {
        return "群聊"; // 迁移前的历史目录
    }
    if key.len() >= 3 && key[..3].eq_ignore_ascii_case("cid") {
        return "群聊";
    }
    if !key.is_empty() && key.chars().all(|c| c.is_ascii_digit()) {
        return "私聊";
    }
    if key == "cli_user" || key.starts_with("cli") {
        return "cli";
    }
    // 清洗后的群聊 chat_id（如 `_GmQ___021142012334576144`）通常以 `_` 起始且含
    // 多段下划线 + 末段数字，作为弱启发归到「群聊」。
    if key.starts_with('_') && key.matches('_').count() >= 2 {
        return "群聊";
    }
    "other"
}

fn build_html_page() -> String {
    r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>TyClaw 管理台</title>
<style>
:root{color-scheme:light;--bg:#f5f6f3;--surface:#fff;--ink:#20231f;--muted:#697069;--line:#dfe3dc;--green:#16784b;--coral:#c84f35;--gold:#9a6a00;--soft:#eef3ed}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font-family:Inter,"PingFang SC","Microsoft YaHei",system-ui,sans-serif;font-size:14px;letter-spacing:0}
header{height:64px;background:var(--surface);border-bottom:1px solid var(--line);display:flex;align-items:center;padding:0 28px;gap:16px;position:sticky;top:0;z-index:5}.brand{font-size:18px;font-weight:720}.status-dot{width:8px;height:8px;border-radius:50%;background:var(--green)}#instance{color:var(--muted);font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;margin-left:auto;max-width:55vw}
nav{background:var(--surface);border-bottom:1px solid var(--line);padding:0 28px;display:flex;gap:24px}.tab{border:0;border-bottom:2px solid transparent;background:transparent;padding:13px 2px 11px;color:var(--muted);font:inherit;font-weight:650;cursor:pointer}.tab[aria-selected="true"]{color:var(--ink);border-color:var(--green)}
main{max-width:1440px;margin:0 auto;padding:22px 28px 48px}.view[hidden]{display:none}.section{margin:0 0 24px}.section-head{display:flex;align-items:end;justify-content:space-between;gap:14px;margin-bottom:10px}h1,h2,h3{margin:0;font-weight:700}h2{font-size:16px}h3{font-size:13px;color:var(--muted)}.muted{color:var(--muted);font-size:12px}.grid{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:10px}.metric{background:var(--surface);border:1px solid var(--line);border-radius:6px;padding:14px;min-height:92px}.metric-label{color:var(--muted);font-size:12px}.metric-value{font-size:25px;line-height:1.2;font-weight:720;margin-top:10px;font-variant-numeric:tabular-nums}.metric-note{font-size:11px;color:var(--muted);margin-top:4px}
.panel{background:var(--surface);border:1px solid var(--line);border-radius:6px;padding:14px;min-width:0}.split{display:grid;grid-template-columns:minmax(0,1.6fr) minmax(320px,1fr);gap:10px}.stack{display:grid;gap:10px}.table-wrap{overflow:auto;max-width:100%}table{width:100%;border-collapse:collapse;font-size:12px}th,td{padding:9px 8px;border-bottom:1px solid #edf0eb;text-align:left;vertical-align:top;white-space:nowrap}th{color:var(--muted);font-weight:650;background:#fafbf9;position:sticky;top:0}td.wrap{white-space:normal;min-width:180px;line-height:1.5}.empty{color:var(--muted);padding:22px 8px;text-align:center}
.filters{display:grid;grid-template-columns:minmax(250px,1.5fr) repeat(5,minmax(120px,1fr)) auto;gap:8px;align-items:end;background:var(--surface);border:1px solid var(--line);border-radius:6px;padding:12px;margin-bottom:10px}label,.filter-field{display:grid;gap:5px;color:var(--muted);font-size:11px}.segmented{height:34px;display:grid;grid-template-columns:repeat(3,1fr);border:1px solid #cfd5cc;border-radius:4px;overflow:hidden}.range-option{border:0;border-right:1px solid #cfd5cc;background:#fff;color:var(--muted);font:inherit;font-weight:650;cursor:pointer}.range-option:last-child{border-right:0}.range-option[aria-pressed="true"]{background:var(--green);color:#fff}.range-option:disabled{opacity:.55;cursor:wait}select,input{height:34px;border:1px solid #cfd5cc;border-radius:4px;background:#fff;color:var(--ink);padding:0 9px;font:inherit;min-width:0}.action{height:34px;border:0;border-radius:4px;background:var(--green);color:white;padding:0 15px;font:inherit;font-weight:650;cursor:pointer}.action:disabled{opacity:.55;cursor:wait}
.alert{border:1px solid #dfb9af;background:#fff4f1;color:#86351f;border-radius:5px;padding:10px 12px;margin-bottom:10px}.alert.ok{border-color:#b9d8c8;background:#f0f8f3;color:#17623f}.alert[hidden]{display:none}.legend{display:flex;gap:16px;color:var(--muted);font-size:11px}.key:before{content:"";display:inline-block;width:10px;height:3px;margin-right:5px;vertical-align:middle;background:var(--green)}.key.users:before{background:var(--coral)}.key.tools:before{background:var(--gold)}.chart{position:relative;height:280px}.chart canvas{width:100%;height:100%;display:block}.badge{display:inline-block;border:1px solid var(--line);border-radius:4px;padding:2px 5px;background:var(--soft);font-size:10px;color:#465048}.error-text{color:var(--coral)}
@media(max-width:980px){.grid{grid-template-columns:repeat(2,minmax(0,1fr))}.split{grid-template-columns:1fr}.filters{grid-template-columns:repeat(3,minmax(0,1fr))}.quick-range{grid-column:span 2}.filters .action{grid-column:span 3}.chart{height:240px}}
@media(max-width:620px){header{height:auto;min-height:58px;padding:12px 16px;align-items:flex-start;flex-wrap:wrap}#instance{order:3;max-width:100%;width:100%;margin-left:0}nav{padding:0 16px;gap:20px}main{padding:16px}.grid{grid-template-columns:repeat(2,minmax(0,1fr));gap:8px}.metric{padding:12px;min-height:82px}.metric-value{font-size:21px}.filters{grid-template-columns:repeat(2,minmax(0,1fr))}.quick-range{grid-column:span 2}.filters .action{grid-column:span 2}.section-head{align-items:start;flex-direction:column}.chart{height:210px}}
</style>
</head>
<body>
<header><span class="status-dot"></span><div class="brand">TyClaw 管理台</div><div id="instance"></div></header>
<nav aria-label="管理视图"><button class="tab" data-view="overview" aria-selected="true">运行概览</button><button class="tab" data-view="analytics" aria-selected="false">使用分析</button></nav>
<main>
<section id="overview" class="view">
  <div class="section"><div class="section-head"><h2>运行状态</h2><span id="overview-updated" class="muted"></span></div><div id="overview-metrics" class="grid"></div></div>
  <div class="split section"><div class="panel"><div class="section-head"><h2>当前任务</h2></div><div id="tasks" class="table-wrap"></div></div><div class="panel"><div class="section-head"><h2>工作区</h2></div><div id="works"></div></div></div>
  <div class="split section"><div class="panel"><div class="section-head"><h2>最近审计</h2></div><div id="audit" class="table-wrap"></div></div><div class="panel"><div class="section-head"><h2>Skill</h2></div><div id="skills" class="table-wrap"></div></div></div>
</section>
<section id="analytics" class="view" hidden>
  <form id="filters" class="filters">
    <div class="filter-field quick-range"><span>快捷范围</span><div class="segmented" role="group" aria-label="快捷范围"><button class="range-option" type="button" data-range="today" aria-pressed="false">本日</button><button class="range-option" type="button" data-range="week" aria-pressed="false">本周</button><button class="range-option" type="button" data-range="month" aria-pressed="true">本月</button></div></div>
    <label>开始日期<input id="from" type="date"></label><label>结束日期<input id="to" type="date"></label>
    <label>工作区<select id="workspace"><option value="">全部</option></select></label><label>渠道<select id="channel"><option value="">全部</option></select></label>
    <label>来源<select id="source"><option value="">全部</option><option value="interactive">人工</option><option value="automated">自动任务</option></select></label>
    <button id="query" class="action" type="submit">查询</button>
  </form>
  <div id="analytics-alert" class="alert" hidden></div>
  <div id="analytics-metrics" class="grid section"></div>
  <div class="panel section"><div class="section-head"><div><h2>使用趋势</h2><div id="range" class="muted"></div></div><div class="legend"><span class="key">请求</span><span class="key users">用户</span><span class="key tools">工具</span></div></div><div class="chart"><canvas id="trend"></canvas></div></div>
  <div class="split section"><div class="panel"><div class="section-head"><h2>工具排行</h2></div><div id="tool-ranking" class="table-wrap"></div></div><div class="panel"><div class="section-head"><h2>用户排行</h2></div><div id="user-ranking" class="table-wrap"></div></div></div>
  <div class="panel section"><div class="section-head"><div><h2>最近问答</h2><div id="detail-range" class="muted"></div></div></div><div id="recent" class="table-wrap"></div></div>
</section>
</main>
<script>
'use strict';
const byId=id=>document.getElementById(id);
const clear=node=>{while(node.firstChild)node.removeChild(node.firstChild)};
const make=(tag,className,text)=>{const node=document.createElement(tag);if(className)node.className=className;if(text!==undefined)node.textContent=String(text);return node};
const fmt=value=>new Intl.NumberFormat('zh-CN').format(Number(value||0));
const pct=value=>Number(value||0).toFixed(1)+'%';
function metric(label,value,note){const box=make('div','metric');box.append(make('div','metric-label',label),make('div','metric-value',value));if(note)box.append(make('div','metric-note',note));return box}
function renderMetrics(target,items){const root=byId(target);clear(root);items.forEach(item=>root.append(metric(item[0],item[1],item[2])))}
function empty(target,text='暂无数据'){const root=byId(target);clear(root);root.append(make('div','empty',text))}
function table(target,headers,rows,wrapColumns=[]){const root=byId(target);clear(root);if(!rows.length){root.append(make('div','empty','暂无数据'));return}const t=make('table');const head=make('thead');const hr=make('tr');headers.forEach(h=>hr.append(make('th','',h)));head.append(hr);const body=make('tbody');rows.forEach(row=>{const tr=make('tr');row.forEach((value,index)=>tr.append(make('td',wrapColumns.includes(index)?'wrap':'',value)));body.append(tr)});t.append(head,body);root.append(t)}
async function getJson(url){const response=await fetch(url,{headers:{Accept:'application/json'},cache:'no-store'});let body={};try{body=await response.json()}catch(_){}if(!response.ok)throw new Error(body.error||('HTTP '+response.status));return body}
function renderAudit(data){const rows=data.audit_recent||[];if(data.privacy_mode){table('audit',['时间','渠道','工具','耗时'],rows.map(v=>[v.time,v.channel,fmt(v.tools),v.duration||'']))}else{table('audit',['时间','渠道','请求','工具','耗时'],rows.map(v=>[v.time,v.channel,v.request,fmt(v.tools),v.duration||'']),[2])}}
function renderRecent(data){const rows=data.recent||[];if(data.privacy_mode){table('recent',['时间','用户','渠道','来源','状态','耗时','工具'],rows.map(v=>[v.started_at,v.user_name||v.masked_user_id,v.channel,v.source,v.status,fmt(v.duration_ms)+' ms',(v.tools||[]).map(t=>t.name).join(', ')]),[6])}else{table('recent',['时间','用户','渠道','来源','状态','问题摘要','回答摘要','耗时','工具'],rows.map(v=>[v.started_at,v.user_name||v.masked_user_id,v.channel,v.source,v.status,v.request_preview,v.response_preview,fmt(v.duration_ms)+' ms',(v.tools||[]).map(t=>t.name).join(', ')]),[5,6,8])}}
async function loadOverview(){try{const data=await getJson('/api/stats');byId('instance').textContent=data.model+' | '+data.workspace+' | ctx '+data.context_window;byId('overview-updated').textContent=new Date().toLocaleTimeString('zh-CN');renderMetrics('overview-metrics',[['活跃任务',fmt(data.active_task_count)],['Skill',fmt(data.skill_count)],['工作区',fmt(data.works_stats?.workspaces_total)],['上下文窗口',fmt(data.context_window)]]);table('tasks',['工作区','用户','任务','运行秒数'],(data.active_tasks||[]).map(v=>[v.workspace,v.user_id,v.summary,fmt(v.elapsed_secs)]),[2]);const ws=data.works_stats||{};const workRows=Object.entries(ws.buckets||{});table('works',['类别','数量'],workRows);if(ws.note){byId('works').append(make('div','muted',ws.note))}table('skills',['名称','分类','状态'],(data.skills||[]).map(v=>[v.name,v.category,v.status]),[0]);renderAudit(data)}catch(error){byId('instance').textContent='运行状态不可用';empty('tasks',error.message)}}
document.querySelectorAll('.tab').forEach(button=>button.addEventListener('click',()=>{document.querySelectorAll('.tab').forEach(tab=>tab.setAttribute('aria-selected',String(tab===button)));document.querySelectorAll('.view').forEach(view=>view.hidden=view.id!==button.dataset.view);if(button.dataset.view==='analytics')loadAnalytics()}));
function syncOptions(id,values){const select=byId(id);const current=select.value;while(select.options.length>1)select.remove(1);values.forEach(value=>{const option=make('option','',value);option.value=value;select.append(option)});if(values.includes(current))select.value=current}
let activeRange='month';
const rangeButtons=()=>Array.from(document.querySelectorAll('.range-option'));
function setActiveRange(value){activeRange=value;rangeButtons().forEach(button=>button.setAttribute('aria-pressed',String(button.dataset.range===value)))}
function analyticsUrl(){const params=new URLSearchParams();if(activeRange){params.set('range',activeRange)}else{['from','to'].forEach(id=>{const value=byId(id).value;if(value)params.set(id,value)})}['workspace','channel','source'].forEach(id=>{const value=byId(id).value;if(value)params.set(id,value)});return '/api/analytics?'+params.toString()}
let analyticsLoading=false;
async function loadAnalytics(){if(analyticsLoading)return;analyticsLoading=true;byId('query').disabled=true;rangeButtons().forEach(button=>button.disabled=true);try{const data=await getJson(analyticsUrl());byId('from').value=data.from;byId('to').value=data.to;byId('range').textContent=data.from+' 至 '+data.to+' | 日统计 | '+data.timezone;byId('detail-range').textContent=data.health.earliest_detail_date?'可查询明细始于 '+data.health.earliest_detail_date:'当前范围无明细';syncOptions('workspace',data.filters.workspaces||[]);syncOptions('channel',data.filters.channels||[]);const s=data.summary;renderMetrics('analytics-metrics',[['活跃用户',fmt(s.active_users),'会话 '+fmt(s.sessions)],['请求',fmt(s.requests),'人工 '+fmt(s.interactive_requests)+' | 自动 '+fmt(s.automated_requests)],['问题',fmt(s.questions),'回答率 '+pct(s.answer_rate)],['错误率',pct(s.error_rate),fmt(s.errors)+' 次错误'],['平均耗时',fmt(s.average_duration_ms)+' ms'],['Prompt Token',fmt(s.prompt_tokens)],['Completion Token',fmt(s.completion_tokens)],['工具调用',fmt(s.tool_calls),'成功率 '+pct(s.tool_success_rate)]]);renderHealth(data.health);drawTrend(data.series||[]);table('tool-ranking',['范围','工具','调用','成功','失败','拒绝','平均耗时'],(data.tools||[]).map(v=>[v.scope,v.name,fmt(v.calls),fmt(v.successes),fmt(v.failures),fmt(v.denied),fmt(v.average_duration_ms)+' ms']));table('user-ranking',['用户','标识','请求','会话','问题','回答','错误','工具'],(data.users||[]).map(v=>[v.user_name||'未命名',v.masked_user_id,fmt(v.requests),fmt(v.sessions),fmt(v.questions),fmt(v.answers),fmt(v.errors),fmt(v.tool_calls)]));renderRecent(data)}catch(error){const alert=byId('analytics-alert');alert.hidden=false;alert.className='alert';alert.textContent='使用统计不可用：'+error.message;clear(byId('analytics-metrics'));drawTrend([])}finally{analyticsLoading=false;byId('query').disabled=false;rangeButtons().forEach(button=>button.disabled=false)}}
function renderHealth(health){const alert=byId('analytics-alert');const issues=[];if(!health.available)issues.push('数据库不可用');if(health.dropped_events)issues.push('队列丢弃 '+fmt(health.dropped_events)+' 条事件');if(health.storage_warning)issues.push('数据库已达到容量告警线');if(health.detail_evictions)issues.push('已提前淘汰 '+fmt(health.detail_evictions)+' 条明细');if(health.last_error)issues.push('最近错误 '+health.last_error);alert.hidden=false;alert.className=issues.length?'alert':'alert ok';alert.textContent=issues.length?issues.join('；'):'统计服务正常 | 数据库 '+fmt(health.database_bytes)+' bytes'}
function drawTrend(series){const canvas=byId('trend');const rect=canvas.getBoundingClientRect();const ratio=window.devicePixelRatio||1;canvas.width=Math.max(1,Math.floor(rect.width*ratio));canvas.height=Math.max(1,Math.floor(rect.height*ratio));const ctx=canvas.getContext('2d');ctx.scale(ratio,ratio);const width=rect.width,height=rect.height;ctx.clearRect(0,0,width,height);const pad={l:44,r:16,t:18,b:35};const chartW=width-pad.l-pad.r,chartH=height-pad.t-pad.b;if(!series.length){ctx.fillStyle='#697069';ctx.font='12px system-ui';ctx.textAlign='center';ctx.fillText('暂无趋势数据',width/2,height/2);return}const keys=[['requests','#16784b'],['active_users','#c84f35'],['tool_calls','#9a6a00']];const max=Math.max(1,...series.flatMap(point=>keys.map(([key])=>Number(point[key]||0))));ctx.strokeStyle='#e4e7e1';ctx.fillStyle='#697069';ctx.font='10px system-ui';ctx.textAlign='right';for(let i=0;i<=4;i++){const y=pad.t+chartH*i/4;ctx.beginPath();ctx.moveTo(pad.l,y);ctx.lineTo(width-pad.r,y);ctx.stroke();const label=max<4?(max*(4-i)/4).toFixed(1):String(Math.round(max*(4-i)/4));ctx.fillText(label,pad.l-7,y+3)}keys.forEach(([key,color])=>{const points=series.map((point,index)=>({x:pad.l+(series.length===1?chartW/2:chartW*index/(series.length-1)),y:pad.t+chartH*(1-Number(point[key]||0)/max)}));ctx.strokeStyle=color;ctx.fillStyle=color;ctx.lineWidth=2;ctx.beginPath();points.forEach((point,index)=>index?ctx.lineTo(point.x,point.y):ctx.moveTo(point.x,point.y));ctx.stroke();points.forEach(point=>{ctx.beginPath();ctx.arc(point.x,point.y,3,0,Math.PI*2);ctx.fill()})});ctx.fillStyle='#697069';ctx.textAlign='center';const step=Math.max(1,Math.ceil(series.length/6));series.forEach((point,index)=>{if(index%step===0||index===series.length-1){const x=pad.l+(series.length===1?chartW/2:chartW*index/(series.length-1));ctx.fillText(point.period_start.slice(5),x,height-10)}})}
rangeButtons().forEach(button=>button.addEventListener('click',()=>{setActiveRange(button.dataset.range);byId('from').value='';byId('to').value='';loadAnalytics()}));['from','to'].forEach(id=>byId(id).addEventListener('change',()=>setActiveRange(null)));byId('filters').addEventListener('submit',event=>{event.preventDefault();loadAnalytics()});window.addEventListener('resize',()=>{if(!byId('analytics').hidden)loadAnalytics()});loadOverview();setInterval(loadOverview,10000);
</script>
</body>
</html>"##.to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let boundary = s.floor_char_boundary(max);
        format!("{}...", &s[..boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_http_method_target_and_headers() {
        let parsed = parse_http_request_headers(
            "GET /api/analytics?range=week HTTP/1.1\r\nAuthorization: Basic abc\r\n\r\n",
        )
        .unwrap();
        assert_eq!(parsed.0, "GET");
        assert_eq!(parsed.1, "/api/analytics?range=week");
        assert_eq!(parsed.2.get("authorization").unwrap(), "Basic abc");
    }

    #[test]
    fn basic_auth_handles_password_colons_and_rejects_wrong_values() {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode("admin:p:a:ss");
        assert!(check_basic_auth(
            &format!("Basic {encoded}"),
            "admin",
            "p:a:ss"
        ));
        assert!(!check_basic_auth(
            &format!("Basic {encoded}"),
            "admin",
            "wrong"
        ));
    }

    fn audit_fixture() -> AuditEntry {
        AuditEntry {
            timestamp: chrono::Utc::now(),
            workspace_key: "workspace".into(),
            session_id: "session".into(),
            user_id: "user".into(),
            user_name: "name".into(),
            channel: "cli".into(),
            request: "historical-request-secret".into(),
            tool_calls: vec![serde_json::json!({ "name": "read_file" })],
            skills_used: Vec::new(),
            final_response: Some("historical-response-secret".into()),
            total_duration: Some(1.25),
            token_usage: None,
        }
    }

    #[test]
    fn audit_api_removes_historical_content_in_privacy_mode() {
        let hidden = audit_entry_json(&audit_fixture(), true);
        assert!(hidden.get("request").is_none());
        assert!(hidden.get("response").is_none());
        assert_eq!(
            hidden.get("tools").and_then(|value| value.as_u64()),
            Some(1)
        );

        let visible = audit_entry_json(&audit_fixture(), false);
        assert_eq!(
            visible.get("request").and_then(|value| value.as_str()),
            Some("historical-request-secret")
        );
        assert_eq!(
            visible.get("response").and_then(|value| value.as_str()),
            Some("historical-response-secret")
        );
    }

    #[test]
    fn analytics_api_removes_historical_previews_in_privacy_mode() {
        let fixture = serde_json::json!({
            "summary": { "requests": 1 },
            "recent": [{
                "status": "success",
                "request_preview": "historical-question-secret",
                "response_preview": "historical-answer-secret"
            }]
        });
        let hidden: serde_json::Value =
            serde_json::from_str(&serialize_analytics_report(&fixture, true).unwrap()).unwrap();
        assert_eq!(
            hidden.get("privacy_mode").and_then(|value| value.as_bool()),
            Some(true)
        );
        let hidden_row = &hidden["recent"][0];
        assert!(hidden_row.get("request_preview").is_none());
        assert!(hidden_row.get("response_preview").is_none());
        assert_eq!(
            hidden_row.get("status").and_then(|value| value.as_str()),
            Some("success")
        );

        let visible: serde_json::Value =
            serde_json::from_str(&serialize_analytics_report(&fixture, false).unwrap()).unwrap();
        assert_eq!(
            visible["recent"][0]
                .get("request_preview")
                .and_then(|value| value.as_str()),
            Some("historical-question-secret")
        );
        assert_eq!(
            visible
                .get("privacy_mode")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn analytics_query_defaults_and_decodes_filters() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let query = parse_analytics_query(
            "workspace=team%2Ffinance&channel=dingtalk+group&source=interactive",
            today,
            400,
        )
        .unwrap();
        assert_eq!(query.from, NaiveDate::from_ymd_opt(2026, 8, 1).unwrap());
        assert_eq!(query.to, today);
        assert_eq!(query.workspace.as_deref(), Some("team/finance"));
        assert_eq!(query.channel.as_deref(), Some("dingtalk group"));
        assert_eq!(query.source, Some(UsageSource::Interactive));
    }

    #[test]
    fn analytics_query_supports_current_day_week_and_month() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 9).unwrap();
        let day = parse_analytics_query("range=today", today, 400).unwrap();
        assert_eq!(day.from, today);
        assert_eq!(day.to, today);

        let week = parse_analytics_query("range=week", today, 400).unwrap();
        assert_eq!(week.from, NaiveDate::from_ymd_opt(2026, 8, 3).unwrap());
        assert_eq!(week.to, today);

        let month = parse_analytics_query("range=month", today, 400).unwrap();
        assert_eq!(month.from, NaiveDate::from_ymd_opt(2026, 8, 1).unwrap());
        assert_eq!(month.to, today);
    }

    #[test]
    fn analytics_query_rejects_invalid_ambiguous_and_oversized_ranges() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        assert_eq!(
            parse_analytics_query("range=quarter", today, 400).unwrap_err(),
            "analytics_invalid_range"
        );
        assert_eq!(
            parse_analytics_query("range=week&from=2026-08-01", today, 400).unwrap_err(),
            "analytics_ambiguous_date_range"
        );
        assert_eq!(
            parse_analytics_query("unknown=value", today, 400).unwrap_err(),
            "analytics_unknown_query_parameter"
        );
        assert_eq!(
            parse_analytics_query("from=2025-07-06&to=2026-08-10", today, 400).unwrap_err(),
            "analytics_invalid_date_range"
        );
        assert!(parse_analytics_query("channel=%ZZ", today, 400).is_err());
    }

    #[test]
    fn management_page_uses_text_nodes_for_dynamic_content() {
        let page = build_html_page();
        assert!(page.contains("运行概览"));
        assert!(page.contains("使用分析"));
        assert!(page.contains("本日"));
        assert!(page.contains("本周"));
        assert!(page.contains("本月"));
        assert!(page.contains("if(data.privacy_mode){table('audit',['时间','渠道','工具','耗时']"));
        assert!(page.contains("if(data.privacy_mode){table('recent',['时间','用户','渠道','来源','状态','耗时','工具']"));
        assert!(!page.contains("时间粒度"));
        assert!(page.contains("textContent"));
        assert!(!page.contains("innerHTML"));
        assert!(!page.contains("Access-Control-Allow-Origin"));
    }
}
