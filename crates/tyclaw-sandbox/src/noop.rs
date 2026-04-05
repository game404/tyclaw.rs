//! Noop 沙箱 —— 无隔离，直接在 host 执行。
//!
//! 用于调试模式或 Docker 不可用时的 fallback。
//! 行为与现有 ExecTool/ReadFileTool 完全一致。

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;
use tyclaw_types::TyclawError;

use crate::types::*;

/// Noop 沙箱：直接在 host 上执行，无隔离。
pub struct NoopSandbox {
    workspace: PathBuf,
    id: String,
}

#[async_trait]
impl Sandbox for NoopSandbox {
    async fn exec(&self, cmd: &str, timeout: Duration) -> Result<SandboxExecResult, TyclawError> {
        let result = tokio::time::timeout(
            timeout,
            Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(&self.workspace)
                .output(),
        )
        .await;

        match result {
            Err(_) => Ok(SandboxExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: -1,
                timed_out: true,
            }),
            Ok(Err(e)) => Err(TyclawError::Tool {
                tool: "sandbox_exec".into(),
                message: format!("Failed to execute: {e}"),
            }),
            Ok(Ok(output)) => Ok(SandboxExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                exit_code: output.status.code().unwrap_or(-1),
                timed_out: false,
            }),
        }
    }

    async fn stat(&self, path: &str) -> Result<SandboxFileStat, TyclawError> {
        let full = self.workspace.join(path);
        match tokio::fs::metadata(&full).await {
            Ok(meta) => Ok(SandboxFileStat {
                exists: true,
                is_file: meta.is_file(),
                is_dir: meta.is_dir(),
                size: Some(meta.len()),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SandboxFileStat {
                exists: false,
                is_file: false,
                is_dir: false,
                size: None,
            }),
            Err(e) => Err(TyclawError::Tool {
                tool: "sandbox_stat".into(),
                message: format!("Stat failed: {e}"),
            }),
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, TyclawError> {
        let full = self.workspace.join(path);
        tokio::fs::read(&full).await.map_err(|e| TyclawError::Tool {
            tool: "sandbox_read".into(),
            message: format!("Read failed: {e}"),
        })
    }

    async fn write_file(&self, path: &str, content: &[u8]) -> Result<(), TyclawError> {
        let full = self.workspace.join(path);
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(&full, content)
            .await
            .map_err(|e| TyclawError::Tool {
                tool: "sandbox_write".into(),
                message: format!("Write failed: {e}"),
            })
    }

    async fn create_dir(&self, path: &str) -> Result<(), TyclawError> {
        let full = self.workspace.join(path);
        tokio::fs::create_dir_all(&full)
            .await
            .map_err(|e| TyclawError::Tool {
                tool: "sandbox_mkdir".into(),
                message: format!("Create dir failed: {e}"),
            })
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<SandboxDirEntry>, TyclawError> {
        let full = self.workspace.join(path);
        let mut entries = Vec::new();
        let mut rd = tokio::fs::read_dir(&full)
            .await
            .map_err(|e| TyclawError::Tool {
                tool: "sandbox_list_dir".into(),
                message: format!("List dir failed: {e}"),
            })?;
        while let Some(entry) = rd.next_entry().await.map_err(|e| TyclawError::Tool {
            tool: "sandbox_list_dir".into(),
            message: format!("{e}"),
        })? {
            entries.push(SandboxDirEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                is_dir: entry.path().is_dir(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn walk_dir(
        &self,
        path: &str,
        max_depth: usize,
    ) -> Result<Vec<SandboxWalkEntry>, TyclawError> {
        let base = self.workspace.join(path);
        let entries = tokio::task::spawn_blocking(move || {
            fn walk(
                dir: &std::path::Path,
                base: &std::path::Path,
                depth: usize,
                max_depth: usize,
                items: &mut Vec<SandboxWalkEntry>,
            ) {
                if depth > max_depth {
                    return;
                }

                let mut entries: Vec<_> = match std::fs::read_dir(dir) {
                    Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
                    Err(_) => return,
                };
                entries.sort_by_key(|entry| entry.file_name());

                for entry in entries {
                    let path = entry.path();
                    let rel = path.strip_prefix(base).unwrap_or(&path);
                    let is_dir = path.is_dir();
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    items.push(SandboxWalkEntry {
                        path: rel_str,
                        is_dir,
                        depth,
                    });
                    if is_dir {
                        walk(&path, base, depth + 1, max_depth, items);
                    }
                }
            }

            let mut items = Vec::new();
            walk(&base, &base, 1, max_depth, &mut items);
            items
        })
        .await
        .map_err(|e| TyclawError::Tool {
            tool: "sandbox_walk_dir".into(),
            message: format!("Walk dir failed: {e}"),
        })?;
        Ok(entries)
    }

    async fn grep_search(
        &self,
        request: SandboxGrepRequest,
    ) -> Result<SandboxGrepResponse, TyclawError> {
        let mut cmd = Command::new("rg");
        cmd.current_dir(&self.workspace);
        cmd.args(["--no-heading", "--line-number", "--color", "never"]);

        match request.output_mode.as_str() {
            "files_only" => {
                cmd.arg("-l");
            }
            "count" => {
                cmd.arg("-c");
            }
            _ => {}
        }

        if request.case_insensitive {
            cmd.arg("-i");
        }
        if let Some(c) = request.context_lines {
            if c > 0 && request.output_mode == "content" {
                cmd.args(["-C", &c.to_string()]);
            }
        }
        if let Some(ref t) = request.file_type {
            cmd.args(["--type", t]);
        }
        if let Some(ref inc) = request.include {
            cmd.args(["--glob", inc]);
        }
        cmd.args(["--max-count", &request.max_results.to_string()]);
        cmd.arg("--").arg(&request.pattern).arg(&request.path);

        let output = cmd.output().await.map_err(|e| TyclawError::Tool {
            tool: "sandbox_grep_search".into(),
            message: format!("rg failed: {e}"),
        })?;

        Ok(SandboxGrepResponse {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }

    async fn glob_search(
        &self,
        pattern: &str,
        path: &str,
    ) -> Result<Vec<SandboxGlobEntry>, TyclawError> {
        let output = Command::new("bash")
            .args([
                "-O",
                "globstar",
                "-O",
                "nullglob",
                "-c",
                "cd \"$2\" || exit 1; pattern=\"$1\"; for f in $pattern; do [ -f \"$f\" ] && printf \"%s\\n\" \"$f\"; done",
                "_",
                pattern,
                path,
            ])
            .current_dir(&self.workspace)
            .output()
            .await
            .map_err(|e| TyclawError::Tool {
                tool: "sandbox_glob_search".into(),
                message: format!("glob failed: {e}"),
            })?;

        if !output.status.success() {
            return Err(TyclawError::Tool {
                tool: "sandbox_glob_search".into(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let mut entries = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
        {
            let full = self.workspace.join(path).join(line);
            let modified_unix_secs = tokio::fs::metadata(&full)
                .await
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            entries.push(SandboxGlobEntry {
                path: line.replace('\\', "/"),
                modified_unix_secs,
            });
        }
        Ok(entries)
    }

    async fn file_exists(&self, path: &str) -> bool {
        self.workspace.join(path).exists()
    }

    async fn remove_file(&self, path: &str) -> Result<(), TyclawError> {
        let full = self.workspace.join(path);
        tokio::fs::remove_file(&full)
            .await
            .map_err(|e| TyclawError::Tool {
                tool: "sandbox_remove".into(),
                message: format!("Remove failed: {e}"),
            })
    }

    async fn copy_from(
        &self,
        container_path: &str,
        host_path: &PathBuf,
    ) -> Result<(), TyclawError> {
        // Noop: container_path 就是 host 路径，直接 copy
        let src = self.workspace.join(container_path);
        if let Some(parent) = host_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::copy(&src, host_path)
            .await
            .map_err(|e| TyclawError::Tool {
                tool: "sandbox_copy".into(),
                message: format!("Copy failed: {e}"),
            })?;
        Ok(())
    }

    fn workspace_root(&self) -> &str {
        self.workspace.to_str().unwrap_or(".")
    }

    fn id(&self) -> &str {
        &self.id
    }
}

/// Noop 沙箱池：不创建容器，直接返回 NoopSandbox。
pub struct NoopPool {
    _workspace: PathBuf,
}

impl NoopPool {
    pub fn new(workspace: PathBuf) -> Self {
        tracing::warn!("Using NoopPool — no sandbox isolation, all tools execute on host");
        Self {
            _workspace: workspace,
        }
    }
}

#[async_trait]
impl SandboxPool for NoopPool {
    async fn acquire(
        &self,
        task_workspace: &PathBuf,
        _data_mounts: &[PathMount],
    ) -> Result<std::sync::Arc<dyn Sandbox>, TyclawError> {
        Ok(std::sync::Arc::new(NoopSandbox {
            workspace: task_workspace.clone(),
            id: "noop".into(),
        }))
    }

    async fn release(
        &self,
        _sandbox: std::sync::Arc<dyn Sandbox>,
        _task_workspace: &PathBuf,
    ) -> Result<(), TyclawError> {
        // Noop: 没有容器需要清理
        Ok(())
    }

    async fn available_count(&self) -> usize {
        usize::MAX // 无限
    }

    async fn total_count(&self) -> usize {
        0
    }

    async fn is_available(&self) -> bool {
        true // 永远可用
    }
}
