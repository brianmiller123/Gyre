//! 发布前预检（H35）：版本一致性、CHANGELOG、三语 README、NOTICES 新鲜度。
//!
//! 每一步都是可独立定位的断言（失败即打印具体缺什么），供 `cargo xtask release-check`
//! 与 CI 的发布 job 复用。**不修改任何文件**（除 `notices` 自身的生成命令）。

use std::path::Path;

use crate::notices;

/// `release-check` 子命令。
///
/// # Errors
/// 任一预检项失败（消息里含具体路径与原因）。
pub fn run(_args: &[String]) -> Result<(), String> {
    let root = notices::workspace_root();
    let mut failures: Vec<String> = Vec::new();

    // 1) 版本一致性：workspace.package.version == Cargo.lock 里 `agent` 包的版本。
    match check_versions(&root) {
        Ok(v) => println!("版本一致: {v}"),
        Err(e) => failures.push(e),
    }

    // 2) CHANGELOG 必须存在且含 `## [Unreleased]` 段（发布脚本据此收尾）。
    match check_changelog(&root) {
        Ok(()) => println!("CHANGELOG 含 [Unreleased] 段"),
        Err(e) => failures.push(e),
    }

    // 3) 三语 README 齐备（README.md / README_ZH.md / README_RU.md）。
    match check_readmes(&root) {
        Ok(()) => println!("三语 README 齐备"),
        Err(e) => failures.push(e),
    }

    // 4) NOTICES 与当前依赖图一致（漂移则提示重跑生成命令）。
    let packages = notices::collect_packages()?;
    let generated = notices::render(&packages);
    let path = notices::default_output();
    match std::fs::read_to_string(&path) {
        Ok(current) if current == generated.text => println!(
            "NOTICES 与依赖图一致（{} 个第三方包 / {} 份许可文本）",
            generated.packages, generated.license_texts
        ),
        Ok(_) => failures.push(format!(
            "{} 与当前依赖图不一致——重跑 `cargo xtask notices` 并提交",
            path.display()
        )),
        Err(e) => failures.push(format!(
            "读取 {} 失败: {e}（先运行 `cargo xtask notices`）",
            path.display()
        )),
    }

    // 5) H38 工具参考文档与当前工具面一致（与 notices 同款漂移门禁）。
    let tools = crate::docs::collect_tools();
    let rendered = crate::docs::render(&tools);
    let docs_path = crate::docs::default_output();
    match std::fs::read_to_string(&docs_path) {
        Ok(current) if current == rendered => {
            println!("docs/tools.md 与工具面一致（{} 个工具）", tools.len());
        }
        Ok(_) => failures.push(format!(
            "{} 与当前工具面不一致——重跑 `cargo xtask docs --tools` 并提交",
            docs_path.display()
        )),
        Err(e) => failures.push(format!(
            "读取 {} 失败: {e}（先运行 `cargo xtask docs --tools`）",
            docs_path.display()
        )),
    }

    if failures.is_empty() {
        println!("release-check 通过");
        return Ok(());
    }
    Err(failures.join("\n  - "))
}

/// 版本一致性：`Cargo.toml` 的 `workspace.package.version` 与 `Cargo.lock` 的 `agent`。
fn check_versions(root: &Path) -> Result<String, String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|e| format!("读取 Cargo.toml 失败: {e}"))?;
    let manifest: toml::Value =
        toml::from_str(&manifest).map_err(|e| format!("Cargo.toml 解析失败: {e}"))?;
    let ws_version = manifest
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .ok_or("Cargo.toml 缺少 workspace.package.version")?
        .to_string();

    let lock = std::fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|e| format!("读取 Cargo.lock 失败: {e}"))?;
    let lock: toml::Value =
        toml::from_str(&lock).map_err(|e| format!("Cargo.lock 解析失败: {e}"))?;
    let lock_version = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .and_then(|pkgs| {
            pkgs.iter()
                .find(|p| p.get("name").and_then(toml::Value::as_str) == Some("agent"))
        })
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .ok_or("Cargo.lock 中未找到 `agent` 包")?;
    if ws_version != lock_version {
        return Err(format!(
            "版本漂移：Cargo.toml workspace={ws_version}，Cargo.lock agent={lock_version}"
        ));
    }
    Ok(ws_version)
}

/// CHANGELOG 必须含 `## [Unreleased]` 段。
fn check_changelog(root: &Path) -> Result<(), String> {
    let path = root.join("CHANGELOG.md");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    if !text.contains("## [Unreleased]") {
        return Err(format!("{} 缺少 `## [Unreleased]` 段", path.display()));
    }
    Ok(())
}

/// 三语 README 齐备。
fn check_readmes(root: &Path) -> Result<(), String> {
    let mut missing = Vec::new();
    for name in ["README.md", "README_ZH.md", "README_RU.md"] {
        if !root.join(name).is_file() {
            missing.push(name);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("缺少 README: {}", missing.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_check_reads_real_workspace_and_agrees() {
        // 仓库自身必须通过版本一致性（否则发布链第一步就断了）。
        let root = notices::workspace_root();
        let v = check_versions(&root).expect("workspace 版本应一致");
        assert!(!v.is_empty());
        check_changelog(&root).expect("CHANGELOG 应含 [Unreleased]");
        check_readmes(&root).expect("三语 README 应齐备");
    }

    #[test]
    fn missing_entries_are_reported_per_item() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_versions(dir.path()).unwrap_err();
        assert!(err.contains("Cargo.toml"), "{err}");
        let err = check_changelog(dir.path()).unwrap_err();
        assert!(err.contains("CHANGELOG.md"), "{err}");
        let err = check_readmes(dir.path()).unwrap_err();
        assert!(err.contains("README.md"), "{err}");
    }
}
