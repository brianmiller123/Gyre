//! 第三方许可证通知生成（H35）。
//!
//! 数据源：`cargo metadata --format-version 1 --all-features`（含 `vendor/` 下的 path
//! 依赖——`about.toml` 注释里点名的四个 fork 必须出现在通知里）。输出**确定性**：
//! 包按 `(name, version)` 排序、许可证文本按内容去重后按首次出现顺序编号。
//!
//! 每个包只写「名称 版本 — 许可证 — 上游链接」，完整许可证文本集中在末尾的
//! `License texts` 段（按内容去重，标注使用它的包数），因此文件体积与依赖数线性相关
//! 而非乘性相关。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// 生成结果（供 `--check` 与单测复用）。
pub struct Generated {
    /// 完整通知文本。
    pub text: String,
    /// 收录的第三方包数。
    pub packages: usize,
    /// 去重后的许可证文本数。
    pub license_texts: usize,
}

/// `cargo metadata` 解析出的包（只保留通知需要的字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkg {
    /// crate 名。
    pub name: String,
    /// 版本。
    pub version: String,
    /// SPDX 许可证表达式（缺失为 `None`）。
    pub license: Option<String>,
    /// 上游仓库/主页（可读来源）。
    pub url: Option<String>,
    /// 包根目录（读 LICENSE 文件用）。
    pub root: PathBuf,
}

