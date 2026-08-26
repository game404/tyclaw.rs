//! Noop 沙箱 —— 无隔离，直接在 host 执行。
//!
//! 用于调试模式或 Docker 不可用时的 fallback。
//! 行为与现有 ExecTool/ReadFileTool 完全一致。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tyclaw_types::TyclawError;

use crate::types::*;
use crate::{validate_workspace_relative_path, workspace_read_error};

#[cfg(target_os = "linux")]
async fn cleanup_run_processes(run_id: &str) -> bool {
    let marker = format!("TYCLAW_EXEC_RUN_ID={run_id}").into_bytes();
    let pids: Vec<String> = std::fs::read_dir("/proc").into_iter().flatten().flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|pid| pid.chars().all(|c| c.is_ascii_digit()))
        .filter(|pid| std::fs::read(format!("/proc/{pid}/environ")).ok().is_some_and(|env| env.split(|byte| *byte == 0).any(|item| item == marker)))
        .collect();
    if pids.is_empty() { return false; }
    let _ = Command::new("kill").arg("-TERM").args(&pids).output().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = Command::new("kill").arg("-KILL").args(&pids).output().await;
    true
}
#[cfg(not(target_os = "linux"))]
async fn cleanup_run_processes(_run_id: &str) -> bool { false }

/// Noop 沙箱：直接在 host 上执行，无隔离。
pub struct NoopSandbox {
    workspace: PathBuf,
    id: String,
}

#[async_trait]
impl Sandbox for NoopSandbox {
    async fn exec(&self, cmd: &str, timeout: Duration) -> Result<SandboxExecResult, TyclawError> {
        self.exec_with_context(cmd, timeout, SandboxExecContext::default()).await
    }
    async fn exec_with_context(&self, cmd: &str, timeout: Duration, context: SandboxExecContext) -> Result<SandboxExecResult, TyclawError> {
        let run_id = uuid::Uuid::new_v4().simple().to_string();
        let mut process = Command::new("sh");
        process
                .arg("-c")
                .arg(cmd)
                .env("TYCLAW_EXEC_RUN_ID", &run_id)
                .current_dir(&self.workspace)
                .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(unix)] process.process_group(0);
        let child = process.spawn().map_err(|e| TyclawError::Tool {
            tool: "sandbox_exec".into(), message: format!("Failed to execute: {e}") })?;
        let pgid = child.id();
        let output = child.wait_with_output();
        tokio::pin!(output);
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        enum Finish { Output(std::io::Result<std::process::Output>), Timeout, Cancelled }
        let finish = if let Some(token) = context.cancellation {
            tokio::select! { result = &mut output => Finish::Output(result), _ = &mut deadline => Finish::Timeout, _ = token.cancelled() => Finish::Cancelled }
        } else {
            tokio::select! { result = &mut output => Finish::Output(result), _ = &mut deadline => Finish::Timeout }
        };

        match finish {
            Finish::Timeout | Finish::Cancelled => {
                #[cfg(unix)] if let Some(pgid) = pgid {
                    let group = format!("-{pgid}");
                    let _ = Command::new("kill").args(["-TERM", "--", &group]).output().await;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = Command::new("kill").args(["-KILL", "--", &group]).output().await;
                }
                let cancelled = matches!(finish, Finish::Cancelled);
                cleanup_run_processes(&run_id).await;
                Ok(SandboxExecResult {
                stdout: String::new(),
                stderr: if cancelled { "Command cancelled".into() } else { String::new() },
                exit_code: -1,
                timed_out: !cancelled,
                termination: cancelled.then_some(SandboxTermination::Cancelled),
            })}
            Finish::Output(Err(e)) => Err(TyclawError::Tool {
                tool: "sandbox_exec".into(),
                message: format!("Failed to execute: {e}"),
            }),
            Finish::Output(Ok(output)) => {
                let detached = cleanup_run_processes(&run_id).await;
                Ok(SandboxExecResult { stdout: String::from_utf8_lossy(&output.stdout).to_string(), stderr: String::from_utf8_lossy(&output.stderr).to_string(), exit_code: if detached { -1 } else { output.status.code().unwrap_or(-1) }, timed_out: false, termination: detached.then_some(SandboxTermination::DetachedProcessDetected) })
            },
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

    async fn read_workspace_file(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, TyclawError> {
        let relative = validate_workspace_relative_path(path)?;
        let root = tokio::fs::canonicalize(&self.workspace)
            .await
            .map_err(|_| workspace_read_error("Workspace work directory is not accessible"))?;
        let target = tokio::fs::canonicalize(root.join(relative))
            .await
            .map_err(|_| workspace_read_error("Workspace file was not found"))?;
        if !target.starts_with(&root) {
            return Err(workspace_read_error(
                "Workspace file resolves outside the work directory",
            ));
        }
        let file = tokio::fs::File::open(&target)
            .await
            .map_err(|_| workspace_read_error("Workspace file could not be read"))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|_| workspace_read_error("Workspace file metadata is not accessible"))?;
        if !metadata.is_file() {
            return Err(workspace_read_error("Workspace path is not a regular file"));
        }
        if metadata.len() > max_bytes as u64 {
            return Err(workspace_read_error(
                "Workspace file exceeds the size limit",
            ));
        }
        let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
        file.take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| workspace_read_error("Workspace file could not be read"))?;
        if bytes.len() > max_bytes {
            return Err(workspace_read_error(
                "Workspace file exceeds the size limit",
            ));
        }
        Ok(bytes)
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
        _workspace_key: &str,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sandbox(root: &Path) -> NoopSandbox {
        NoopSandbox {
            workspace: root.to_path_buf(),
            id: "test".into(),
        }
    }

    #[tokio::test]
    async fn secure_workspace_read_accepts_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("reports")).unwrap();
        std::fs::write(directory.path().join("reports/daily.md"), "日报").unwrap();

        let bytes = sandbox(directory.path())
            .read_workspace_file("reports/daily.md", 100)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "日报");
    }

    #[tokio::test]
    async fn secure_workspace_read_rejects_absolute_parent_and_oversize_paths() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("large.md"), b"12345").unwrap();
        let sandbox = sandbox(directory.path());

        for path in ["/etc/passwd", "../outside.md", "reports/../../outside.md"] {
            let error = sandbox.read_workspace_file(path, 100).await.unwrap_err();
            assert!(!error
                .to_string()
                .contains(&directory.path().display().to_string()));
        }
        let error = sandbox
            .read_workspace_file("large.md", 4)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("size limit"));
        assert!(!error
            .to_string()
            .contains(&directory.path().display().to_string()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn secure_workspace_read_allows_internal_symlink_and_rejects_external_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("inside.md"), "inside").unwrap();
        std::fs::write(outside.path().join("outside.md"), "outside").unwrap();
        symlink("inside.md", directory.path().join("inside-link.md")).unwrap();
        symlink(
            outside.path().join("outside.md"),
            directory.path().join("outside-link.md"),
        )
        .unwrap();
        let sandbox = sandbox(directory.path());

        let bytes = sandbox
            .read_workspace_file("inside-link.md", 100)
            .await
            .unwrap();
        assert_eq!(bytes, b"inside");
        let error = sandbox
            .read_workspace_file("outside-link.md", 100)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("outside the work directory"));
        assert!(!error
            .to_string()
            .contains(&outside.path().display().to_string()));
    }
}
