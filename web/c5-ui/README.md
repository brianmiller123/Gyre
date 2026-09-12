# Agent · Console (WebUI)

面向本项目 **Rust 编码智能体** 的现代化 Web 控制台。它通过 HTTP + WebSocket
直连 `agent --serve`（`agent_server`），实时渲染智能体的推理、工具调用与审批交互。

> 技术栈：**React 18 + TypeScript + Tailwind CSS + Vite**，零第三方图表依赖，
> 自研 Markdown 渲染与深浅色主题（CSS 变量 + 防闪烁预渲染）。

---

## 与后端的对接（真实协议，非 Mock）

| 方向 | 端点 / 帧 | 说明 |
| --- | --- | --- |
| HTTP | `GET /api/sessions?token=` | 建会话 → `{ session_id, ws_url }` |
| HTTP | `GET /api/models` | 可用模型 profile 列表 |
| HTTP | `GET /api/sessions?token=&model=` | 建会话，`model` 为别名（默认模型省略） |
| HTTP | `GET /api/workspace?token=` | 工作区根目录信息 |
| HTTP | `GET /api/fs?token=&path=` | 目录直接子项（只读，目录优先） |
| HTTP | `GET /api/file?token=&path=` | 文件内容（≤2 MiB，含二进制/截断标记；路径越界 403） |
| HTTP | `GET /api/stats?token=` | 活跃会话数 / 模型数 |
| WS | `/ws/{id}?token=` | 双向事件流 |
| C→S | `new_task` / `respond` / `cancel` | `ClientFrame`（serde `tag=type`） |
| S→C | `state_changed` `text_delta` `thinking_delta` `say` `ask` `tool_exec` `usage` `done` `error` | `ServerFrame` |

`parseFrame` 对 serde 的内部标签枚举怪癖（结构体变体拍平 / newtype 变体嵌套）做了
容错归一化。审批 `ask` 在对话流中渲染为「批准 / 拒绝」卡片（追问类提供文本回复），
回执以 `respond` 帧发送（`yes` / `no` / `{text}`）。

---

## 快速开始

### 开发模式（热更新）
```bash
# 终端 A：启动 agent 服务（默认 127.0.0.1:8080）
cargo run -- serve            # 或：agent --serve

# 终端 B：启动前端（:5173，/api 与 /ws 已代理到 :8080）
cd web/c5-ui
npm install
npm run dev
# 打开 http://localhost:5173
```

### 生产模式（由 agent 服务直接托管）
```bash
cd web/c5-ui
npm install
npm run build      # tsc 类型检查 + vite 构建，产物直接写入 web/ 根目录
# 之后 `agent --serve` 即在 http://127.0.0.1:8080 提供本控制台
```

> 鉴权：若 `config.toml` 设置了 `server.auth_token`，在「设置」中填入对应 token
> （`${ENV}` 已展开后的值），会以 `?token=` 附带在请求上。

---

## 功能特性

- **实时对话流**：用户消息气泡、助手 Markdown 输出（代码块带复制）、流式光标
- **推理可视化**：可折叠「思考过程」、工具调用块（带输出折叠）
- **审批交互**：`ask` 渲染为批准/拒绝/文本回复卡片，与 `ClientFrame::Respond` 配对
- **统一命令面板（⌘K）**：跨类目检索并执行全部应用动作（会话 / 视图 / 偏好），与侧栏导航、
  顶栏 ⋮ 菜单消费同一份 `AppAction` 注册表——功能不会只在某处存在，也不会两处重复
- **状态机与用量**：运行面板展示 `AgentState`、累计 token（输入/输出/缓存）、成本、轮次/工具数、
  上下文占用与子代理；≥xl 可停靠/收起，<xl 为右侧抽屉
- **模型切换**：输入区上方工具栏的胶囊下拉选择模型别名（与会话绑定，切换即新对话，有内容时二次确认）
- **多模式**：Code / Architect / Ask / Debug，与模型、审批模式并列在同一条工具栏（随 `new_task` 发送）
- **文件浏览**：只读浏览 agent 打开目录下的文件树（侧栏「文件浏览器」），点击预览源码，
  支持 **highlight.js 语法高亮**（行号、语言标签、>2 MiB 截断提示、二进制占位）
