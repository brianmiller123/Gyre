//! Hashline 纯数据类型：跨 parser/apply/preview 共享，不引用文件系统或 schema 库。
//!
//! 移植自 [`oh-my-pi hashline/types.ts`](../../../third/oh-my-pi/packages/hashline/src/types.ts)。

use std::collections::BTreeMap;

/// 行号锚点（1-indexed）。
pub type Anchor = u32;

/// 插入位置游标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    /// 文件开头（`INS.HEAD:`）。
    Bof,
    /// 文件末尾（`INS.TAIL:`）。
    Eof,
    /// 在某行之前（`INS.PRE N:`）。
    BeforeAnchor(Anchor),
    /// 在某行之后（`INS.POST N:`）。
    AfterAnchor(Anchor),
}

/// 文件级操作（`REM` / `MV DEST`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    /// 删除整个文件（`REM`）。
    Remove,
    /// 移动/重命名（`MV dest`）。
    Move {
        /// 目标路径。
        dest: String,
    },
}

/// 单个解析出的 hunk（行锚定操作）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    /// 行替换（`SWAP start.=end:` + `+` 正文）。
    Replace {
        /// 起始行（含）。
        start: Anchor,
        /// 结束行（含）。
        end: Anchor,
        /// 新内容（每行一条，已去掉 `+` 前缀）。
        body: Vec<String>,
    },
    /// 行删除（`DEL start.=end` / `DEL N`）。
    Delete {
        /// 起始行（含）。
        start: Anchor,
        /// 结束行（含）。
        end: Anchor,
    },
    /// 插入（`INS.PRE/POST/HEAD/TAIL N:` + `+` 正文）。
    Insert {
        /// 插入游标。
        cursor: Cursor,
        /// 新内容。
        body: Vec<String>,
    },
    /// 文件级操作（`REM` / `MV dest`）。
    File(FileOp),
}

/// 一个文件的 hashline 区段：`[path#hash]` + 一组 hunk。
#[derive(Debug, Clone)]
pub struct FileSection {
    /// 文件路径（相对工作区）。
    pub path: String,
    /// 内容指纹标签（4 hex）；`None` 表示省略。
    pub hash: Option<String>,
    /// 区段内的 hunk 列表（按出现顺序）。
    pub hunks: Vec<Hunk>,
}

/// 应用一组 hunk 到文本的结果。
#[derive(Debug, Clone, Default)]
pub struct ApplyResult {
    /// 编辑后正文；`None` 表示文件被 `REM` 删除。
    pub text: Option<String>,
    /// 诊断告警（hash 不匹配、被丢弃的 after 锚等）。
    pub warnings: Vec<String>,
    /// 首个变更行（1-indexed）；无操作时为 `None`。
    pub first_changed_line: Option<Anchor>,
    /// 目标移动路径（`MV`）。
    pub moved_to: Option<String>,
}

/// 一次 hashline patch 应用到多文件的结果汇总。
#[derive(Debug, Clone, Default)]
pub struct PatchReport {
    /// 逐文件结果：path → ApplyResult。
    pub files: BTreeMap<String, ApplyResult>,
    /// 跨文件告警。
    pub warnings: Vec<String>,
}
