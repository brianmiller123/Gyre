//! 结构化记忆的 LLM 沉淀（working → episodic 提炼）。
//!
//! 移植 oh-my-pi mnemopi `consolidateToEpisodic` 语义的轻量版：把**未沉淀**
//! （`consolidated_at == None`）的工作记忆记录批量交给 LLM，提炼为一条精炼的
//! 长期（episodic）记录（`source = "consolidated"`、`metadata.tier = "episodic"`、
//! 重要性取批内最大值、标签取并集），然后把原记录标记 `consolidated_at`，
//! 避免下一轮重复吸收。
//!
//! 与 [`crate::store::LocalMemoryStore::consolidate`]（raw notes → MEMORY.md 全文合并）
//! 的差异：本模块面向结构化记录，输出是「提炼后的情节」，原记录**保留可召回**
//! （不删除、不改 `valid_until`），只是不再参与后续沉淀——召回面只增不减。

use crate::structured::MemoryRecord;

/// 沉淀报告（观测用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsolidateReport {
    /// 处理的 bank 数。
    pub banks: usize,
    /// 被吸收（标记 consolidated_at）的记录数。
    pub absorbed: usize,
    /// 生成的提炼记录数。
    pub distilled: usize,
    /// 本轮因**租约被他人持有**而跳过（多会话并发保护；未做任何 LLM 调用）。
    pub skipped: bool,
}

/// 单批最多吸收的记录数（超出的留在下轮）。
pub const CONSOLIDATE_BATCH_MAX: usize = 50;
/// 单批输入内容总字符预算（超预算从旧到新截断，防提示词膨胀）。
pub const CONSOLIDATE_CHARS_BUDGET: usize = 8000;
/// 至少这么多条未沉淀记录才触发 LLM 沉淀（1 条不值得一次 LLM 调用）。
pub const CONSOLIDATE_MIN_RECORDS: usize = 2;

/// 沉淀 LLM 的 system 指令（与 [`crate::store`] 的合并助手同风格）。
pub const CONSOLIDATION_SYSTEM: &str = "你是长期记忆沉淀助手。把工作记忆批量提炼为精炼的长期记忆条目：\
保留技术决策、约束、惯例、已踩坑与用户偏好；去除重复与细节噪音；\
输出紧凑的 Markdown（可含多条要点），不要解释过程。";

/// 构造沉淀提示词（纯函数，便于测试）：按时间升序列出未沉淀记录。
#[must_use]
pub fn structured_consolidation_prompt(records: &[MemoryRecord]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(records.len());
    for (i, r) in records.iter().enumerate() {
        lines.push(format!(
            "{}. [重要性 {}] {}",
            i + 1,
            r.importance,
            r.content.replace('\n', " ")
        ));
    }
    format!(
        "把以下 {} 条原始记忆提炼为精炼的长期记忆（保留技术决策、约束、惯例、已踩坑；\
         去除重复与细节噪音；可合并为一条或多条要点，用 Markdown 列表输出）：\n\n{}",
        records.len(),
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn rec(content: &str, importance: u8) -> MemoryRecord {
        MemoryRecord {
            id: String::new(),
            content: content.into(),
            source: "test".into(),
            importance,
            scope: "project".into(),
            metadata: BTreeMap::new(),
            tags: vec![],
            ts: 1,
            valid_until: None,
            consolidated_at: None,
        }
    }

    #[test]
    fn prompt_lists_records_with_importance() {
        let prompt = structured_consolidation_prompt(&[
            rec("用户偏好 Bun", 3),
            rec("依赖升级走 minor 先", 5),
        ]);
        assert!(prompt.contains("2 条原始记忆"));
        assert!(prompt.contains("1. [重要性 3] 用户偏好 Bun"));
        assert!(prompt.contains("2. [重要性 5] 依赖升级走 minor 先"));
    }

    #[test]
    fn prompt_handles_empty_batch() {
        let prompt = structured_consolidation_prompt(&[]);
        assert!(prompt.contains("0 条原始记忆"));
        assert!(prompt.trim_end().ends_with('：'));
    }
}
