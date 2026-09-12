//! `xtask`：工作区自动化入口（H35 发布编排）。
//!
//! 子命令：
//! - `notices [--check] [--output <path>]`：从 `cargo metadata` 生成第三方许可证通知
//!   （`THIRD-PARTY-NOTICES.txt`）；`--check` 只比对不写，漂移即非零退出（CI 门禁）。
//! - `release-check`：发布前预检（版本一致性 / CHANGELOG `[Unreleased]` / 三语 README /
//!   NOTICES 与当前依赖图一致）。
//!
//! 与 `about.toml` 的分工：`about.toml` + `cargo about generate` 仍是**发布资产**的
//! 权威生成器（含完整许可文本、accepted 分支解析）；`xtask notices` 提供**离线、无额外
//! 工具链依赖**的通知文件与漂移门禁，保证 Cargo.lock 变更后不会被遗忘。

#![deny(unsafe_code)]

mod docs;
mod notices;
mod release;

use std::process::ExitCode;

const USAGE: &str = "\
用法: cargo xtask <命令> [选项]

命令:
  notices [--check] [--output <path>]   生成/校验 THIRD-PARTY-NOTICES.txt
  docs --tools [--check]                生成/校验 docs/tools.md（工具参考）
  release-check                         发布前预检（版本/CHANGELOG/README/NOTICES/工具文档）
  help                                  显示本帮助";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    match cmd {
        "notices" => match notices::run(rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask notices 失败: {e}");
                ExitCode::FAILURE
            }
        },
        "docs" => match docs::run(rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask docs 失败: {e}");
                ExitCode::FAILURE
            }
        },
        "release-check" => match release::run(rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask release-check 失败: {e}");
                ExitCode::FAILURE
            }
        },
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("未知命令: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
