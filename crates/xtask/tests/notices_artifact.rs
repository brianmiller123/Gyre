//! 生成物契约测试（H35）：`THIRD-PARTY-NOTICES.txt` 必须自洽且覆盖 vendor fork。
//!
//! 这里**不重新运行 cargo**（避免嵌套 cargo 的锁竞争）：断言已提交生成物的结构契约——
//! 统计行与明细行一致、四个 vendor fork 在列、许可证全文段非空、头部标明生成方式。
//! 生成物与依赖图的**新鲜度**由 `cargo xtask notices --check`（CI 门禁）保证。

use std::collections::HashSet;
use std::path::PathBuf;

fn notices_path() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop(); // crates/xtask → crates
    dir.pop(); // crates → root
    dir.join("THIRD-PARTY-NOTICES.txt")
}

#[test]
fn notices_artifact_is_self_consistent() {
    let text = std::fs::read_to_string(notices_path())
        .expect("THIRD-PARTY-NOTICES.txt 应存在（运行 `cargo xtask notices`）");

    // 1) 头部标明生成方式与禁止手工编辑。
    assert!(
        text.starts_with("THIRD-PARTY-NOTICES\n"),
        "头部应声明文件性质"
    );
    assert!(
        text.contains("cargo xtask notices") && text.contains("请勿手工编辑"),
        "应说明生成命令并禁止手工编辑"
    );

    // 2) 统计行与「source links」明细行数一致（自洽性）。
    let declared: usize = text
        .lines()
        .find_map(|l| {
            l.strip_prefix("共收录 ")
                .and_then(|rest| rest.split(' ').next())
                .and_then(|n| n.parse().ok())
        })
        .expect("应含 `共收录 N 个第三方包` 统计行");
    let detail_section = text
        .split("## Packages with source links")
        .nth(1)
        .expect("应含 source links 段");
    let detail_end = detail_section
        .find("\n## ")
        .map_or(detail_section.len(), |i| i);
    let detail_lines = detail_section[..detail_end]
        .lines()
        .filter(|l| l.starts_with("- "))
        .count();
    assert_eq!(
        declared, detail_lines,
        "统计行（{declared}）与明细行数（{detail_lines}）不一致"
    );
    assert!(declared > 100, "第三方包数应达到三位数，实际 {declared}");

    // 3) vendor/ 下的四个 fork 必须出现在通知里（MIT 义务，不可被  --include-local 类
    //    过滤漏掉——它们同时是 workspace 成员）。
    let names: HashSet<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("- "))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    for fork in ["pi-shell", "pi-builtins", "pi-walker", "brush-core"] {
        assert!(names.contains(fork), "通知缺少 vendor fork {fork}");
    }

    // 4) 许可证全文段非空，且同时含 MIT 与 Apache-2.0 正文（许可证履行要件）。
    let texts = text
        .split("## License texts")
        .nth(1)
        .expect("应含 License texts 段");
    assert!(
        texts.contains("Permission is hereby granted, free of charge"),
        "缺少 MIT 正文"
    );
    assert!(
        texts.contains("Apache License") && texts.contains("Version 2.0"),
        "缺少 Apache-2.0 正文"
    );
    assert!(
        texts.matches("### [").count() > 10,
        "去重后的许可证文本应有两位数以上，实际 {}",
        texts.matches("### [").count()
    );
}