/// 运行 `cargo metadata` 并解析出第三方包列表（排除工作区自身成员）。
///
/// # Errors
/// `cargo metadata` 启动失败、非零退出，或输出非法 JSON。
pub fn collect_packages() -> Result<Vec<Pkg>, String> {
    let out = std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--all-features",
            "--locked",
        ])
        .output()
        .map_err(|e| format!("无法运行 cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata 退出码 {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    // 只排除**本仓库自己的** crates（`<root>/crates/*`）：`vendor/` 下的 fork 虽因
    // Cargo 的 path-dependency 自动入册也成为 workspace 成员，但它们仍是第三方代码，
    // 必须出现在通知里（about.toml 注释点名的四项 MIT 义务）。
    parse_metadata(
        &String::from_utf8_lossy(&out.stdout),
        Some(&workspace_root().join("crates")),
    )
}

/// 从 `cargo metadata` JSON 提取第三方包（纯函数，便于单测）。
///
/// `own_crates_dir`：本仓库自有 crate 的根（`<root>/crates`）。工作区成员中位于该目录下
/// 的条目被排除（自己的代码不是第三方）；`vendor/` 下的 path fork 虽同为工作区成员，
/// 但会保留（第三方代码）。
///
/// # Errors
/// JSON 结构不符合预期（缺 `packages` / `workspace_members`）。
pub fn parse_metadata(json: &str, own_crates_dir: Option<&Path>) -> Result<Vec<Pkg>, String> {
    let v: Value =
        serde_json::from_str(json).map_err(|e| format!("metadata JSON 解析失败: {e}"))?;
    let members: std::collections::HashSet<&str> = v
        .get("workspace_members")
        .and_then(Value::as_array)
        .ok_or("metadata 缺少 workspace_members")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let packages = v
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("metadata 缺少 packages")?;
    let mut out = Vec::new();
    for p in packages {
        let id = p.get("id").and_then(Value::as_str).unwrap_or_default();
        let manifest = p
            .get("manifest_path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let root = Path::new(manifest)
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        if members.contains(id) && own_crates_dir.is_some_and(|dir| root.starts_with(dir)) {
            continue; // 本仓库自有 crate 不属于第三方
        }
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .ok_or("package 缺少 name")?
            .to_string();
        let version = p
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("0.0.0")
            .to_string();
        let license = p.get("license").and_then(Value::as_str).map(str::to_string);
        let url = p
            .get("repository")
            .and_then(Value::as_str)
            .or_else(|| p.get("homepage").and_then(Value::as_str))
            .map(str::to_string);
        out.push(Pkg {
            name,
            version,
            license,
            url,
            root,
        });
    }
    out.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    Ok(out)
}

/// 渲染通知文本。
#[must_use]
pub fn render(packages: &[Pkg]) -> Generated {
    let mut by_license: BTreeMap<String, Vec<&Pkg>> = BTreeMap::new();
    let mut no_license: Vec<&Pkg> = Vec::new();
    for p in packages {
        match &p.license {
            Some(l) => by_license.entry(l.clone()).or_default().push(p),
            None => no_license.push(p),
        }
    }

    let mut text = String::new();
    text.push_str("THIRD-PARTY-NOTICES\n");
    text.push_str("===================\n\n");
    text.push_str(
        "本文件由 `cargo xtask notices` 生成（数据源：cargo metadata --all-features）。\n\
         请勿手工编辑——依赖变更后重跑该命令；CI 以 `cargo xtask notices --check` 校验漂移。\n\
         完整许可文本见文末 “License texts” 段（按内容去重）。\n",
    );
    // 这是生成物：仓库自身以 workspace MIT 许可分发，第三方包在此逐一列出。
    text.push_str(&format!(
        "\n共收录 {} 个第三方包，去重后 {} 份许可证文本。\n\n",
        packages.len(),
        count_license_texts(packages),
    ));
    text.push_str("## Packages by license\n\n");
    for (license, pkgs) in &by_license {
        text.push_str(&format!("### {license} ({} packages)\n\n", pkgs.len()));
        for p in pkgs {
            text.push_str(&format!("- {} {}\n", p.name, p.version));
        }
        text.push('\n');
    }
    if !no_license.is_empty() {
        text.push_str(&format!(
            "### (未声明许可证) ({} packages)\n\n",
            no_license.len()
        ));
        for p in &no_license {
            text.push_str(&format!("- {} {}\n", p.name, p.version));
        }
        text.push('\n');
    }

    text.push_str("## Packages with source links\n\n");
    let mut indexed: Vec<&Pkg> = packages.iter().collect();
    indexed.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    for p in indexed {
        let url = p.url.as_deref().unwrap_or("(no upstream url)");
        let license = p.license.as_deref().unwrap_or("(none declared)");
        text.push_str(&format!("- {} {} — {license} — {url}\n", p.name, p.version));
    }

    // 许可证全文：按内容去重；同一份文本只出现一次，标注使用它的包数。
    let mut texts: Vec<(String, Vec<String>)> = Vec::new();
    for p in packages {
        for (file, body) in license_files(p) {
            let label = format!("{} {} ({})", p.name, p.version, file);
            if let Some(slot) = texts.iter_mut().find(|(b, _)| *b == body) {
                slot.1.push(label);
            } else {
                texts.push((body, vec![label]));
            }
        }
    }
    text.push_str("\n## License texts\n\n");
    if texts.is_empty() {
        text.push_str("（未能定位到任何许可证文件——请检查依赖包的 LICENSE 文件是否存在）\n");
    }
    for (idx, (body, users)) in texts.iter().enumerate() {
        text.push_str(&format!(
            "\n### [{:03}] 被 {} 个包使用\n\n使用方（前 8 个）：{}\n\n```text\n{}\n```\n",
            idx + 1,
            users.len(),
            users.iter().take(8).cloned().collect::<Vec<_>>().join(", "),
            body.trim_end()
        ));
    }

    Generated {
        text,
        packages: packages.len(),
        license_texts: texts.len(),
    }
}

/// 去重后的许可文本数（与 [`render`] 的统计一致）。
fn count_license_texts(packages: &[Pkg]) -> usize {
    let mut seen: Vec<String> = Vec::new();
    for p in packages {
        for (_, body) in license_files(p) {
            if !seen.contains(&body) {
                seen.push(body);
            }
        }
    }
    seen.len()
}

/// 读取包根目录下的许可证/通知文件（`LICENSE*` / `COPYING*` / `UNLICENSE*` / `NOTICE*`）。
///
/// 返回 `(文件名, 内容)`；内容按原样保留（渲染侧统一 trim 尾部空白）。
#[must_use]
pub fn license_files(pkg: &Pkg) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(&pkg.root) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            if !p.is_file() {
                return false;
            }
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_ascii_uppercase();
            ["LICENSE", "COPYING", "UNLICENSE", "NOTICE"]
                .iter()
                .any(|k| name.starts_with(k))
        })
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            let name = p.file_name()?.to_str()?.to_string();
            Some((name, text))
        })
        .collect()
}

/// 默认输出路径（仓库根）。
#[must_use]
pub fn default_output() -> PathBuf {
    workspace_root().join("THIRD-PARTY-NOTICES.txt")
}

/// 工作区根（xtask 自身 manifest 的祖父目录：`<root>/crates/xtask/Cargo.toml`）。
#[must_use]
pub fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop(); // crates/xtask → crates
    dir.pop(); // crates → <root>
    dir
}

