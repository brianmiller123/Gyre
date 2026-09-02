# understand 黄金回归样本

阶段 0（Understand-Anything 数据管道桥）产出的**确定性基线样本**，供阶段 1+
（Rust 原生重构 `understand-scan` / `understand-parse`）做行为一致性回归对比。

> 架构背景：Understand-Anything 集成架构 §6.3 / §8 阶段 1–7（内部设计文档，未随仓库分发）。

---

## 样本清单

| 文件 | 产出脚本 | 内容 | 回归断言点 |
|---|---|---|---|
| `gyre-scan-result.json` | `scan-project.mjs` | Gyre 仓库文件清单：211 文件、`estimatedComplexity=large` | `contentDigest`、`files[].{path,language,fileCategory,sizeLines}` 集合、`stats` |
| `gyre-import-map.json` | `extract-import-map.mjs` | 导入图：283 边、77 文件有导入 | `importMap` 键集合、每文件解析的内部导入路径集合 |

样本取自 Gyre 仓库自身（commit 见 `contentDigest` 关联），`--exclude "third/*,web/c5-ui/node_modules/*,target/*"`。

---

## 重新生成（验证脚本可达性 / 刷新基线）

```bash
UA_SKILL=third/Understand-Anything/understand-anything-plugin/skills/understand
# 前提：已 pnpm --filter @understand-anything/core build
node "$UA_SKILL/scan-project.mjs" "$(git rev-parse --show-toplevel)" \
  tests/understand/fixtures/gyre-scan-result.json \
  --exclude "third/*,web/c5-ui/node_modules/*,target/*"

node -e "const fs=require('fs');const s=require('./tests/understand/fixtures/gyre-scan-result.json');fs.writeFileSync('tests/understand/fixtures/gyre-imp-input.json',JSON.stringify({projectRoot:process.cwd(),files:s.files}));"
node "$UA_SKILL/extract-import-map.mjs" \
  tests/understand/fixtures/gyre-imp-input.json \
  tests/understand/fixtures/gyre-import-map.json
rm -f tests/understand/fixtures/gyre-imp-input.json
```

---

## 回归对比（阶段 1+ Rust 实现）

当 `understand-scan` / `understand-parse` Rust crate 实现完成，对**同一 Gyre 仓库**（同 commit）跑 Rust 实现，
产出 `rust-scan-result.json` / `rust-import-map.json`，断言**结构等价**：

```rust
// 伪代码：理解-anything 回归测试
// 1. 文件集合等价：path 集合相等
// 2. 每文件 {language, fileCategory} 相等（sizeLines 允许 ±0，因计数口径可能微调）
// 3. contentDigest 相等（若 Rust 用同哈希算法）
// 4. importMap：每文件的解析内部导入路径集合相等（顺序无关）
```

允许的预期差异（须文档化）：
- `contentDigest`：若 Rust 改用不同规范化（如行尾归一），digest 会变——此时改断言"文件集合 + 内容规范化后等价"。
- 行计数边界：UA 手写字节级 `\n` 计数；Rust 若用 `bytecount`，须确认末行无换行等边界一致。
- `.understandignore` 语义：Rust 用 `ignore` crate（ripgrep 超集）可能比 npm `ignore` 多/少过滤——须对齐 `filteredByIgnore`。

---

## 扩展样本

每新增一个目标语言/项目类型，追加一组样本（命名 `<project>-<artifact>.json`），
覆盖阶段 1+ 提取器移植的回归。建议至少：
- 纯 Rust 项目（本 Gyre）✅ 已有
- 多语言 monorepo（TS + Python + Go 混合）
- 含大量非代码文件的项目（config/docs/infra 占比高）