- **连接管理**：新建会话、重连、连接状态指示、错误条
- **设置面板**：唯一的偏好入口，「连接」「外观」两个标签页——连接页含服务器地址、token、
  默认模式、测试连接与 SOCKS5 出站代理；外观页含主题预览卡、语言下拉与**实时强调色换肤**
- **响应式 + 深浅色**：侧栏 ≥lg 常驻 / <lg 抽屉，运行面板 ≥xl 常驻可收起 / <xl 抽屉

---

## 信息架构与入口

功能按**三条正交轴**划分，每个轴只有一个归属地，同一动作不会在页面不同位置重复出现：

| 轴 | 内容 | 主要入口 | 其他路径 |
| --- | --- | --- | --- |
| **会话生命周期** | 新建 / 清空 / 停止 | 顶栏 **⋮** 会话菜单 | ⌘K 命令面板 · 斜杠命令（`/new`、`/clear`、`/cancel`） |
| **视图切换** | 文件浏览器 / 统计 / 运行面板 | 侧栏导航（文件浏览器 · 统计）+ 顶栏**遥测按钮**（运行面板开合） | ⌘K · `/files` · `#/stats` |
| **偏好设置** | 连接 / 外观 | 侧栏「设置」→ **设置面板** | ⌘K |

判定规则：**「换一段对话就不再需要」的进会话菜单；「换一段对话仍然需要」的进偏好。**

**侧栏**（≥lg 常驻，<lg 抽屉）自上而下：品牌 → **新建对话**（唯一常驻主按钮）→ 会话历史
（搜索 / 分组 / 重命名 / 删除 / 分支树）→ ── 视图导航（文件浏览器 · 统计）→ ── 设置 →
── **可点击的连接状态行**（→ 设置·连接）→ 版本页脚。
侧栏**不含**主题开关、语言菜单与清空对话按钮——它们是偏好或会话动作，各有唯一归属地。

**顶栏**从左到右：移动端菜单按钮 · 会话标题 + 状态/活动徽章 · **⌘K 入口** · **遥测按钮**
（宽屏显示累计 token/成本并高亮当前开合态，窄屏仅图标）· **⋮** 会话菜单。

**清空对话的唯一常驻入口**是顶栏 ⋮ 菜单（另见 ⌘K 与 `/clear`）；设置面板**不再包含**
「数据 / 清空对话」区块。

**运行面板（Inspector）**：≥xl 常驻并可停靠/收起（停靠态持久化于 `localStorage`
键 `agent-inspector-docked`），<xl 为右侧抽屉；唯一开合入口是顶栏遥测按钮。

**输入区**：上方一条工具栏并列三枚同构胶囊——**模型 / 审批模式 / 模式**；输入框底部只剩
附件、提示词增强与发送/停止。运行中「停止」只有输入框内一个入口（另有 `Esc`）。

---

## 全局快捷键

全站只绑定**两个**全局快捷键：

| 快捷键 | 作用 |
| --- | --- |
| `⌘K` / `Ctrl+K` | 打开 / 关闭命令面板（`CommandPalette`，统一入口检索全部动作） |
| `⌘⇧L` / `Ctrl+Shift+L` | 切换明暗主题 |

其余键位均为**输入区局部**，仅在输入框聚焦时生效：`Enter` 发送、`Shift+Enter` 换行、
`Esc` 停止运行中的任务，以及斜杠菜单展开时的 `↑` `↓` / `Tab` / `Enter` / `Esc`
（提示文案固定在输入区页脚）。

---

## 项目结构

