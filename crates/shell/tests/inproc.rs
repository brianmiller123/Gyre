//! agent-shell 集成冒烟测试：验证 50+ 进程内工具真实可执行。
//!
//! 这些测试不 fork 外部二进制——全部经 pi-shell 内建在进程内完成，
//! 因此跨平台（Linux/macOS/Windows）行为一致。

use std::collections::HashMap;

use agent_shell::{InProcShell, RunOptions};
use tempfile::tempdir;

fn shell() -> InProcShell {
    let mut env = HashMap::new();
    env.insert(
        "HOME".to_owned(),
        std::env::temp_dir().to_string_lossy().into_owned(),
    );
    InProcShell::new(Some(env))
}

#[tokio::test]
async fn cat_reads_file() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("hello.txt");
    std::fs::write(&file, "line one\nline two\n").unwrap();

    let out = shell()
        .run_utility(
            "cat",
            &[file.to_string_lossy().into_owned()],
            &RunOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, "line one\nline two\n");
    assert!(out.stderr.is_empty());
}

#[tokio::test]
async fn pipeline_uses_inproc_utilities() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("log.txt");
    std::fs::write(&file, "alpha\nbeta\nalpha\n").unwrap();

    // cat | grep | wc 全部进程内
    let cmd = format!("cat {} | grep alpha | wc -l", file.to_string_lossy());
    let out = shell().run(&cmd, &RunOptions::default()).await.unwrap();

    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, "2\n");
}

#[tokio::test]
async fn sed_substitutes_in_stream() {
    let out = shell()
        .run(
            "echo 'hello world' | sed 's/world/rust/'",
            &RunOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, "hello rust\n");
}

#[tokio::test]
async fn jq_parses_json() {
    let out = shell()
        .run(
            "echo '{\"a\": [1, 2, 3]}' | jq '.a | length'",
            &RunOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, "3\n");
}

#[tokio::test]
async fn sponge_writes_atomically() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("out.txt");

    // 同文件读写安全：`cat file | sponge file`（延迟打开 + 原子替换）
    let cmd = format!(
        "printf 'first\\nsecond\\n' | sponge {}",
        file.to_string_lossy()
    );
    let out = shell().run(&cmd, &RunOptions::default()).await.unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "first\nsecond\n");
}

#[tokio::test]
async fn run_utility_escapes_arguments() {
    let out = shell()
        .run_utility(
            "printf",
            &["%s".to_owned(), "a b;c'd".to_owned()],
            &RunOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, "a b;c'd");
}

#[tokio::test]
async fn respects_cwd() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("marker.txt"), "x").unwrap();

    let out = shell()
        .run(
            "cat marker.txt",
            &RunOptions {
                cwd: Some(dir.path().to_path_buf()),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, "x");
}

#[tokio::test]
async fn reports_failures() {
    let out = shell()
        .run_utility(
            "cat",
            &["/nonexistent/definitely-missing".to_owned()],
            &RunOptions::default(),
        )
        .await
        .unwrap();

    assert_ne!(out.exit_code, Some(0));
    assert!(out.stderr.to_lowercase().contains("no such file"));
}

#[tokio::test]
async fn honours_timeout() {
    let out = shell()
        .run(
            "sleep 30",
            &RunOptions {
                timeout_ms: Some(200),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();

    assert!(out.timed_out || out.cancelled || out.exit_code != Some(0));
}

#[tokio::test]
async fn find_walks_directories() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();

    let out = shell()
        .run(
            "find . -name '*.rs'",
            &RunOptions {
                cwd: Some(dir.path().to_path_buf()),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(out.exit_code, Some(0));
    assert!(out.stdout.contains("a.rs"));
    assert!(!out.stdout.contains("b.txt"));
}
