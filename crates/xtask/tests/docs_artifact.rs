//! 生成物契约测试（H38）：`docs/tools.md` 必须覆盖各分组与关键工具，且结构自洽。
//!
//! 与 `notices_artifact` 同策略：不重新装配工具（生成器本身已有单测），只断言已提交
//! 生成物的结构契约；与当前工具面的一致性由 `cargo xtask docs --tools --check`（CI
//! release preflight）保证。

use std::path::PathBuf;

fn docs_path() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop();
    dir.pop();
    dir.join("docs").join("tools.md")
}

#[test]
fn tools_doc_covers_groups_and_flagship_tools() {
    let text = std::fs::read_to_string(docs_path())
        .expect("docs/tools.md 应存在（运行 `cargo xtask docs --tools`）");

    // 1) 头部声明生成方式（防手工编辑）。
    assert!(text.starts_with("# 工具参考"));
    assert!(
        text.contains("cargo xtask docs --tools") && text.contains("请勿手工编辑"),
        "应说明生成命令并禁止手工编辑"
    );

    // 2) 统计行与概览表行数一致（结构自洽）。
    let declared: usize = text
        .lines()
        .find_map(|l| {
            l.strip_prefix("共 **")
                .and_then(|rest| rest.split('*').next())
                .and_then(|n| n.parse().ok())
        })
        .expect("应含 `共 **N** 个工具` 统计行");
    let overview = text
        .split("## 概览")
        .nth(1)
        .and_then(|s| s.split("## 参数详情").next())
        .expect("应含概览表");
    let rows = overview.lines().filter(|l| l.starts_with("| `")).count();
    assert_eq!(declared, rows, "统计行与概览表行数不一致");
    assert!(
        declared >= 25,
        "工具数应覆盖核心+可选+会话+宿主，实际 {declared}"
    );

    // 3) 每个分组都要有工具（缺一组说明生成器漏装配）。
    for group in ["| 核心 |", "| 可选 |", "| 会话 |", "| 宿主 |"] {
        assert!(overview.contains(group), "概览表缺少分组 {group}");
    }

    // 4) 关键工具在列（覆盖文件/执行/搜索/会话/宿主五类）。
    for name in [
        "read_file",
        "write_file",
        "run_command",
        "grep",
        "glob",
        "todo",
        "goal",
        "task",
        "hub",
        "ask",
    ] {
        assert!(
            text.contains(&format!("### `{name}`")),
            "工具参考缺少 {name} 的详情段"
        );
    }

    // 5) 可选工具标注启用开关（用户据此知道怎么开）。
    assert!(
        text.contains("`[tools] ast = true`"),
        "可选工具应标注 `[tools] <key> = true`"
    );
    // 6) 至少一个工具带参数表（read_file 的 path 必填）。
    let read_section = text.split("### `read_file`").nth(1).expect("read_file 段");
    assert!(read_section.contains("| 参数 | 类型 | 必填 | 说明 |"));
    assert!(
        read_section.contains("| `path` | `string` | 是 |"),
        "read_file.path 应为必填 string"
    );
}