/// `notices` 子命令。
///
/// # Errors
/// 参数非法、`cargo metadata` 失败，或 `--check` 发现漂移。
pub fn run(args: &[String]) -> Result<(), String> {
    let mut check = false;
    let mut output = default_output();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => check = true,
            "--output" => {
                output = PathBuf::from(
                    it.next()
                        .ok_or_else(|| "--output 需要一个路径参数".to_string())?,
                );
            }
            other => return Err(format!("未知参数: {other}")),
        }
    }
    let packages = collect_packages()?;
    let generated = render(&packages);
    if check {
        let current = std::fs::read_to_string(&output).map_err(|e| {
            format!(
                "读取 {} 失败: {e}（先运行 `cargo xtask notices`）",
                output.display()
            )
        })?;
        if current == generated.text {
            println!(
                "notices 已是最新（{} 个第三方包 / {} 份许可文本）",
                generated.packages, generated.license_texts
            );
            return Ok(());
        }
        return Err(format!(
            "{} 与当前依赖图不一致——请重跑 `cargo xtask notices` 并提交结果",
            output.display()
        ));
    }
    std::fs::write(&output, &generated.text)
        .map_err(|e| format!("写入 {} 失败: {e}", output.display()))?;
    println!(
        "已生成 {}（{} 个第三方包 / {} 份许可文本）",
        output.display(),
        generated.packages,
        generated.license_texts
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str, license: Option<&str>) -> Pkg {
        Pkg {
            name: name.into(),
            version: version.into(),
            license: license.map(str::to_string),
            url: Some(format!("https://example.com/{name}")),
            root: PathBuf::from("/nonexistent"),
        }
    }

    #[test]
    fn parse_metadata_excludes_workspace_members_and_sorts() {
        let json = r#"{
          "workspace_members": ["agent 0.1.0 (path+file:///w/crates/agent)"],
          "packages": [
            {"id":"agent 0.1.0 (path+file:///w/crates/agent)","name":"agent","version":"0.1.0",
             "license":"MIT","manifest_path":"/w/crates/agent/Cargo.toml"},
            {"id":"serde 1.0.0 (registry+https://x)","name":"serde","version":"1.0.0",
             "license":"MIT OR Apache-2.0","repository":"https://github.com/serde-rs/serde",
             "manifest_path":"/r/serde-1.0.0/Cargo.toml"},
            {"id":"aaa 0.2.0 (registry+https://x)","name":"aaa","version":"0.2.0",
             "manifest_path":"/r/aaa-0.2.0/Cargo.toml"}
          ]
        }"#;
        let pkgs = parse_metadata(json, Some(Path::new("/w/crates"))).unwrap();
        // 自有 crate 被排除；其余按 (name, version) 排序。
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "aaa");
        assert_eq!(pkgs[0].license, None);
        assert_eq!(pkgs[1].name, "serde");
        assert_eq!(pkgs[1].version, "1.0.0");
        assert_eq!(
            pkgs[1].url.as_deref(),
            Some("https://github.com/serde-rs/serde")
        );
        assert_eq!(pkgs[1].root, Path::new("/r/serde-1.0.0"));
    }

    #[test]
    fn parse_metadata_rejects_missing_sections() {
        assert!(parse_metadata("{}", None).is_err());
        assert!(parse_metadata("not json", None).is_err());
    }

    #[test]
    fn parse_metadata_keeps_vendor_forks_that_are_workspace_members() {
        // vendor/ 下的 path fork 会因 Cargo 的自动入册成为 workspace 成员，
        // 但它们不是自有 crate（不在 crates/ 下）→ 必须保留。
        let json = r#"{
          "workspace_members": [
            "agent 0.1.0 (path+file:///w/crates/agent)",
            "pi-shell 0.1.0 (path+file:///w/vendor/pi-shell)"
          ],
          "packages": [
            {"id":"agent 0.1.0 (path+file:///w/crates/agent)","name":"agent","version":"0.1.0",
             "manifest_path":"/w/crates/agent/Cargo.toml"},
            {"id":"pi-shell 0.1.0 (path+file:///w/vendor/pi-shell)","name":"pi-shell",
             "version":"0.1.0","license":"MIT","manifest_path":"/w/vendor/pi-shell/Cargo.toml"}
          ]
        }"#;
        let pkgs = parse_metadata(json, Some(Path::new("/w/crates"))).unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "pi-shell");
        assert_eq!(pkgs[0].license.as_deref(), Some("MIT"));
    }

    #[test]
    fn render_lists_packages_by_license_and_undeclared_group() {
        let pkgs = vec![
            pkg("aaa", "0.1.0", Some("MIT")),
            pkg("bbb", "0.2.0", Some("MIT")),
            pkg("ccc", "0.3.0", None),
        ];
        let g = render(&pkgs);
        assert_eq!(g.packages, 3);
        assert!(g.text.contains("### MIT (2 packages)"), "{}", g.text);
        assert!(g.text.contains("- aaa 0.1.0"));
        assert!(g.text.contains("### (未声明许可证) (1 packages)"));
        assert!(g.text.contains("- ccc 0.3.0"));
        assert!(g.text.contains("共收录 3 个第三方包"), "应含统计行");
        // 未声明许可证不影响渲染成功（通知文件仍需列出）。
        assert!(g.text.starts_with("THIRD-PARTY-NOTICES"));
    }
}
