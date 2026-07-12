//! 验证 skill 发现：扫描当前 cwd 的 skill，打印并演示 skill:// 解析。
//! 用法（在项目根）：cargo run -p agent-skills --example verify_skills

use agent_core::{SkillLoadOptions, SkillResolver};
use agent_skills::SkillRegistry;

#[tokio::main]
async fn main() {
    let cwd = std::env::current_dir().expect("无法获取 cwd");
    let opts = SkillLoadOptions::default();
    let cat = SkillRegistry::native(cwd.clone()).load(&opts).await.expect("加载失败");

    println!("cwd: {}", cwd.display());
    if cat.is_empty() {
        println!("未发现任何 skill");
        return;
    }
    println!("发现 {} 个 skill:", cat.skills.len());
    for s in &cat.skills {
        println!("  - {} ({}): {}", s.name, s.source.provider, s.description);
        println!("      file: {}", s.file_path.display());
    }
    // 演示 skill:// 解析
    if let Some(first) = cat.skills.first() {
        let url = format!("skill://{}", first.name);
        match SkillResolver::resolve(&cat, &url) {
            Ok(p) => println!("\nresolve {url} -> {}", p.display()),
            Err(e) => println!("\nresolve {url} 失败: {e}"),
        }
    }
    if !cat.warnings.is_empty() {
        eprintln!("告警: {}", cat.warnings.join("; "));
    }
}
