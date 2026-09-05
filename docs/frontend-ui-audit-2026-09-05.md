# Gyre 前端审计与优化方案（2026-09-05）

**范围**：`web/c5-ui/`（Agent·Console，React 18 + TS + Tailwind 3.4 + Vite，约 1 万行，rust-embed 内嵌进 server 二进制）与 `crates/server/src/collab_guest.html`（协同访客单页）。
**方法**：三路并行审查（页面结构盘点 / 设计体系 / 交互·可访问性·性能），关键发现逐一人工复核到 file:line。
**处置**：P0（7 项）与 P1（约 14 项）已于本次全部落地；P2 列入路线图待后续迭代。

---

## 一、总体结论

前端整体工程质量明显高于同类自研控制台的平均水平：语义化 CSS 变量令牌体系完整（light/dark 双主题 + accent 换肤 + 防闪烁）、Modal 有焦点陷阱与焦点归还、WS 断线指数退避重连 + 心跳看门狗、乐观更新、自研 SVG 图表零依赖、`!important` 零使用。因此本审计没有发现"页面结构混乱"级的问题，主要缺口集中在：

1. **确定性 bug**（焦点环丢失、变量名拼写、失败态空白、静默失败、构建残留）；
2. **可访问性短板**（对话流无 live region、菜单/抽屉键盘不可达、低对比度文本）；
3. **性能长尾**（highlight.js 全量打进主包、无代码分割）。

| 优先级 | 数量 | 处置 |
|---|---|---|
| P0 bug/缺陷 | 7 | ✅ 本次修复 |
| P1 体验/可访问性/性能 | 14 | ✅ 本次修复 |
| P2 规范收敛/长尾 | 12 | 📋 路线图 |

**收益量化**：主 bundle 489KB → **376KB**（-23%，语言 chunk 按需加载）；`web/assets` 陈旧构建残留清理（不再有 4 代历史 bundle 被打进 release 二进制，单次内嵌体积 -2.3MB）；键盘用户全站可见焦点；读屏用户可感知新消息/审批/错误。

---

## 二、P0（已修复）：bug 级缺陷

### P0-1 按钮键盘焦点环丢失 🔴
- **位置**：`web/c5-ui/src/components/ui.tsx:91`（Button 基类串 `focus-visible:outline-none`，无替代焦点指示）
- **影响**：全局所有 `<Button>`（27+ 处）键盘 Tab 导航时焦点完全不可见，违反 WCAG 2.1 AA 2.4.7（Focus Visible）；键盘与读屏用户无法定位当前操作目标。
- **修复**：统一替换为 `focus-visible:ring-2 ring-primary/70 ring-offset-2 ring-offset-bg`。
- **最佳实践**：WCAG 2.4.7；任何 `outline-none` 必须伴随可见替代（ring/背景变化）。

### P0-2 BranchTreeModal accent 变量名拼写错误 🔴
- **位置**：`BranchTreeModal.tsx:109` `accent-[var(--primary)]`——令牌实为 `--c-primary`（`index.css:22`）
- **影响**：handoff 复选框的 accent-color 不生效、不跟随主题/accent 换肤。
- **修复**：改为 Tailwind 语义类 `accent-primary`（同时消除了对 CSS 变量名的硬依赖）。

### P0-3 分支树拉取失败呈空白盒 🔴
- **位置**：`BranchTreeModal.tsx:36-51,124-147`；根因 `fetchBranches`（`useAgentSession.tsx`）走 `apiGet` 把一切错误吞成 `null`，与"合法空树"无法区分。
- **影响**：网络/服务端失败时模态框主体只剩一个空边框盒——无错误说明、无重试入口，用户只能关闭重开。
- **修复**：`fetchBranches` 改返回 `{ tree, error }`（不再复用吞错的 apiGet）；模态框新增错误态（图标 + 原因 + 重试按钮），错误/空树/加载三态分离。
- **最佳实践**：错误态必须与空态区分（Nielsen 启发式 #9 错误恢复）；失败要提供重试路径。

### P0-4 运行中斜杠命令静默失败 🔴
- **位置**：`Composer.tsx:206-208` `if (running) return`——无任何提示，输入原样保留。
- **影响**：典型"按了没反应"，用户无法区分"已发送/被拦截/出错了"，挫败感直接来源。
- **修复**：拦截时弹 toast（`composer.slash_running`，4 语言），纯文本 steering 行为保持不变。

