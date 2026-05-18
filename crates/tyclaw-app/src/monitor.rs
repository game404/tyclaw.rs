//! 轻量监控 HTTP 服务 —— 可配置 bind/port，可选 Basic 认证。
//!
//! 端点：GET / HTML；GET /api/stats JSON

use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tyclaw_control::WorkspaceKeyStrategy;
use tyclaw_orchestration::Orchestrator;

#[derive(Clone)]
pub struct MonitorOptions {
    pub bind: String,
    pub port: u16,
    pub basic_auth: Option<(String, String)>,
}

pub fn spawn_monitor(orchestrator: Arc<Orchestrator>, options: Option<MonitorOptions>) {
    let Some(opts) = options else { return };
    let addr = format!("{}:{}", opts.bind.trim(), opts.port);
    let basic = opts.basic_auth.clone();
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
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                let n = match stream.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let Some((path, headers)) = parse_http_request_headers(&request) else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = stream.shutdown().await;
                    return;
                };
                let path = path.split('?').next().unwrap_or(&path).to_string();
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
                let response = match path.as_str() {
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

fn parse_http_request_headers(raw: &str) -> Option<(String, HashMap<String, String>)> {
    let head_end = raw.find("\r\n\r\n").or_else(|| raw.find("\n\n"))?;
    let head = &raw[..head_end];
    let mut lines = head.lines();
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let _method = parts.next()?;
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Some((path, headers))
}

fn check_basic_auth(header_value: &str, expect_user: &str, expect_password: &str) -> bool {
    check_basic_auth_inner(header_value, expect_user, expect_password).unwrap_or(false)
}

fn check_basic_auth_inner(header_value: &str, expect_user: &str, expect_password: &str) -> Option<bool> {
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
    base64::engine::general_purpose::STANDARD.decode(s.trim()).ok()
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
    let works_stats = build_works_stats(orch);
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
        "works_stats": works_stats,
    })
    .to_string()
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
                .map(|(name, count)| {
                    (name.to_string(), serde_json::json!(count))
                })
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

fn build_html_page(orch: &Orchestrator) -> String {
    let stats = build_stats_json(orch);
    format!(
        r##"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8"><title>TyClaw Monitor</title>
<style>body{{font-family:system-ui;background:#0d1117;color:#c9d1d9;padding:16px;}}h1{{color:#58a6ff;}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:10px;margin:12px 0;}}.card{{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:12px;}}.label{{color:#8b949e;font-size:0.75em;}}.value{{font-size:1.5em;font-weight:bold;color:#f0f6fc;}}.green{{color:#3fb950;}}.blue{{color:#58a6ff;}}.orange{{color:#d29922;}}h2{{color:#58a6ff;font-size:1em;margin-top:16px;}}table{{width:100%;font-size:0.85em;border-collapse:collapse;}}th,td{{padding:6px;border-bottom:1px solid #21262d;text-align:left;}}.small{{color:#8b949e;font-size:0.8em;}}</style></head><body>
<h1>TyClaw.rs Monitor</h1><div id="sub" class="small"></div>
<div class="grid" id="cards"></div>
<h2>Works 目录（启发式）</h2><div id="works"></div>
<h2>Active Tasks</h2><div id="tasks"></div>
<h2>Skills</h2><div id="skills"></div>
<h2>Recent Audit</h2><div id="audit"></div>
<script>
const data = {stats};
document.getElementById('sub').textContent = data.model+' · '+data.workspace+' · ctx='+data.context_window;
let c = '<div class="card"><span class="label">Active</span><div class="value green">'+data.active_task_count+'</div></div>';
c += '<div class="card"><span class="label">Skills</span><div class="value blue">'+data.skill_count+'</div></div>';
if(data.works_stats&&data.works_stats.workspaces_total!==undefined)
  c += '<div class="card"><span class="label">Works 目录</span><div class="value orange">'+data.works_stats.workspaces_total+'</div><div class="small">'+(data.works_stats.workspace_key_strategy||'')+'</div></div>';
document.getElementById('cards').innerHTML=c;
const ws=data.works_stats;
let wh=(ws&&ws.note)?'<p class="small">'+ws.note+'</p>':'';
if(ws&&ws.buckets&&Object.keys(ws.buckets).length){{
  wh+='<table><tr><th>类别</th><th>数量</th></tr>';
  for(const[n,v] of Object.entries(ws.buckets)) wh+='<tr><td>'+n+'</td><td>'+v+'</td></tr>';
  wh+='</table>';
}}else wh+='<p class="small">无分桶</p>';
document.getElementById('works').innerHTML=wh;
if(!data.active_tasks.length)document.getElementById('tasks').innerHTML='<p class="small">无</p>';
else{{let h='<table><tr><th>WS</th><th>User</th><th>Summary</th></tr>';data.active_tasks.forEach(t=>h+='<tr><td>'+t.workspace+'</td><td>'+t.user_id+'</td><td>'+t.summary+'</td></tr>');document.getElementById('tasks').innerHTML=h+'</table>';}}
if(!data.skills.length)document.getElementById('skills').innerHTML='<p class="small">无</p>';
else{{let h='<table><tr><th>Name</th><th>Cat</th></tr>';data.skills.forEach(s=>h+='<tr><td>'+s.name+'</td><td>'+s.category+'</td></tr>');document.getElementById('skills').innerHTML=h+'</table>';}}
if(!data.audit_recent.length)document.getElementById('audit').innerHTML='<p class="small">无</p>';
else{{let h='<table><tr><th>Time</th><th>Ch</th><th>Req</th></tr>';data.audit_recent.forEach(e=>h+='<tr><td>'+e.time+'</td><td>'+e.channel+'</td><td>'+e.request+'</td></tr>');document.getElementById('audit').innerHTML=h+'</table>';}}
setTimeout(()=>location.reload(),5000);
</script></body></html>"##,
        stats = stats
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
