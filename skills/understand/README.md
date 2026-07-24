# understand skill（阶段 0 · Understand-Anything 数据管道桥）

把任意代码库转化为交互式知识图谱（`.ua/knowledge-graph.json`）。本 skill 是
[Understand-Anything](https://github.com/Egonex-AI/Understand-Anything) 多阶段流水线在
Gyre 上的桥接编排：确定性计算复用 UA 现成脚本，LLM 语义阶段用 Gyre `task` 子代理。

完整架构与多路径评估见 [`plans/understand-anything-integration-architecture.md`](../../plans/understand-anything-integration-architecture.md)。

---

## 1. 前置依赖

| 依赖 | 版本 | 用途 |
|---|---|---|
| Node.js | ≥ 22 | 运行 UA 确定性脚本（scan / batch / structure / fingerprint） |
| pnpm | ≥ 10 | 构建 `@understand-anything/core`（`corepack` 启用） |
| Python | ≥ 3.8 | `merge-batch-graphs.py`（图合并归一化） |
| git | 任意 | 取 commit hash、增量 `git diff` |

### 首次构建 UA 核心库

UA 脚本 import 编译产物 `packages/core/dist/index.js`。首次使用须构建一次：

```bash
cd third/Understand-Anything/understand-anything-plugin
corepack pnpm install
pnpm --filter @understand-anything/core build
```

> 本仓库已 vendored UA 于 `third/Understand-Anything/`，故 `$UA_ROOT` 探测会命中该路径。
> 若改用全局安装（`install.sh`），命中 `~/.understand-anything-plugin`。

---

## 2. 启用 skill

skill 发现机制（见 [`crates/skills/src/native.rs`](../../crates/skills/src/native.rs)）：
非递归扫描 `<dir>/<name>/SKILL.md`。三种启用方式任选其一。

### 方式 A — 项目配置（推荐，仅本项目）

在 `.agent/config.toml`（项目级）的 `[skills]` 段指向本目录：

```toml
[skills]
enabled = true
custom_directories = ["skills"]
```

### 方式 B — 项目级目录

```bash
mkdir -p .agent/skills
ln -s "$(pwd)/skills/understand" .agent/skills/understand
```

（Gyre 自 cwd 向上 walkup `.agent/skills`。）

### 方式 C — 用户级（全项目可用）

```bash
ln -s "$(pwd)/skills/understand" ~/.config/agent/skills/understand
```

---

## 3. 命令审批规则（放行 UA 脚本调用）

`run_command` 默认按 `[agent] approval_mode` 与 `[agent.tools.approval]` 治理。
UA 流水线需频繁调用 `node` / `python` / `git` / `mkdir`。建议在 `.agent/config.toml`
加入 allow 规则（glob），避免逐次确认：

```toml
[agent.commands]
# allow > deny > ask

[[agent.commands.allow]]
pattern = "git *"

[[agent.commands.allow]]
pattern = "node *"

[[agent.commands.allow]]
pattern = "python *"

[[agent.commands.allow]]
pattern = "python3 *"

[[agent.commands.allow]]
pattern = "pnpm *"

[[agent.commands.allow]]
pattern = "mkdir *"

[[agent.commands.allow]]
pattern = "find *"

[[agent.commands.allow]]
pattern = "test *"
```

> 安全提示：`node *` / `python *` 放行较宽。生产环境可收窄为精确脚本路径，例如
> `pattern = "node */skills/understand/*.mjs *"`。`rm -rf *` 仍由默认 deny 规则拦截。

---

## 4. 使用

启动 Gyre（CLI 或 Web），在 code/debug 模式下对要分析的项目要求：

```
用 understand skill 分析当前代码库
```

或限定子目录（大仓场景）：

```
用 understand skill 分析 src/frontend
```

模型会在 `<skills>` 段看到 understand，`read_file skill://understand` 加载完整编排，
随后按 Phase 0–7 执行：确定性阶段 `run_command` 调 UA 脚本，语义阶段 `task` 并行子代理。

产出：`$PROJECT_ROOT/.ua/knowledge-graph.json`（或 `.understand-anything/`）。

---

## 5. 黄金回归样本（阶段 0 验证闭环）

阶段 0 的核心价值之一是**为后续 Rust 原生重构（阶段 1+）固化黄金样本**。流程：

1. 对若干代表性项目（如本仓库 Gyre 自身、`third/Understand-Anything` 自身、一个多语言小项目）各跑一次完整 `/understand`。
2. 把产出的 `.ua/knowledge-graph.json` 复制到 `tests/fixtures/understand/<project>-knowledge-graph.json` 并提交。
3. 记录 `gitCommitHash`（图内 `project.gitCommitHash`）+ 对应 commit 的 `inputDigest`。
4. 阶段 1+ 每移植一个 Rust 模块，用**同输入**断言"JSON 结构等价"（节点 id 集合、边 `(source,target,type)` 集合、layer/tour 结构），作为行为一致性回归基线。

### 快速自检（确定性脚本可达性）

确认 UA 脚本能被 Gyre 调用：

```bash
UA_SKILL=third/Understand-Anything/understand-anything-plugin/skills/understand
node "$UA_SKILL/scan-project.mjs" "$(pwd)" /tmp/ua-scan-test.json && echo "scan OK"
test -f /tmp/ua-scan-test.json && echo "output OK"
python3 "$UA_SKILL/merge-batch-graphs.py" --help 2>/dev/null || python3 -c "import sys; print('python OK')"
```

> 若 `scan-project.mjs` 报找不到 `@understand-anything/core`，回到 §1 执行 `pnpm --filter @understand-anything/core build`。

---

## 6. 已知限制（阶段 0 范围）

- **性能**：继承 UA 的 Node/WASM tree-sitter 与 worker 进程并发瓶颈。大仓（>1000 文件）耗时显著。优化见架构报告 §6（原生重构路线）。
- **运行时依赖**：需 Node + pnpm + Python 三运行时，与 Gyre"单二进制"理念冲突。阶段 8（原生重构完成）后下线。
- **审批交互**：子代理（`task`）内 `run_command` 经 Gyre 审批；首次跑可能多次确认，建议先配好 §3 的 allow 规则。
- **提示注入**：被分析源码内的指令文本可能影响 file-analyzer 子代理的语义输出（结构事实不受影响）。已在 SKILL.md 安全约束段说明"不可信数据"围栏。