### P0-5 切换模型不取消在跑任务 🔴
- **位置**：`useAgentSession.tsx` `switchModel`——直接断开 WS；`ModelSwitcher.tsx:59-90` 确认弹窗无警示。
- **影响**：旧任务在后端继续跑完并**持续消耗 token**，用户以为已停止；与 `newChat`（先发 `cancel`）行为不一致。
- **修复**：`switchModel` 对齐 `newChat` 先发 `{type:'cancel'}`；ModelSwitcher 确认弹窗在 `running` 时显示警示条（`shell.switch_running_warn`）。
- **最佳实践**：破坏性/有代价的切换必须在确认层显式告知后果。

### P0-6 构建残留打进发布二进制 🔴
- **位置**：`vite.config.ts`（`emptyOutDir:false`，因 outDir=`../` 不能直接清空否则会删源码）+ `web/assets/` 实测累积 4 代陈旧 bundle（5 CSS + 4 JS ≈ 2.3MB）；另 `vite.config.js/.d.ts` 为 `tsc -b` 误生成产物被提交入库。
- **影响**：rust-embed `#[folder="../../web"]` 把全部历史产物打进 release 二进制（白增约 2.3MB，且随时间持续增长）。
- **修复**：① 自定义 Vite 插件 `cleanPreviousBuild`（buildStart 时定点清理 `web/index.html` 与 `web/assets/`）；② `tsconfig.node.json` 加 `outDir: node_modules/.tmp` 使 emit 不再落在源码目录，删除误提交文件并补 `.gitignore`；③ 清理存量 9 个陈旧 bundle。
- **最佳实践**：构建产物幂等；嵌入式资产目录必须与构建清理范围严格一致。

### P0-7 i18n 缺 key 与硬编码文案 🟠
- **位置**：ru/ja 各缺 13 个 key（`branches.*` 10 个、`composer.enhance(_error)`、`sessions.branches`）；硬编码三处——`SubAgentMonitor.tsx:143`（中文"日志（n）"）、`mentions.ts:151`（英文警告）、`commands.ts:270`（中文"# 命令参数"）；`SessionList.tsx:402`（aria-label 硬编码中文"更多操作"）。
- **影响**：俄/日用户在分支树、Enhance 等处看到英文；中英之外的读屏用户听到中文标签；注入命令的标题随 UI 语言不一致。
- **修复**：补齐 ru/ja 全部缺失 key；四处硬编码全部改走 `t()`（mentions 的 ExpandCtx 增加 `t` 参数）。

---

## 三、P1（已修复）：体验 / 可访问性 / 性能

### 可访问性组

| # | 问题 | 位置 | 修复 |
|---|---|---|---|
| P1-1 | 对话区无 live region：流式回复/审批卡/错误对读屏完全无感知（WCAG 4.1.3） | `Transcript.tsx:41-50` | 消息容器加 `role="log"` + `aria-live="polite"`（`transcript.log_aria`）；AskCard 与 error 条目加 `role="alert"` 插入即播报 |
| P1-2 | Dropdown 无键盘支持（无方向键/Esc/焦点移入，无 aria-expanded/haspopup） | `ui.tsx` Dropdown、`Sidebar.tsx:139-178` 语言菜单 | Dropdown 重写：ArrowUp/Down 循环导航、Home/End、Esc 关闭并归还焦点、Tab 自动收起、键盘打开自动聚焦首项、`aria-haspopup/expanded`、菜单项 `tabIndex=-1`（WAI-ARIA APG menu 模式）；语言菜单同样补齐 |
| P1-3 | 抽屉/全屏浮层无焦点管理：焦点可在背景游走、无 Esc | `AgentShell.tsx` 移动侧栏/Inspector 抽屉、`StatisticsPanel` | 抽取共享 `useDialogA11y`（焦点移入/归还 + Tab 陷阱 + Esc + 锁滚动，复用 Modal 原逻辑），Modal 同步去重改用该 hook；抽屉与统计页全部接入 `role="dialog"` + `aria-modal` |
| P1-4 | 表单控件无可访问名（仅 placeholder） | `Composer.tsx` 主输入、`SessionList.tsx` 搜索框、`Transcript.tsx` 回复框 | 补 `aria-label`；主输入补全 combobox 模式（`role="combobox"` + `aria-autocomplete="list"`） |
| P1-5 | Markdown 标题渲染为 `<p>`，丢失文档大纲 | `markdown.tsx:233-239` | 改渲染为 `h1`-`h6`，保留原字号样式 |
| P1-6 | 低对比度文本（深色代码底上 white/25-50） | `WorkspacePanel.tsx:659`、`markdown.tsx:346,349` | 提升至 white/45-65（≥4.5:1，WCAG 1.4.3） |