```
src/
├── main.tsx                      # 入口（单页，无路由，便于静态托管）
├── App.tsx                       # Provider 栈：Theme→Settings→Notifications→AgentSession
├── index.css                     # 设计系统（CSS 变量 + 深浅色主题 + 表面/胶囊组件类）
├── lib/
│   ├── agent/
│   │   ├── types.ts              # 线协议类型 + parseFrame 容错解析
│   │   ├── useAgentSession.tsx   # 连接 hook：建会话 / WS / 帧分发 / 发送·审批·取消
│   │   ├── commands.ts           # 斜杠命令注册表（与 ⌘K 面板共用同一批会话方法）
│   │   ├── markdown.tsx          # 自研 Markdown 渲染（代码块/标题/列表/引用…）
│   │   └── ui.ts                 # 状态/级别→徽章元数据
│   ├── locales/                  # 四语扁平字典（en / zh / ja / ru，按需懒加载）
│   ├── settings.tsx              # 连接设置 Context（localStorage 持久化）
│   ├── theme.tsx / notifications.tsx / format.ts / cn.ts
├── components/
│   ├── agent/
│   │   ├── AgentShell.tsx        # 主框架：侧栏 + 对话列 + 运行面板 + 抽屉 + 全局快捷键
│   │   ├── CommandPalette.tsx    # ⌘K 命令面板 + AppAction 单一事实源（侧栏/顶栏共消费）
│   │   ├── Sidebar.tsx           # 品牌 / 新建 / 会话列表 / 视图导航 / 设置 / 连接状态
│   │   ├── SessionList.tsx       # 会话历史：搜索、分组、重命名、删除、分支树
│   │   ├── Transcript.tsx        # 对话渲染（含审批卡、工具块、欢迎页）
│   │   ├── Composer.tsx          # 自适应输入框 + 附件/增强/发送·停止
│   │   ├── ModelSwitcher.tsx     # 输入区工具栏：模型胶囊
│   │   ├── ApprovalModeSwitcher.tsx   # 输入区工具栏：审批模式胶囊
│   │   ├── ModeSwitcher.tsx      # 输入区工具栏：模式胶囊（与上两者同构）
│   │   ├── Inspector.tsx         # 运行面板：连接/状态机/用量/上下文/子代理/模型
│   │   ├── WorkspacePanel.tsx    # 文件浏览器（左/右停靠、浮动窗口，几何持久化）
│   │   ├── StatisticsPanel.tsx   # 统计仪表盘（`#/stats`，可刷新/分享）
│   │   └── SettingsPanel.tsx     # 设置面板：「连接」「外观」两个标签页
│   ├── ui.tsx / icons.tsx / Toaster.tsx   # 复用基础组件库
```

---

## 设计系统

颜色以 RGB 三元组 CSS 变量定义（`src/index.css` 的 `:root` / `.dark`），Tailwind 经
`rgb(var(--c-*) / <alpha-value>)` 映射。在「设置 → 外观」可实时切换全局强调色
（写入 `--c-primary` / `--c-primary-glow` 并持久化）。

### 设计令牌（`tailwind.config.ts`）

**字号** —— 闭合 9 档（含行高），**禁止 `text-[Npx]` 任意值**：

| 令牌 | `2xs` | `xs` | `sm` | `base` | `md` | `lg` | `xl` | `2xl` | `3xl` |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 字号 / 行高 | 11/15 | 12/17 | 13/19 | 14/21 | 15/23 | 17/24 | 20/28 | 24/32 | 30/36 |

**圆角** —— `sm 4 · md 8 · lg 10 · xl 14 · 2xl 20`（语义依次为：小控件 / 控件 / 卡片 / 浮层）。

**表面** —— 三级**实体**表面（组件类定义在 `src/index.css`），**禁止 `bg-surface-2/NN`
这类半透明任意值**：

| 组件类 | 用途 |
| --- | --- |
| `.glass-bar` | 顶栏 / 侧栏 / 输入区 / 运行面板等跨视口长条容器（唯一保留玻璃拟态的表面） |
| `.card` | 静置卡片（统计卡、区块容器） |
| `.card-inset` | 卡内嵌套的次级容器（行、指标小卡） |
| `.overlay-surface` | 下拉菜单 / 弹窗等浮层 |
| `.section-label` · `.chip` · `.focus-ring` | 区块小标题 · 上下文选择器胶囊 · 自定义触发器焦点态 |

**层级（z-index）** —— 使用语义令牌，不写裸 `z-[N]`：

```
raised 10 · header 20 · dropdown 50 · palette 70 · stats 80 · drawer 90 · workspace 95 · modal 100 · toast 120
```

完整方案、逐项依据与验证证据见
[`docs/frontend-ui-ia-refactor-2026-09-12.md`](../../docs/frontend-ui-ia-refactor-2026-09-12.md)。

---

_连接到本地 agent 服务；UI 自身不内置任何 Mock，所有数据来自真实后端事件流。_
