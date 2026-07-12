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
npm run build      # tsc 类型检查 + vite 构建 → dist/
npm run deploy     # 将 dist/ 拷贝到 web/ 根目录
# 之后 `agent --serve` 即在 http://127.0.0.1:8080 提供本控制台
```

> 鉴权：若 `config.toml` 设置了 `server.auth_token`，在「设置」中填入对应 token
> （`${ENV}` 已展开后的值），会以 `?token=` 附带在请求上。

---

## 功能特性

- **实时对话流**：用户消息气泡、助手 Markdown 输出（代码块带复制）、流式光标
- **推理可视化**：可折叠「思考过程」、工具调用块（带输出折叠）
- **审批交互**：`ask` 渲染为批准/拒绝/文本回复卡片，与 `ClientFrame::Respond` 配对
- **状态机与用量**：运行面板展示 `AgentState`、累计 token（输入/输出/缓存）、成本、轮次/工具数
- **模型切换**：顶栏下拉选择模型别名（与会话绑定，切换即新对话，有内容时二次确认）
- **多模式**：Code / Architect / Ask / Debug（随 `new_task` 发送）
- **文件浏览**：只读浏览 agent 打开目录下的文件树（侧栏「文件浏览」），点击预览源码，
  支持 **highlight.js 语法高亮**（行号、语言标签、>2 MiB 截断提示、二进制占位）
- **连接管理**：新建会话、重连、连接状态指示、错误条
- **设置面板**：服务器地址、token、测试连接、主题、**实时强调色换肤**
- **响应式 + 深浅色**：侧栏/运行面板在桌面常驻、移动端抽屉；像素级响应式

---

## 项目结构

```
src/
├── main.tsx                      # 入口（单页，无路由，便于静态托管）
├── App.tsx                       # Provider 栈：Theme→Settings→Notifications→AgentSession
├── index.css                     # 设计系统（CSS 变量 + 深浅色主题）
├── lib/
│   ├── agent/
│   │   ├── types.ts              # 线协议类型 + parseFrame 容错解析
│   │   ├── useAgentSession.tsx   # 连接 hook：建会话 / WS / 帧分发 / 发送·审批·取消
│   │   ├── markdown.tsx          # 自研 Markdown 渲染（代码块/标题/列表/引用…）
│   │   └── ui.ts                 # 状态/级别→徽章元数据
│   ├── settings.tsx              # 连接设置 Context（localStorage 持久化）
│   ├── theme.tsx / notifications.tsx / format.ts / cn.ts
├── components/
│   ├── agent/
│   │   ├── AgentShell.tsx        # 主框架：侧栏 + 对话列 + 运行面板 + 抽屉
│   │   ├── Sidebar.tsx           # 品牌 / 新建 / 连接状态 / 主题
│   │   ├── Transcript.tsx        # 对话渲染（含审批卡、工具块、欢迎页）
│   │   ├── Composer.tsx          # 自适应输入框 + 模式 + 发送/停止
│   │   ├── Inspector.tsx         # 运行面板：连接/状态机/用量/模型
│   │   └── SettingsPanel.tsx     # 连接 + 外观设置
│   ├── ui.tsx / icons.tsx / Toaster.tsx   # 复用基础组件库
```

---

## 设计系统

颜色以 RGB 三元组 CSS 变量定义（`src/index.css` 的 `:root` / `.dark`），Tailwind 经
`rgb(var(--c-*) / <alpha-value>)` 映射。在「设置 → 外观」可实时切换全局强调色
（写入 `--c-primary` / `--c-primary-glow` 并持久化）。

---

_连接到本地 agent 服务；UI 自身不内置任何 Mock，所有数据来自真实后端事件流。_