### 交互体验组

| # | 问题 | 位置 | 修复 |
|---|---|---|---|
| P1-7 | Server URL 只在保存时 toast 校验，`Field/Input` 的 error/invalid 能力闲置、必填无标识 | `SettingsPanel.tsx:87-98` | 输入即内联校验（非法 URL 红框 + 错误文案），保存复用同一校验结果；Field 标 required |
| P1-8 | AskCard 回复无在途保护，连点重复 respond | `Transcript.tsx:429-446` | `sent` 状态防重，提交后按钮禁用直至 resolved 回执 |
| P1-9 | 非图片附件被静默丢弃（超 10MiB 有提示，格式不支持没有） | `Composer.tsx:263`、`onPaste` | 统一由 `handleFiles` 拦截并 toast 不支持数量（`composer.attach_unsupported`） |
| P1-10 | 清空对话（3 处）均无二次确认，与删除会话的确认策略不一致 | `AgentShell.tsx` TopBar、`Sidebar.tsx:124`、`SettingsPanel.tsx:294-304` | 新增共享 `ConfirmDialog` 组件，三处统一接入（标题/正文/确认 4 语言） |
| P1-11 | 连接错误横幅：truncate 导致长错误看不全，且无重试入口 | `AgentShell.tsx:93-98` | 新增 `ErrorBanner`：错误可展开/收起 + 一键重连（`connect(sessionId)`） |
| P1-12 | slash 菜单键盘高亮项无滚动跟随，命令多时选中项滚出可视区 | `Composer.tsx:346-370` | 高亮项 `scrollIntoView({block:'nearest'})` |
| P1-13 | SOCKS5 状态拉取失败与"服务端未配置"混同展示 | `useAgentSession.tsx:398-413`、`SettingsPanel.tsx:176-195` | `refreshSocks5Status` 返回成功与否；面板区分"加载中/失败+重试/未配置"三态 |

### 性能与规范组

| # | 问题 | 位置 | 修复 |
|---|---|---|---|
| P1-14 | highlight.js core + 29 种语言全量静态打进主包（主包 489KB），零代码分割 | `highlight.ts` | 语言全部改动态 `import()`（29 个懒加载 chunk），新增 `useHighlightedCode` hook（deferred 合并 + 过期守卫 + chunk 未就绪时纯文本兜底）；markdown CodeBlock 与 WorkspacePanel 文件查看器统一迁移；**主包 489KB → 376KB** |
| P1-15 | 遮罩透明度 4 种（40/45/50/55）、z-index 混用两套写法 9 个值 | Modal/抽屉/统计页/工作区浮窗 | 统一 `.app-backdrop` 组件类（bg-black/55 + blur + fade-in）；tailwind 定义 `zIndex` 分层令牌（dropdown=50/stats=80/drawer=90/workspace=95/modal=100/toast=120）并全量替换 `z-[N]` |
| P1-16 | 图标按钮类串重复（`h-9 w-9`/`h-8 w-8` 各多处）；`.glass`/`.card` 死类 0 使用 | `AgentShell`/`Sidebar`/`StatisticsPanel`/`ui.tsx`/`index.css:127-133` | 新增 `IconButton`（强制 label，自带焦点态）替换 7 处手写；删除两个死类 |

---

## 四、P2 路线图（本次未执行，按性价比排序）

