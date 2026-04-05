use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use tyclaw_orchestration::{Orchestrator, OrchestratorFeatures, RequestContext};
use tyclaw_provider::{LLMProvider, OpenAICompatProvider};
use tyclaw_tools::{ListDirTool, ReadFileTool, ToolRegistry};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = env::var("TYCLAW_WORKSPACE").unwrap_or_else(|_| ".".into());
    let api_key = env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY is required for sdk_demo");
    let api_base =
        env::var("OPENAI_API_BASE").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = env::var("TYCLAW_MODEL").unwrap_or_else(|_| "gpt-4o".into());

    let ws_path = {
        let path = PathBuf::from(&workspace);
        std::fs::create_dir_all(&path).ok();
        std::fs::canonicalize(&path).unwrap_or(path)
    };
    let mut provider_impl = OpenAICompatProvider::new(&api_key, &api_base, &model, None);
    provider_impl.set_snapshot_dir(ws_path.join("logs").join("snap").join("llm_requests"));
    let provider: Arc<dyn LLMProvider> = Arc::new(provider_impl);

    // 演示外部工具注入：这里只开放只读工具，便于嵌入其他系统。
    let mut runtime_tools = ToolRegistry::new();
    runtime_tools.register(Box::new(ReadFileTool::new(Some(ws_path.clone()))));
    runtime_tools.register(Box::new(ListDirTool::new(Some(ws_path.clone()))));

    let features = OrchestratorFeatures {
        enable_audit: false,
        enable_memory: false,
        enable_rbac: true,
        enable_rate_limit: false,
    };

    let orchestrator = Orchestrator::builder(provider, &ws_path)
        .with_model(model)
        .with_max_iterations(12)
        .with_tools(runtime_tools)
        .with_features(features)
        .build();

    let req = RequestContext::new("sdk_user", "default", "sdk", "demo-chat");
    let user_message = env::args()
        .nth(1)
        .unwrap_or_else(|| "请列出当前目录下的 Rust 文件".into());

    let response = orchestrator
        .handle_with_context(&user_message, &req, None)
        .await?;

    println!("Assistant: {}", response.text);
    if !response.tools_used.is_empty() {
        println!("Tools used: {}", response.tools_used.join(", "));
    }
    println!("Duration: {:.2}s", response.duration_seconds);

    Ok(())
}
