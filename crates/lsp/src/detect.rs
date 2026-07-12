//! 语言服务器自动检测：根据项目根目录文件推断应启动的 LSP 服务器。
//!
//! 支持检测：Rust (rust-analyzer)、TypeScript/JavaScript (typescript-language-server / deno)、
//! Go (gopls)、Python (pyright / pylsp)、C# (omnisharp)、Java (jdtls)、Zig (zls)。

use std::path::Path;

/// 语言服务器描述信息。
#[derive(Debug, Clone)]
pub struct LspServerInfo {
    /// 人类可读名称。
    pub name: String,
    /// 可执行文件命令。
    pub command: String,
    /// 命令行参数。
    pub args: Vec<String>,
    /// 支持的语言 ID 列表（LSP 标准语言标识符）。
    pub languages: Vec<String>,
}

/// 检测项目根目录下所有应启动的语言服务器。
///
/// 遍历已知配置文件，为每个匹配的语言返回对应的服务器信息。
/// 注意：不检查服务器可执行文件是否存在——调用方应在 spawn 失败时处理。
///
/// # 检测规则
///
/// | 配置文件 | 语言 | 服务器 |
/// |----------|------|--------|
/// | `Cargo.toml` | Rust | rust-analyzer |
/// | `package.json` | TypeScript/JavaScript | ts-ls / deno |
/// | `go.mod` | Go | gopls |
/// | `pyproject.toml` / `setup.py` / `requirements.txt` | Python | pyright / pylsp |
/// | `.csproj` / `.sln` | C# | omnisharp |
/// | `pom.xml` / `build.gradle` | Java | jdtls |
/// | `build.zig` | Zig | zls |
#[must_use]
pub fn detect_servers(root: &Path) -> Vec<LspServerInfo> {
    let mut servers = Vec::new();

    // ── Rust: Cargo.toml ──────────────────────────────────────────────
    if root.join("Cargo.toml").exists() {
        servers.push(LspServerInfo {
            name: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
            args: vec![],
            languages: vec!["rust".into()],
        });
    }

    // ── TypeScript/JavaScript: package.json ──────────────────────────
    if root.join("package.json").exists() {
        // deno 项目优先用 deno lsp
        if root.join("deno.json").exists() || root.join("deno.jsonc").exists() {
            servers.push(LspServerInfo {
                name: "deno".into(),
                command: "deno".into(),
                args: vec!["lsp".into()],
                languages: vec![
                    "typescript".into(),
                    "javascript".into(),
                    "typescriptreact".into(),
                    "javascriptreact".into(),
                ],
            });
        } else {
            servers.push(LspServerInfo {
                name: "typescript-language-server".into(),
                command: "typescript-language-server".into(),
                args: vec!["--stdio".into()],
                languages: vec![
                    "typescript".into(),
                    "javascript".into(),
                    "typescriptreact".into(),
                    "javascriptreact".into(),
                ],
            });
        }
    }

    // ── Go: go.mod ───────────────────────────────────────────────────
    if root.join("go.mod").exists() {
        servers.push(LspServerInfo {
            name: "gopls".into(),
            command: "gopls".into(),
            args: vec![],
            languages: vec!["go".into(), "gomod".into()],
        });
    }

    // ── Python: pyproject.toml / setup.py / requirements.txt ─────────
    let is_python = root.join("pyproject.toml").exists()
        || root.join("setup.py").exists()
        || root.join("setup.cfg").exists()
        || root.join("requirements.txt").exists()
        || root.join("Pipfile").exists();

    if is_python {
        // 优先 pyright（更快、更准确），回退 pylsp
        servers.push(LspServerInfo {
            name: "pyright".into(),
            command: "pyright-langserver".into(),
            args: vec!["--stdio".into()],
            languages: vec!["python".into()],
        });
    }

    // ── C#: .csproj / .sln ──────────────────────────────────────────
    if let Ok(entries) = std::fs::read_dir(root) {
        let has_dotnet = entries.filter_map(|e| e.ok()).any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".csproj") || name.ends_with(".sln")
        });
        if has_dotnet {
            servers.push(LspServerInfo {
                name: "omnisharp".into(),
                command: "omnisharp".into(),
                args: vec!["--languageserver".into()],
                languages: vec!["csharp".into()],
            });
        }
    }

    // ── Java: pom.xml / build.gradle ─────────────────────────────────
    let is_java = root.join("pom.xml").exists()
        || root.join("build.gradle").exists()
        || root.join("build.gradle.kts").exists();

    if is_java {
        servers.push(LspServerInfo {
            name: "jdtls".into(),
            command: "jdtls".into(),
            args: vec![],
            languages: vec!["java".into()],
        });
    }

    // ── Zig: build.zig ──────────────────────────────────────────────
    if root.join("build.zig").exists() {
        servers.push(LspServerInfo {
            name: "zls".into(),
            command: "zls".into(),
            args: vec![],
            languages: vec!["zig".into()],
        });
    }

    servers
}

/// 根据文件路径扩展名推断 LSP 语言 ID。
#[must_use]
pub fn language_id_from_path(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust"),
        Some("ts") => Some("typescript"),
        Some("tsx") => Some("typescriptreact"),
        Some("js") => Some("javascript"),
        Some("jsx") => Some("javascriptreact"),
        Some("go") => Some("go"),
        Some("py") | Some("pyi") => Some("python"),
        Some("cs") => Some("csharp"),
        Some("java") => Some("java"),
        Some("zig") => Some("zig"),
        Some("c") | Some("h") => Some("c"),
        Some("cpp") | Some("cc") | Some("cxx") | Some("c++") => Some("cpp"),
        Some("hpp") | Some("hh") | Some("hxx") => Some("cpp"),
        Some("toml") => Some("toml"),
        Some("json") => Some("json"),
        Some("yaml") | Some("yml") => Some("yaml"),
        Some("md") | Some("mdx") => Some("markdown"),
        Some("css") => Some("css"),
        Some("scss") | Some("sass") => Some("scss"),
        Some("html") | Some("htm") => Some("html"),
        Some("sql") => Some("sql"),
        Some("sh") | Some("bash") | Some("zsh") => Some("shellscript"),
        Some("dockerfile") => Some("dockerfile"),
        Some("vue") => Some("vue"),
        Some("svelte") => Some("svelte"),
        _ => None,
    }
}

/// 根据文件路径从服务器列表中查找对应的 LSP 服务器。
#[must_use]
pub fn find_server_for_file<'a>(
    servers: &'a [LspServerInfo],
    file_path: &Path,
) -> Option<&'a LspServerInfo> {
    let lang_id = language_id_from_path(file_path)?;
    servers.iter().find(|s| s.languages.iter().any(|l| l == lang_id))
}