| # | 事项 | 位置/规模 | 建议 | 成本 |
|---|---|---|---|---|
| P2-1 | 任意字号碎片化：`text-[Npx]` 74 处、10 个值（9-15px） | Transcript(22)/WorkspacePanel(14)/markdown(7) 等 | 在 tailwind 扩展 2-3 个命名档位（如 `2xs=10px`/`xs=11px`/`sm=12px`）逐步替换；新代码禁用任意值 | M |
| P2-2 | 消息列表无虚拟化；`useAgentSession`（1330 行）每个 delta 对 items 全量 map 拷贝 | `Transcript.tsx:44-46`、`useAgentSession.tsx:339-356` | 长会话优先：delta 改按 id 定点更新 + `react-window` 类虚拟化；顺带把巨型 hook 拆成 connection/transcript/actions 域 | L |
| P2-3 | 粘贴图片直接把 ≤10MiB base64 塞进 DOM 与状态 | `Composer.tsx:387-391` | 预览用 `URL.createObjectURL` + 发送前压缩/降采样（canvas） | M |
| P2-4 | 无 `prefers-reduced-motion` 处理（pulse/ping/shimmer/aurora/滑入全量播放） | `index.css`、tailwind keyframes | `@media (prefers-reduced-motion: reduce)` 全局关闭装饰动画 | S |
| P2-5 | `collab_guest.html` 独立第三套令牌（配色 #4f8cff 系、Segoe UI、仅 dark） | `crates/server/src/collab_guest.html:9-16` | 对齐主应用 teal 令牌与字体（保持零构建单文件形态，内联一份精简变量即可） | S |
| P2-6 | 统计柱状图仅鼠标 hover 可读值，无键盘/触摸/表格替代 | `charts.tsx:95-140` | 加 `role="img"` + aria-label 汇总，或附带可折叠数据表 | S |
| P2-7 | 工作区拖拽（窗口移动/缩放/分割条）纯 Pointer 事件，无键盘替代 | `WorkspacePanel.tsx:263-287,513-520` | 把手改 button + 方向键调整（aria-valuenow） | M |
| P2-8 | 断点只有 sm/lg/xl，640-1024px 区间粒度粗 | 全局 | 视觉验证后补 `md:` 过渡规则 | S |
| P2-9 | 字体 3 族 × 全字重（71 个 woff/woff2 分片内嵌二进制） | `main.tsx:9-20` | 只打包实际使用的字重（400/500/600 中按需裁剪）；运行时影响小，主要是二进制体积 | S |
| P2-10 | 代码块底色 `#0b0d12/#070809` 体系外硬编码；highlight.js 仅 github-dark 主题 | `Transcript.tsx:367`、`WorkspacePanel.tsx:538`、`highlight.ts` | 定义 `--c-code-bg` 令牌；light 模式是否跟随浅色主题作为设计决策一并定 | S |
| P2-11 | c5-ui 无 eslint/prettier/stylelint 工具链 | `web/c5-ui/` | 最小集：eslint + react-hooks + jsx-a11y（可直接防住本审计中 2 类问题复发） | S |
| P2-12 | 树缩进算法不统一（`depth*14+6` vs `depth*12+4`）；ModelSwitcher 固定魔法宽度 | `BranchTreeModal.tsx:192`、`WorkspacePanel.tsx:597`、`ModelSwitcher.tsx:38,43` | 统一缩进常量；宽度改 min/max 语义类 | XS |

---

## 五、验证记录

- `npm --prefix web/c5-ui run build`（tsc -b + vite）通过；主包 489KB → 376KB（gzip 118.6KB）。
- `web/assets/` 仅含本次产物（1 JS + 1 CSS + 字体分片），9 个陈旧 bundle 从 git 与磁盘移除。
- 静态冒烟：index.html / 主 JS / 主 CSS 均 200 且内容可解析。
- 建议后续人工回归：键盘走查（Tab/箭头/Esc 全流程）、读屏（NVDA/VoiceOver 听对话流与审批播报）、移动端抽屉、light/dark + 7 种 accent 换肤。

## 六、最佳实践参考

- WCAG 2.1 AA：2.4.7 Focus Visible、1.4.3 Contrast、4.1.3 Status Messages
- WAI-ARIA Authoring Practices：Dialog（焦点陷阱/归还）、Menu（方向键循环/闭环）、Combobox（aria-expanded/activedescendant）
- web.dev：代码分割与动态 import；Prefer reduced motion
- 设计系统纪律：令牌唯一事实源（颜色/层级/字号均收敛到配置），任意值（`text-[Npx]`/`z-[N]`）需评审后使用
