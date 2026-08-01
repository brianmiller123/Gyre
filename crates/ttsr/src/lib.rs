//! # agent-ttsr
//!
//! 时间旅行流规则（Time-Traveling Stream Rules）：规则不写进 system prompt（零上下文
//! 成本），而是对模型**流式输出**实时匹配——正则匹配文本/思考增量、ast-grep 匹配
//! 写/编辑载荷。命中即中断流，注入规则为隐藏消息后从同一轮重试（"时间旅行"）。
//!
//! 移植自 oh-my-pi `export/ttsr.ts` + `session/ttsr-coordinator.ts`（设计详见
//! `docs/oh-my-pi-feature-analysis.md` §1）。与 oh-my-pi 的关键对齐：
//! - 规则文件为 Markdown（frontmatter + 正文），目录 `<cwd>/.gyre/rules/*.md`；
//! - 注入抑制状态以 `[ttsr-injection:…]` 标记消息持久化，压缩/恢复后不重复注入；
//! - 文本/思考作用域 v1 仅支持中断（`interruptMode: always`）；工具作用域同时支持
//!   `always`（丢弃重试）与 `never`（折叠进工具结果提醒）；
//! - Rust `regex` crate 线性时间匹配，无 JS 版 `ReDoS` 风险。
//!
//! ```toml
//! # .gyre/rules/no-box-leak.md
//! ---
//! name: no-box-leak
//! condition: ["(?i)Box::leak"]
//! scope: [text, tool:write_file]
//! ---
//! 禁止在生产代码路径使用 `Box::leak`；请改用 `Arc<str>`。
//! ```

pub mod coordinator;
pub mod frontmatter;
pub mod matcher;
pub mod rule;

pub use coordinator::{parse_marker, ToolOutcome, TtsrConfig, TtsrCoordinator, INJECTION_MARKER};
pub use matcher::{digest_for, path_of, tool_calls_of, TtsrManager};
pub use rule::{discover_rules, parse_rule, InterruptMode, Repeat, Rule, RuleScope};
