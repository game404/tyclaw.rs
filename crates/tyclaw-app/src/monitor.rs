//! 轻量监控 HTTP 服务 —— 监听 127.0.0.1，提供运行状态页面。
//!
//! 端点：
//! - GET /          HTML 监控页面（自动刷新）
//! - GET /api/stats  JSON 格式状态数据

use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tyclaw_orchestration::Orchestrator;

/// 启动监控 HTTP 服务（后台 task）。
pub fn spawn_monitor(orchestrator: Arc<Orchestrator>, port: u16) {
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{port}");
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
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = match tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");

                let response = match path {
                    "/api/stats" => {
                        let json = build_stats_json(&orch);
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{json}"
                        )
                    }
                    _ => {
                        let html = build_html_page(&orch);
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{html}"
                        )
                    }
                };
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
}

fn build_stats_json(orch: &Orchestrator) -> String {
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
        .map(|e| {
            serde_json::json!({
                "time": e.timestamp.format("%H:%M:%S").to_string(),
                "user": e.user_name,
                "channel": e.channel,
                "request": truncate(&e.request, 80),
                "tools": e.tool_calls.len(),
                "duration": e.total_duration.map(|d| format!("{d:.1}s")),
                "response": e.final_response.as_deref().map(|r| truncate(r, 100)),
            })
        })
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

    let app = orch.app();
    serde_json::json!({
        "model": app.model,
        "workspace": app.workspace.display().to_string(),
        "context_window": app.context_window_tokens,
        "active_tasks": active_tasks,
        "active_task_count": active_tasks.len(),
        "audit_recent": audit_entries,
        "skills": skills,
        "skill_count": skills.len(),
    })
    .to_string()
}

fn build_html_page(orch: &Orchestrator) -> String {
    let stats = build_stats_json(orch);
    format!(
        r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>TyClaw.rs Monitor</title>
<style>
  * {{ margin:0; padding:0; box-sizing:border-box; }}
  body {{ font-family: -apple-system, 'Segoe UI', sans-serif; background:#0d1117; color:#c9d1d9; padding:20px; }}
  h1 {{ color:#58a6ff; margin-bottom:8px; font-size:1.6em; }}
  .subtitle {{ color:#8b949e; margin-bottom:20px; font-size:0.9em; }}
  .grid {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(200px,1fr)); gap:12px; margin-bottom:24px; }}
  .card {{ background:#161b22; border:1px solid #30363d; border-radius:8px; padding:16px; }}
  .card .label {{ color:#8b949e; font-size:0.8em; text-transform:uppercase; }}
  .card .value {{ color:#f0f6fc; font-size:1.8em; font-weight:bold; margin-top:4px; }}
  .card .value.green {{ color:#3fb950; }}
  .card .value.blue {{ color:#58a6ff; }}
  .card .value.orange {{ color:#d29922; }}
  h2 {{ color:#58a6ff; font-size:1.1em; margin:20px 0 10px; border-bottom:1px solid #21262d; padding-bottom:6px; }}
  table {{ width:100%; border-collapse:collapse; font-size:0.85em; }}
  th {{ text-align:left; color:#8b949e; padding:6px 10px; border-bottom:1px solid #30363d; font-weight:normal; text-transform:uppercase; font-size:0.75em; }}
  td {{ padding:6px 10px; border-bottom:1px solid #21262d; }}
  tr:hover td {{ background:#161b22; }}
  .tag {{ display:inline-block; background:#1f6feb22; color:#58a6ff; padding:2px 8px; border-radius:4px; font-size:0.8em; margin:1px; }}
  .tag.active {{ background:#23883622; color:#3fb950; }}
  .tag.builtin {{ background:#d2992222; color:#d29922; }}
  .truncate {{ max-width:300px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }}
  .refresh {{ color:#484f58; font-size:0.75em; float:right; }}
  .empty {{ color:#484f58; font-style:italic; padding:12px; }}
</style>
</head>
<body>
<h1>🦀 TyClaw.rs Monitor</h1>
<div class="subtitle" id="subtitle">loading...</div>

<div class="grid" id="cards"></div>

<h2>Active Tasks</h2>
<div id="active-tasks"></div>

<h2>Skills</h2>
<div id="skills"></div>

<h2>Recent Audit (today)</h2>
<div id="audit"></div>

<script>
const data = {stats};

document.getElementById('subtitle').textContent =
  data.model + ' · ' + data.workspace + ' · ctx=' + data.context_window;

document.getElementById('cards').innerHTML = `
  <div class="card"><div class="label">Active Tasks</div><div class="value green">${{data.active_task_count}}</div></div>
  <div class="card"><div class="label">Skills</div><div class="value blue">${{data.skill_count}}</div></div>
  <div class="card"><div class="label">Model</div><div class="value" style="font-size:1em">${{data.model}}</div></div>
  <div class="card"><div class="label">Context Window</div><div class="value orange">${{(data.context_window/1000)}}K</div></div>
`;

// Active tasks
if (data.active_tasks.length === 0) {{
  document.getElementById('active-tasks').innerHTML = '<div class="empty">No active tasks</div>';
}} else {{
  let html = '<table><tr><th>Workspace</th><th>User</th><th>Summary</th><th>Elapsed</th></tr>';
  data.active_tasks.forEach(t => {{
    html += `<tr><td>${{t.workspace}}</td><td>${{t.user_id}}</td><td class="truncate">${{t.summary}}</td><td>${{t.elapsed_secs}}s</td></tr>`;
  }});
  html += '</table>';
  document.getElementById('active-tasks').innerHTML = html;
}}

// Skills
if (data.skills.length === 0) {{
  document.getElementById('skills').innerHTML = '<div class="empty">No skills loaded</div>';
}} else {{
  let html = '<table><tr><th>Name</th><th>Category</th><th>Description</th><th>Status</th></tr>';
  data.skills.forEach(s => {{
    const cls = s.status === 'builtin' ? 'builtin' : 'active';
    html += `<tr><td>${{s.name}}</td><td><span class="tag">${{s.category}}</span></td><td class="truncate">${{s.description}}</td><td><span class="tag ${{cls}}">${{s.status}}</span></td></tr>`;
  }});
  html += '</table>';
  document.getElementById('skills').innerHTML = html;
}}

// Audit
if (data.audit_recent.length === 0) {{
  document.getElementById('audit').innerHTML = '<div class="empty">No audit entries today</div>';
}} else {{
  let html = '<table><tr><th>Time</th><th>User</th><th>Channel</th><th>Request</th><th>Tools</th><th>Duration</th></tr>';
  data.audit_recent.forEach(e => {{
    html += `<tr><td>${{e.time}}</td><td>${{e.user}}</td><td>${{e.channel}}</td><td class="truncate">${{e.request}}</td><td>${{e.tools}}</td><td>${{e.duration || '-'}}</td></tr>`;
  }});
  html += '</table>';
  document.getElementById('audit').innerHTML = html;
}}

// Auto-refresh every 5s
setTimeout(() => location.reload(), 5000);
</script>
</body>
</html>"##
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let boundary = s.floor_char_boundary(max);
        format!("{}...", &s[..boundary])
    }
}
