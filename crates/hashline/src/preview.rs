//! Hashline 紧凑差异预览：给定编辑前后文本，产出 `+/-` 行预览与增删计数。
//!
//! 移植自 [`oh-my-pi hashline/diff-preview.ts`](../../../third/oh-my-pi/packages/hashline/src/diff-preview.ts)（精简 LCS）。

/// 紧凑预览结果。
#[derive(Debug, Clone, Default)]
pub struct CompactDiffPreview {
    /// 预览文本（`+`/`-`/` ` 前缀行）。
    pub preview: String,
    /// 新增行数。
    pub added_lines: usize,
    /// 删除行数。
    pub removed_lines: usize,
}

/// 预览最多展示的行数上限。
const MAX_PREVIEW_LINES: usize = 200;

/// 构建紧凑差异预览。
#[must_use]
pub fn build_compact_diff(old: &str, new: &str) -> CompactDiffPreview {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = lcs_diff(&a, &b);

    let mut preview = String::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut shown = 0usize;

    for op in ops {
        if shown >= MAX_PREVIEW_LINES {
            preview.push_str("…（预览已截断）\n");
            break;
        }
        match op {
            DiffOp::Equal(line) => {
                push_line(&mut preview, ' ', line);
                shown += 1;
            }
            DiffOp::Delete(line) => {
                push_line(&mut preview, '-', line);
                removed += 1;
                shown += 1;
            }
            DiffOp::Insert(line) => {
                push_line(&mut preview, '+', line);
                added += 1;
                shown += 1;
            }
        }
    }

    CompactDiffPreview {
        preview,
        added_lines: added,
        removed_lines: removed,
    }
}

fn push_line(out: &mut String, prefix: char, line: &str) {
    out.push(prefix);
    out.push_str(line);
    out.push('\n');
}

enum DiffOp<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

/// 基于标准 LCS 的行差异（O(n·m)，适用于常规源文件规模）。
fn lcs_diff<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<DiffOp<'a>> {
    let n = a.len();
    let m = b.len();
    // dp[i][j] = a[..i] 与 b[..j] 的 LCS 长度
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1] + 1
            } else {
                dp[i - 1][j].max(dp[i][j - 1])
            };
        }
    }

    // 回溯
    let mut ops = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            ops.push(DiffOp::Equal(a[i - 1]));
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            ops.push(DiffOp::Delete(a[i - 1]));
            i -= 1;
        } else {
            ops.push(DiffOp::Insert(b[j - 1]));
            j -= 1;
        }
    }
    while i > 0 {
        ops.push(DiffOp::Delete(a[i - 1]));
        i -= 1;
    }
    while j > 0 {
        ops.push(DiffOp::Insert(b[j - 1]));
        j -= 1;
    }
    ops.reverse();
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_add_remove() {
        let d = build_compact_diff("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(d.added_lines, 2);
        assert_eq!(d.removed_lines, 1);
        assert!(d.preview.contains("-b"));
        assert!(d.preview.contains("+B"));
        assert!(d.preview.contains("+d"));
    }
}
