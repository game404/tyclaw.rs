//! Docker 沙箱集成测试。
//! 需要本地 Docker 可用 + tyclaw-sandbox:latest 镜像已构建。
//! 运行：cargo test -p tyclaw-sandbox --test docker_test

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tyclaw_sandbox::*;

static TEMP_DIR_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// 创建临时 users 目录（DockerPool 的 per-user 根目录）。
fn temp_users_dir() -> PathBuf {
    let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "tyclaw-sandbox-test-{}-{sequence}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("skills")).ok();
    std::fs::create_dir_all(dir.join("tools")).ok();
    std::fs::write(dir.join("defaults.py"), "").ok();
    dir
}

/// 构造 per-user workspace 路径：{users_dir}/{user_id}/work
fn user_workspace(users_dir: &PathBuf, user_id: &str) -> PathBuf {
    let ws = users_dir.join(user_id).join("work");
    std::fs::create_dir_all(&ws).ok();
    ws
}

fn cleanup(dir: &PathBuf) {
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn test_docker_pool_create() {
    let config = DockerConfig::default();
    let users_dir = temp_users_dir();
    let pool = DockerPool::new(config, users_dir.clone()).await;
    assert!(pool.is_ok(), "Pool creation failed: {:?}", pool.err());
    cleanup(&users_dir);
}

#[tokio::test]
async fn test_docker_exec() {
    let config = DockerConfig::default();
    let users_dir = temp_users_dir();
    let pool = DockerPool::new(config, users_dir.clone())
        .await
        .expect("Pool creation failed");
    let ws = user_workspace(&users_dir, "test_exec");

    let sandbox = pool
        .acquire("test_exec", &ws, &[])
        .await
        .expect("Acquire failed");
    let result = sandbox
        .exec("echo hello", Duration::from_secs(10))
        .await
        .expect("Exec failed");

    assert_eq!(result.stdout.trim(), "hello");
    assert_eq!(result.exit_code, 0);
    assert!(!result.timed_out);

    pool.release(sandbox, &ws).await.expect("Release failed");
    cleanup(&users_dir);
}

#[tokio::test]
async fn test_docker_python() {
    let config = DockerConfig::default();
    let users_dir = temp_users_dir();
    let pool = DockerPool::new(config, users_dir.clone())
        .await
        .expect("Pool creation failed");
    let ws = user_workspace(&users_dir, "test_python");

    let sandbox = pool
        .acquire("test_python", &ws, &[])
        .await
        .expect("Acquire failed");
    let result = sandbox
        .exec(
            "python3 -c 'import pandas; print(pandas.__version__)'",
            Duration::from_secs(30),
        )
        .await
        .expect("Exec failed");

    assert_eq!(result.exit_code, 0, "Python exec failed: {}", result.stderr);
    assert!(!result.stdout.trim().is_empty(), "No pandas version output");
    println!("pandas version: {}", result.stdout.trim());

    pool.release(sandbox, &ws).await.expect("Release failed");
    cleanup(&users_dir);
}

#[tokio::test]
async fn test_docker_write_read() {
    let config = DockerConfig::default();
    let users_dir = temp_users_dir();
    let pool = DockerPool::new(config, users_dir.clone())
        .await
        .expect("Pool creation failed");
    let ws = user_workspace(&users_dir, "test_writerw");

    let sandbox = pool
        .acquire("test_writerw", &ws, &[])
        .await
        .expect("Acquire failed");

    // 写文件
    sandbox
        .write_file("work/test.txt", b"hello sandbox")
        .await
        .expect("Write failed");

    // 读文件
    let content = sandbox
        .read_file("work/test.txt")
        .await
        .expect("Read failed");
    assert_eq!(String::from_utf8_lossy(&content), "hello sandbox");

    // 文件存在
    assert!(sandbox.file_exists("work/test.txt").await);
    assert!(!sandbox.file_exists("nonexistent.txt").await);

    // release 后 workspace 应该同步回 host
    pool.release(sandbox, &ws).await.expect("Release failed");

    // 验证文件同步回 host
    let host_file = ws.join("test.txt");
    assert!(
        host_file.exists(),
        "File not synced to host: {:?}",
        host_file
    );
    let host_content = std::fs::read_to_string(&host_file).expect("Read host file failed");
    assert_eq!(host_content, "hello sandbox");

    cleanup(&users_dir);
}

#[tokio::test]
async fn test_docker_list_dir() {
    let config = DockerConfig::default();
    let users_dir = temp_users_dir();
    let pool = DockerPool::new(config, users_dir.clone())
        .await
        .expect("Pool creation failed");
    let ws = user_workspace(&users_dir, "test_listdir");

    let sandbox = pool
        .acquire("test_listdir", &ws, &[])
        .await
        .expect("Acquire failed");

    sandbox.write_file("a.txt", b"aaa").await.unwrap();
    sandbox.write_file("b.txt", b"bbb").await.unwrap();
    sandbox.write_file("sub/c.txt", b"ccc").await.unwrap();

    let entries = sandbox.list_dir(".").await.expect("List dir failed");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"), "Missing a.txt in {:?}", names);
    assert!(names.contains(&"b.txt"), "Missing b.txt in {:?}", names);
    assert!(names.contains(&"sub"), "Missing sub/ in {:?}", names);

    let sub_entry = entries.iter().find(|e| e.name == "sub").unwrap();
    assert!(sub_entry.is_dir);

    pool.release(sandbox, &ws).await.expect("Release failed");
    cleanup(&users_dir);
}

#[tokio::test]
async fn test_docker_host_isolation() {
    let config = DockerConfig::default();
    let users_dir = temp_users_dir();
    let pool = DockerPool::new(config, users_dir.clone())
        .await
        .expect("Pool creation failed");
    let ws = user_workspace(&users_dir, "test_isolation");

    let sandbox = pool
        .acquire("test_isolation", &ws, &[])
        .await
        .expect("Acquire failed");

    // 容器内应该看不到 host 的文件
    let result = sandbox
        .exec("cat /etc/hostname", Duration::from_secs(5))
        .await
        .unwrap();
    // 容器有自己的 hostname，不是 host 的
    assert_eq!(result.exit_code, 0);

    // 不能访问 host 的 home 目录
    let result = sandbox
        .exec("ls /Users 2>&1 || echo NOT_FOUND", Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        result.stdout.contains("NOT_FOUND") || result.stdout.contains("No such file"),
        "Container should not see /Users: {}",
        result.stdout
    );

    pool.release(sandbox, &ws).await.expect("Release failed");
    cleanup(&users_dir);
}

#[tokio::test]
async fn test_docker_file_ownership() {
    let config = DockerConfig::default();
    assert!(
        config.run_as_host_user,
        "run_as_host_user should default to true"
    );
    assert_eq!(config.memory, "2g", "memory should default to 2g");
    assert_eq!(
        config.memory_swap, "2g",
        "memory_swap should equal memory (swap disabled)"
    );
    assert_eq!(config.cpus, "2", "cpus should default to 2");
    assert_eq!(config.shm_size, "512m", "shm_size should default to 512m");

    let users_dir = temp_users_dir();
    let pool = DockerPool::new(config, users_dir.clone())
        .await
        .expect("Pool creation failed");
    let ws = user_workspace(&users_dir, "test_ownership");

    let sandbox = pool
        .acquire("test_ownership", &ws, &[])
        .await
        .expect("Acquire failed");

    sandbox
        .write_file("work/owned.txt", b"host-deletable")
        .await
        .expect("Write failed");
    sandbox
        .exec(
            "mkdir -p work/tmp/subdir && echo data > work/tmp/subdir/deep.txt",
            Duration::from_secs(5),
        )
        .await
        .expect("Exec via shell failed");

    pool.release(sandbox, &ws).await.expect("Release failed");

    let host_file = ws.join("owned.txt");
    assert!(host_file.exists(), "File should exist on host");
    std::fs::remove_file(&host_file)
        .expect("Host user should be able to delete container-created file");

    let tmp_subdir = ws.join("tmp/subdir");
    if tmp_subdir.exists() {
        std::fs::remove_dir_all(&tmp_subdir)
            .expect("Host user should be able to remove_dir_all container-created dirs");
    }

    cleanup(&users_dir);
}

#[tokio::test]
async fn test_docker_exec_timeout() {
    let config = DockerConfig::default();
    let users_dir = temp_users_dir();
    let pool = DockerPool::new(config, users_dir.clone())
        .await
        .expect("Pool creation failed");
    let ws = user_workspace(&users_dir, "test_timeout");

    let sandbox = pool
        .acquire("test_timeout", &ws, &[])
        .await
        .expect("Acquire failed");

    let result = sandbox
        .exec("sleep 30", Duration::from_secs(2))
        .await
        .expect("Exec failed");
    assert!(result.timed_out, "Should have timed out");

    pool.release(sandbox, &ws).await.expect("Release failed");
    cleanup(&users_dir);
}

#[tokio::test]
async fn test_timeout_does_not_kill_unrelated_process_group() {
    let users_dir = temp_users_dir();
    let pool = DockerPool::new(DockerConfig::default(), users_dir.clone())
        .await.expect("Pool creation failed");
    let ws = user_workspace(&users_dir, "test_targeted_timeout");
    let sandbox = pool.acquire("test_targeted_timeout", &ws, &[])
        .await.expect("Acquire failed");
    let started = tokio::process::Command::new("docker").args([
        "exec", sandbox.id(), "sh", "-c",
        "setsid sleep 30 >/dev/null 2>&1 & echo $! > /workspace/work/tmp/unrelated.pid",
    ]).output().await.expect("Failed to start unrelated process");
    assert!(started.status.success());

    let result = sandbox.exec("sleep 30", Duration::from_secs(1)).await.expect("Exec failed");
    assert!(result.timed_out);
    let check = sandbox.exec("kill -0 $(cat work/tmp/unrelated.pid)", Duration::from_secs(5))
        .await.expect("Failed to inspect unrelated process");
    assert_eq!(check.exit_code, 0, "unrelated process was terminated");
    let _ = sandbox.exec("kill $(cat work/tmp/unrelated.pid) 2>/dev/null || true", Duration::from_secs(5)).await;
    pool.release(sandbox, &ws).await.expect("Release failed");
    cleanup(&users_dir);
}

/// 回归测试：曾经触发 `invalid mode: /workspace` 的 workspace_key 现在能正常 acquire+exec。
///
/// 关键路径：
/// 1. `workspace_key` 含 `:` `+` `=`（钉钉 chat_id 真实形态）；
/// 2. control 把 leaf 清洗为单层不含坏字符的目录名；
/// 3. sandbox `--mount type=bind` 不再依赖 `-v` 解析；
/// 4. 容器名 sanitize 后符合 Docker 命名规范。
///
/// 若环境不支持创建该 leaf（理论上 Linux/macOS 都行）则 fail，便于发现回归。
#[tokio::test]
async fn test_docker_acquire_with_base64_chat_id_key() {
    // 直接借用 control 的清洗算法，避免脚本/代码漂移
    let workspace_key = "+GmQ==:test_base64_chat_id";
    let leaf = tyclaw_control::filesystem_workspace_leaf(workspace_key);
    assert!(
        !leaf.contains(':') && !leaf.contains('+') && !leaf.contains('='),
        "leaf must not contain raw : + = chars, got '{leaf}'"
    );

    let config = DockerConfig::default();
    let users_dir = temp_users_dir();
    let pool = DockerPool::new(config, users_dir.clone())
        .await
        .expect("Pool creation failed");

    // workspace_key 只用于容器名和缓存键；实际挂载源始终由 task_workspace 决定。
    let task_workspace = user_workspace(&users_dir, &leaf);
    let sandbox = pool
        .acquire(workspace_key, &task_workspace, &[])
        .await
        .expect("Acquire failed: 含 + = : 的 workspace_key 应能挂载成功");

    let result = sandbox
        .exec("echo ok", Duration::from_secs(10))
        .await
        .expect("Exec failed");
    assert_eq!(result.stdout.trim(), "ok");
    assert_eq!(result.exit_code, 0);

    // 验证容器名也已清洗（不含 Docker 非法字符）
    let id = sandbox.id();
    for c in id.chars() {
        assert!(
            c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-',
            "container id should be docker-legal, got '{id}' (bad char '{c}')"
        );
    }

    pool.release(sandbox, &task_workspace).await.ok();
    // 用名称兜底删除（防止 leak）
    let _ = tokio::process::Command::new("docker")
        .args([
            "rm",
            "-f",
            &tyclaw_sandbox::sanitize_container_name(workspace_key),
        ])
        .output()
        .await;
    cleanup(&users_dir);
}

/// 回归测试：自定义 works 目录必须作为实际挂载源，且同一 key 改变挂载源时重建容器。
#[tokio::test]
async fn test_docker_uses_task_workspace_and_rebuilds_stale_mount() {
    let config = DockerConfig::default();
    let run_dir = temp_users_dir().join("run-dir");
    let custom_works = temp_users_dir().join("custom-works");
    std::fs::create_dir_all(run_dir.join("skills")).ok();
    std::fs::create_dir_all(run_dir.join("tools")).ok();
    std::fs::write(run_dir.join("defaults.py"), "").ok();
    let pool = DockerPool::new(config, run_dir.clone())
        .await
        .expect("Pool creation failed");
    let workspace_key = "test_custom_works_mount";
    let first_work = user_workspace(&custom_works, "first");
    let second_work = user_workspace(&custom_works, "second");
    std::fs::write(first_work.join("mount-marker.txt"), "first").unwrap();
    std::fs::write(second_work.join("mount-marker.txt"), "second").unwrap();

    let first = pool
        .acquire(workspace_key, &first_work, &[])
        .await
        .expect("First acquire failed");
    assert_eq!(
        String::from_utf8(
            first
                .read_workspace_file("mount-marker.txt", 100)
                .await
                .unwrap()
        )
        .unwrap(),
        "first"
    );
    first
        .exec(
            "printf reused > /tmp/tyclaw-mount-reuse-marker",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    pool.release(first, &first_work).await.unwrap();

    let reused = pool
        .acquire(workspace_key, &first_work, &[])
        .await
        .expect("Acquire with unchanged mount failed");
    let marker = reused
        .exec("cat /tmp/tyclaw-mount-reuse-marker", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(marker.stdout, "reused");
    pool.release(reused, &first_work).await.unwrap();

    let second = pool
        .acquire(workspace_key, &second_work, &[])
        .await
        .expect("Acquire with changed mount failed");
    assert_eq!(
        String::from_utf8(
            second
                .read_workspace_file("mount-marker.txt", 100)
                .await
                .unwrap()
        )
        .unwrap(),
        "second"
    );
    pool.release(second, &second_work).await.ok();

    let _ = tokio::process::Command::new("docker")
        .args(["rm", "-f", &sanitize_container_name(workspace_key)])
        .output()
        .await;
    cleanup(&run_dir);
    cleanup(&custom_works);
}
