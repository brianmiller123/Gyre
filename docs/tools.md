# 工具参考

> 本文件由 `cargo xtask docs --tools` 生成（数据源：真实装配的工具实例）。
> 请勿手工编辑——工具 schema/描述变更后重跑该命令；CI 以 `--check` 校验漂移。

共 **34** 个工具：核心 7 / 可选 14（需 `[tools] <key> = true` 启用）/ 会话 11 / 宿主 2。

## 概览

| 工具 | 分组 | 能力 | 启用 | 说明 |
|---|---|---|---|---|
| `read_file` | 核心 | `read_only` | 恒开 | 读取工作区内文件内容并附行号。仅限只读，不修改文件。路径可带行选择器（`:N`、`:N-M`、`:N+K`、`:N-`、逗号多区间、`:raw`）精确取段；普通文本读取首行输出 `[相对路径#哈希]` 段头（哈希为全文指纹），编辑（app… |
| `write_file` | 核心 | `write` | 恒开 | 写入文件（覆盖）。自动创建父目录。属于写入类操作，通常需审批。 |
| `list_files` | 核心 | `read_only` | 恒开 | 列出目录条目。recursive=false（默认）仅直接子项；true 递归所有文件（尊重 .gitignore）。 |
| `run_command` | 核心 | `execute` | 恒开 | 在工作区目录执行 shell 命令（bash 兼容内嵌引擎，跨平台语义一致）并返回合并的 stdout/stderr。属于执行类操作，默认需审批。 |
| `grep` | 核心 | `read_only` | 恒开 | 在工作区正则搜索文件内容（尊重 .gitignore，隐藏文件也搜）。多文件时按文件分组展示（20 文件/页 × 每文件 20 命中），用 skip 翻页。 |
| `glob` | 核心 | `read_only` | 恒开 | 按 glob 模式（如 **/*.rs）发现工作区内文件路径。支持分号分隔多模式；         可按需包含隐藏文件或忽略 .gitignore（H33 对齐 omp `find`）。 |
| `web_search` | 核心 | `network` | 恒开 | 联网搜索（多 provider 顺序回退链：配置 TAVILY_API_KEY / BRAVE_API_KEY 时 Tavily / Brave API 优先，其后免 key DuckDuckGo；可选 searxng 实例经 GYRE_… |
| `replace_block` | 可选 | `write` | `[tools] ast = true` | 用 tree-sitter 解析文件，将「某行起始的句法块」（如函数/结构体/方法）整体替换为新内容。当前支持 Rust。 |
| `ast_search` | 可选 | `read_only` | `[tools] ast = true` | 用 ast-grep 按结构化 pattern 在源码文件中搜索（支持 $X / $$$Y meta 变量），返回每处匹配的行号与片段。多语言：rust/python/javascript/typescript/go。 |
| `ast_rewrite` | 可选 | `write` | `[tools] ast = true` | 用 ast-grep 按结构化 pattern 重写源码文件（支持 $X / $$$Y meta 变量，原地改写）。多语言：rust/python/javascript/typescript/go。 |
| `read_image` | 可选 | `read_only` | `[tools] image = true` | 读取本地图片文件（png/jpeg/gif/webp），作为图像供模型查看分析。仅限只读。 |
| `image_gen` | 可选 | `network` | `[tools] image = true` | 调用图像生成 API（OpenAI 兼容 /images/generations）按 prompt 生成图片。需配置环境变量 IMAGE_API_KEY 或 OPENAI_API_KEY。 |
| `lsp` | 可选 | `read_only` | `[tools] lsp = true` | Language Server Protocol client: get diagnostics, go to definition, find references, hover for type info, list document… |
| `lsp_apply` | 可选 | `write` | `[tools] lsp = true` | 应用 LSP 编辑（write_file 之外的结构化编辑）：`edits` 直接传入 lsp rename/code_actions 返回的编辑列表；或 `code_action_index` + `uri`/`line`/`chara… |
| `apply_hashline` | 可选 | `write` | `[tools] hashline = true` | 按 hashline 行锚定格式批量编辑文件：每段以 [path#hash] 开头，含 SWAP/DEL/INS/REM/MV 操作。每次编辑后行号重新编号，须基于最新 read 的行号。 |
| `run_pty_command` | 可选 | `execute` | `[tools] pty = true` | 在伪终端（PTY）中执行 shell 命令，返回合并 stdout/stderr 与退出码。适用于需要 TTY 的命令（top/vim/交互式 REPL 等）。属执行类操作，默认需审批。 |
| `shell_session` | 可选 | `execute` | `[tools] pty = true` | 在**持久** PTY 会话中执行 shell 命令：`cd` / `export` 等状态跨命令保留（多条命令共享同一 shell）。op=run 执行 / status 查看会话状态 / close 回收会话。适合「先切目录再构建」这… |
| `debug` | 可选 | `execute` | `[tools] debug = true` | Debug Adapter Protocol client: launch/attach a program, continue/pause/step, list threads, read stack traces/scopes/var… |
| `ssh` | 可选 | `execute` | `[tools] ssh = true` | 解析 ~/.ssh/config 并在远端主机执行命令（action ∈ connect/exec/list/disconnect）。属于执行类操作，默认需审批；v1 每次 exec 直连，不维护长连接。 |
| `browser` | 可选 | `execute` | `[tools] browser = true` | 控制 headless chromium 浏览网页：navigate(url) 打开页面；evaluate(js) 在页面执行 JavaScript（returnByValue 取回结果）；screenshot(selector?, fu… |
| `github` | 可选 | `network` | `[tools] github = true` | 查询/操作 GitHub PR、issue、仓库文件、搜索与 Actions（CI）。action（omp 别名 `op`）∈ {get_pr,list_prs,get_issue,list_issues,list_runs,get_ru… |
| `todo` | 会话 | `read_only` | 恒开 | 任务清单：规划多步任务并跟踪进度。单活跃不变量——任何时刻至多一个 in_progress，start 新任务自动闭环旧的。多步任务（≥3 步）开工前先 write/start 建清单，每完成一步即 complete 并 start 下一… |
| `ask` | 会话 | `read_only` | 恒开 | 向用户提一个需要决策的问题：当任务指令含糊、存在多种 materially 不同做法、或缺少必要信息（密钥/账号/偏好）时使用。返回用户的文本回答（含选项 label 或自由输入）。用户拒绝时返回错误——此时应基于现有信息选择最保守做法继… |
| `checkpoint` | 会话 | `read_only` | 恒开 | 在当前对话位置打一个检查点（单活跃：新的覆盖旧的）。适合在开始一段探索性改动前建立基线；走偏后可用 rewind 回卷到检查点（被回卷的消息折叠为摘要）。note 说明该位置的意义。 |
| `rewind` | 会话 | `read_only` | 恒开 | 回卷到最近一次 checkpoint 的位置：检查点之后的消息折叠为 handoff 摘要并继续。没有检查点时返回错误。回卷消耗检查点（需重新 checkpoint）。适合探索走偏后的整体回退。 |
| `security_scan` | 会话 | `write` | 恒开 | 扫描工作区中的常见安全问题：密钥/令牌泄漏（API key、私钥、JWT、.env 明文）、密钥文件权限过宽、危险代码模式（rm -rf /、eval 外部输入、SQL 拼接）。v1 为本地规则扫描（无远程知识库、无 SARIF）。pat… |
| `hub` | 会话 | `read_only` | 恒开 | 进程内消息总线、子任务监督面与进程托管：代理消息（send 定向投递 / recv 拉取收件箱 / list 在册代理 / agents 名册（含状态与积压）/ broadcast 广播给除自己外全部 / wait 阻塞等下一条消息 / … |
| `recall` | 会话 | `read_only` | 恒开 | 检索跨会话长期记忆：按查询返回最相关的过往记忆（带相关度分数）。在回答关于过往会话、项目历史或用户偏好的问题前主动使用。仅 structured 记忆后端支持语义检索；local 后端返回空。 |
| `retain` | 会话 | `read_only` | 恒开 | 把一条事实（决策、偏好、项目约定）保存到跨会话长期记忆，供未来会话 recall。importance 0..=5（默认 1）：高价值事实用 3-5（如用户偏好、关键架构决策）。 |
| `reflect` | 会话 | `read_only` | 恒开 | 基于跨会话长期记忆综合回答一个开放问题（只读，不写入）：检索记忆库中与问题相关的过往记忆并综合为回答。适合「我们从过往会话中学到了什么」「关于 X 的既有结论/偏好是什么」类提问；要保存新知识请改用 retain 或 learn。 |
| `memory_edit` | 会话 | `read_only` | 恒开 | 管理跨会话长期记忆：search 检索（命中带记录 id）、update 就地改写内容/重要性、invalidate 标记失效（可指向替代记录，失效后不再参与检索但保留可追溯）、forget 按 id 删除、banks 列出记忆库、cle… |
| `learn` | 会话 | `read_only` | 恒开 | 把本会话学到的可复用知识存入跨会话记忆：fact/lesson（默认 fact）存为可检索记录，mental_model 存为心智模型（下次会话自动注入）。内容须自包含、不依赖当前上下文（是什么、何时、为何）。 |
| `goal` | 宿主 | `read_only` | 恒开 | 管理持续目标（objective）：op=create 新建、get 查看、replace 替换、pause 暂停、resume 恢复、complete 完成、drop 放弃。目标活跃时引擎会在停止边界自动续跑，直到 complete 或… |
| `task` | 宿主 | `read_only` | 恒开 | 委派子任务给独立子 Agent（独立上下文，不继承当前对话），返回其最终结果。支持单任务（task）或并行多任务（tasks，按并发护栏并行）。单任务可给 output_schema（JSON Schema）要求结构化 JSON 输出——… |

## 参数详情

### `read_file`

读取工作区内文件内容并附行号。仅限只读，不修改文件。路径可带行选择器（`:N`、`:N-M`、`:N+K`、`:N-`、逗号多区间、`:raw`）精确取段；普通文本读取首行输出 `[相对路径#哈希]` 段头（哈希为全文指纹），编辑（apply_hashline）必须引用最新段头与行号锚定。

- 分组：核心 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 是 | 文件路径（相对工作区根或绝对路径），或内部协议：skill://<name>[/<rel>]（skill 内容）、memory://[summary\|full]（跨会话记忆）、mcp://<server>/<uri>（MCP 资源）、local://<rel>（显式工作区相对）、artifact://<id>（shake 归档内容回读）、http(s)://（抓取网页）、pr://<owner>/<repo>[/<N>]（GitHub PR，文件缓存）、issue://<owner>/<repo>[/<N>]（GitHub issue）。本地路径可带行选择器后缀（1 基绝对行号）：`:N` / `:N-`（第 N 行到文件尾）、`:N-M`（闭区间）、`:N+K`（第 N 行起 K 行）、`:a-b,c-d`（逗号多区间，升序合并）、`:raw`（原样输出，无行号）、`:raw:N-M` 或 `:N-M:raw`（原文切片）。普通文本读取首行输出 `[相对路径#哈希]` 段头（哈希为全文指纹）；后续 apply_hashline 编辑必须引用最新段头与行号。raw / 协议 / 归档 / SQLite / notebook 读取无段头。 |
| `summary` | `boolean` | 否 | 可选：对支持语言的代码文件做结构摘要——折叠大体块、保留签名，行号与原文一致。适合快速浏览大文件；需逐行编辑时仍用真实行号。与行选择器互斥（带选择器时忽略）。 |

### `write_file`

写入文件（覆盖）。自动创建父目录。属于写入类操作，通常需审批。

- 分组：核心 · 能力：`write` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 是 | 文件路径 |
| `content` | `string` | 是 | 完整文件内容 |

### `list_files`

列出目录条目。recursive=false（默认）仅直接子项；true 递归所有文件（尊重 .gitignore）。

- 分组：核心 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 否 | 目录路径（相对工作区，默认 .） |
| `recursive` | `boolean` | 否 | 是否递归（默认 false） |

### `run_command`

在工作区目录执行 shell 命令（bash 兼容内嵌引擎，跨平台语义一致）并返回合并的 stdout/stderr。属于执行类操作，默认需审批。

- 分组：核心 · 能力：`execute` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `command` | `string` | 是 | 完整命令字符串（bash 语法） |
| `cwd` | `string` | 否 | 执行目录；相对路径基于工作区根解析，须已存在。默认工作区根 |
| `env` | `object` | 否 | 附加环境变量（覆盖会话同名变量）；键须为合法环境变量名 |
| `timeout` | `integer` | 否 | 超时秒数（1–3600）；0 表示不限时。默认 120 |
| `async` | `boolean` | 否 | true = 后台执行：立即返回作业 id，命令继续运行；完成后结果自动投回会话，hub 的 jobs/cancel/wait 可查询/取消/等待 |
| `pty` | `boolean` | 否 | true = 在伪终端（PTY）中运行：需要 TTY 的命令（top/vim/交互式安装器等）用此项；与 async 互斥。未接入 PTY 时返回明确错误 |

### `grep`

在工作区正则搜索文件内容（尊重 .gitignore，隐藏文件也搜）。多文件时按文件分组展示（20 文件/页 × 每文件 20 命中），用 skip 翻页。

- 分组：核心 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `pattern` | `string` | 是 | 正则模式（Rust regex 语法；保留首尾空白，空模式报错） |
| `path` | `any` | 否 | 搜索目标：文件、目录或 glob（如 src/**/*.ts）；多个用分号分隔（"src; tests"）；缺省搜索工作区根 (".") |
| `case` | `boolean` | 否 | 区分大小写搜索（默认 true） |
| `gitignore` | `boolean` | 否 | 尊重 .gitignore（默认 true） |
| `skip` | `any` | 否 | 收集结果前跳过的文件数——上次调用触达文件上限时用它翻页 |

### `glob`

按 glob 模式（如 **/*.rs）发现工作区内文件路径。支持分号分隔多模式；         可按需包含隐藏文件或忽略 .gitignore（H33 对齐 omp `find`）。

- 分组：核心 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `pattern` | `string` | 否 | glob 模式（可用 `;` 分隔多个，如 `src/**/*.ts; test/**/*.ts`） |
| `path` | `string` | 否 | `pattern` 的别名（对齐 omp `find.path`）：可为 glob、文件或目录；缺省搜索工作区根 |
| `hidden` | `boolean` | 否 | 是否包含隐藏文件（`.*`）；默认 true（对齐 omp） |
| `gitignore` | `boolean` | 否 | 是否尊重 .gitignore；默认 true |
| `limit` | `integer` | 否 | 结果上限（默认 200，最大 200） |

### `web_search`

联网搜索（多 provider 顺序回退链：配置 TAVILY_API_KEY / BRAVE_API_KEY 时 Tavily / Brave API 优先，其后免 key DuckDuckGo；可选 searxng 实例经 GYRE_SEARXNG_URL）。query 支持 `site:example.com` 过滤；命中 arxiv/crates.io/npm/github 时返回结构 markdown。`recency` 为结果时限过滤（近一天/周/月/年）：Tavily/Brave/DuckDuckGo 原生支持；searxng 无原生周窗口，week 降级为 month。

- 分组：核心 · 能力：`network` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `query` | `string` | 是 | 搜索查询（可用 site: 限定域名） |
| `max_results` | `integer` | 否 | 返回结果数上限（默认 5） |
| `recency` | `string` | 否 | 结果时限过滤（可选）；searxng 后端无原生周窗口，week 降级为 month |

### `replace_block`

用 tree-sitter 解析文件，将「某行起始的句法块」（如函数/结构体/方法）整体替换为新内容。当前支持 Rust。

- 分组：可选 · 能力：`write` · 启用：`[tools] ast = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 是 | 文件路径 |
| `line` | `integer` | 是 | 块起始的 1-indexed 行号 |
| `content` | `string` | 是 | 替换为的新块内容 |

### `ast_search`

用 ast-grep 按结构化 pattern 在源码文件中搜索（支持 $X / $$$Y meta 变量），返回每处匹配的行号与片段。多语言：rust/python/javascript/typescript/go。

- 分组：可选 · 能力：`read_only` · 启用：`[tools] ast = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 是 | 源码文件路径 |
| `pattern` | `string` | 是 | ast-grep 模式，如 `fn $NAME($$$ARGS) { $$$BODY }` |
| `pat` | `string` | 否 | `pattern` 的别名（对齐 omp `ast_grep.pat`） |
| `lang` | `string` | 否 | 语言（可选；省略则按扩展名推断） |
| `strictness` | `string` | 否 | 匹配严格度（可选，默认 smart） |

### `ast_rewrite`

用 ast-grep 按结构化 pattern 重写源码文件（支持 $X / $$$Y meta 变量，原地改写）。多语言：rust/python/javascript/typescript/go。

- 分组：可选 · 能力：`write` · 启用：`[tools] ast = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 是 | 源码文件路径 |
| `pattern` | `string` | 是 | ast-grep 匹配模式 |
| `pat` | `string` | 否 | `pattern` 的别名（对齐 omp `ast_edit.ops[].pat`） |
| `rewrite` | `string` | 是 | 重写模板（可引用 pattern 中的 meta 变量） |
| `lang` | `string` | 否 | 语言（可选；省略则按扩展名推断） |
| `strictness` | `string` | 否 | 匹配严格度（可选，默认 smart） |
| `preview` | `boolean` | 否 | 暂存模式：不落盘，返回 (proposed) 预览；用 write_file xd://resolve 应用 |

### `read_image`

读取本地图片文件（png/jpeg/gif/webp），作为图像供模型查看分析。仅限只读。

- 分组：可选 · 能力：`read_only` · 启用：`[tools] image = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 是 | 图片路径（相对工作区根或绝对路径） |

### `image_gen`

调用图像生成 API（OpenAI 兼容 /images/generations）按 prompt 生成图片。需配置环境变量 IMAGE_API_KEY 或 OPENAI_API_KEY。

- 分组：可选 · 能力：`network` · 启用：`[tools] image = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `prompt` | `string` | 是 | 图像描述（prompt） |
| `size` | `string` | 否 | 尺寸如 1024x1024（可选，默认 1024x1024） |
| `model` | `string` | 否 | 模型如 dall-e-3（可选） |

### `lsp`

Language Server Protocol client: get diagnostics, go to definition, find references, hover for type info, list document symbols, search workspace symbols, rename symbols, and get code actions/quick fixes.

- 分组：可选 · 能力：`read_only` · 启用：`[tools] lsp = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string` | 是 | The LSP operation to perform. |
| `uri` | `string` | 否 | File URI (file:///path/to/file). Required for all actions except workspace_symbols. |
| `text` | `string` | 否 | Full file content (required for open_document). |
| `line` | `integer` | 否 | 0-based line number. |
| `character` | `integer` | 否 | 0-based character offset. |
| `new_name` | `string` | 否 | New symbol name (required for rename). |
| `query` | `string` | 否 | Search query (required for workspace_symbols). |

### `lsp_apply`

应用 LSP 编辑（write_file 之外的结构化编辑）：`edits` 直接传入 lsp rename/code_actions 返回的编辑列表；或 `code_action_index` + `uri`/`line`/`character`取对应 code action 的 edits 应用、有 command 则执行。多文件自动分组，自底向上应用，区间重叠报错。

- 分组：可选 · 能力：`write` · 启用：`[tools] lsp = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `edits` | `array` | 否 | LSP 编辑列表（lsp rename/code_actions 输出原样回传） |
| `code_action_index` | `integer` | 否 | 按 code_actions 返回列表的下标应用（与 uri/line/character 配合） |
| `uri` | `string` | 否 | 文件 URI（code_action_index 模式） |
| `line` | `integer` | 否 | 行（code_action_index 模式，0-based） |
| `character` | `integer` | 否 | 列（code_action_index 模式，0-based） |

### `apply_hashline`

按 hashline 行锚定格式批量编辑文件：每段以 [path#hash] 开头，含 SWAP/DEL/INS/REM/MV 操作。每次编辑后行号重新编号，须基于最新 read 的行号。

- 分组：可选 · 能力：`write` · 启用：`[tools] hashline = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `patch` | `string` | 否 | hashline patch 文本，可含多个 [path#hash] 段；段内为 SWAP/DEL/INS/REM/MV 操作 |
| `input` | `string` | 否 | `patch` 的别名（对齐 omp `edit` 工具的 patch 模式字段名） |
| `path` | `string` | 否 | 可选：当 patch 不含段头时的回退目标文件路径 |

### `run_pty_command`

在伪终端（PTY）中执行 shell 命令，返回合并 stdout/stderr 与退出码。适用于需要 TTY 的命令（top/vim/交互式 REPL 等）。属执行类操作，默认需审批。

- 分组：可选 · 能力：`execute` · 启用：`[tools] pty = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `command` | `string` | 是 | 完整命令字符串 |
| `timeout_ms` | `integer` | 否 | 可选超时（毫秒） |
| `rows` | `integer` | 否 | 可选终端行数（默认 24） |
| `cols` | `integer` | 否 | 可选终端列数（默认 80） |

### `shell_session`

在**持久** PTY 会话中执行 shell 命令：`cd` / `export` 等状态跨命令保留（多条命令共享同一 shell）。op=run 执行 / status 查看会话状态 / close 回收会话。适合「先切目录再构建」这类多步流程；一次性无状态命令用 run_pty_command 或 run_command。属执行类操作，默认需审批。

- 分组：可选 · 能力：`execute` · 启用：`[tools] pty = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `op` | `string` | 是 | run 执行命令 / status 查看会话（是否存活、是否失效）/ close 回收 |
| `command` | `string` | 否 | op=run：命令字符串（在持久 shell 中执行） |
| `timeout_ms` | `integer` | 否 | op=run 超时（毫秒，默认 60000）；超时会杀掉会话，下次 run 自动重建 |

### `debug`

Debug Adapter Protocol client: launch/attach a program, continue/pause/step, list threads, read stack traces/scopes/variables, evaluate expressions, and manage breakpoints. Supports lldb-dap (C/C++/Rust), dlv dap (Go), debugpy (Python).

- 分组：可选 · 能力：`execute` · 启用：`[tools] debug = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string` | 是 | The DAP operation to perform. |
| `adapter` | `string` | 否 | Adapter name: lldb-dap / dlv / debugpy. Default: auto (first found on PATH). |
| `session` | `string` | 否 | Session handle id (v1: single session, ignored). |
| `source` | `string` | 否 | Source file path (set_breakpoint). |
| `line` | `integer` | 否 | 1-based line number (set_breakpoint). |
| `thread_id` | `integer` | 否 | Thread id (continue/pause/next/step_in/step_out/stack_trace). Default: first thread. |
| `frame_id` | `integer` | 否 | Stack frame id from stack_trace (scopes/evaluate). |
| `variables_reference` | `integer` | 否 | Variables reference from scopes/variables (variables). |
| `expression` | `string` | 否 | Expression to evaluate (evaluate). |
| `breakpoint_id` | `integer` | 否 | Breakpoint id returned by set_breakpoint (remove_breakpoint). |

### `ssh`

解析 ~/.ssh/config 并在远端主机执行命令（action ∈ connect/exec/list/disconnect）。属于执行类操作，默认需审批；v1 每次 exec 直连，不维护长连接。

- 分组：可选 · 能力：`execute` · 启用：`[tools] ssh = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string` | 是 | 操作：connect=解析并校验主机配置；exec=远端执行命令；list=列出可用主机；disconnect=断开（v1 no-op） |
| `host` | `string` | 否 | ~/.ssh/config 中的主机别名 |
| `command` | `string` | 否 | exec 时在远端执行的命令（原样传给远端 shell） |

### `browser`

控制 headless chromium 浏览网页：navigate(url) 打开页面；evaluate(js) 在页面执行 JavaScript（returnByValue 取回结果）；screenshot(selector?, full_page?) 截图保存；click(selector)、text(selector, text)、scroll(selector?/direction?/amount?) 模拟交互；close 关闭浏览器实例。

- 分组：可选 · 能力：`execute` · 启用：`[tools] browser = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string` | 是 | 浏览器操作：navigate 打开页面；evaluate 执行 JS；screenshot 截图；click/text/scroll 模拟交互；close 关闭浏览器 |
| `url` | `string` | 否 | navigate：要打开的 URL |
| `js` | `string` | 否 | evaluate：要执行的 JavaScript（returnByValue + awaitPromise 取回结果） |
| `selector` | `string` | 否 | screenshot/click/text/scroll：CSS 选择器 |
| `text` | `string` | 否 | text：填入输入框的文本 |
| `full_page` | `boolean` | 否 | screenshot：true 时截取整页而非视口 |
| `direction` | `string` | 否 | scroll：无 selector 时的滚动方向（默认 down） |
| `amount` | `integer` | 否 | scroll：滚动像素数（默认 500） |

### `github`

查询/操作 GitHub PR、issue、仓库文件、搜索与 Actions（CI）。action（omp 别名 `op`）∈ {get_pr,list_prs,get_issue,list_issues,list_runs,get_run,get_run_logs,repo_view,file_read,search_issues,search_prs,search_code,search_repos,graphql,create_pr,merge_pr,comment}；repo="owner/name"（search_repos 可省）。get_* 需 number；list_* 可选 limit；file_read 需 path（可选 branch）；search_* 需 query；graphql 需 query（+可选 variables）；create_pr 需 title/head/base；comment 需 body；merge_pr 可选 method。omp 的 `pr_create` 等价于 `create_pr`。写操作需配置 allow_write。鉴权读取 GH_TOKEN/GITHUB_TOKEN。

- 分组：可选 · 能力：`network` · 启用：`[tools] github = true`

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string` | 是 | 查询/操作动作（omp 别名 `op`） |
| `op` | `string` | 否 | `action` 的 omp 别名（两者给一个即可，`action` 优先） |
| `path` | `string` | 否 | file_read：仓库内相对路径 |
| `branch` | `string` | 否 | file_read：分支/commit（缺省 = 默认分支） |
| `repo` | `string` | 否 | owner/repo，例如 "octocat/Hello-World" |
| `number` | `integer` | 否 | PR/issue/run 编号（get_*/merge_pr/comment 必填） |
| `limit` | `integer` | 否 | 列表返回条数上限（list_*；默认 10，上限 100） |
| `title` | `string` | 否 | create_pr 的标题 |
| `head` | `string` | 否 | create_pr 的源分支（head） |
| `base` | `string` | 否 | create_pr 的目标分支（base） |
| `body` | `string` | 否 | create_pr/comment 的正文 |
| `method` | `string` | 否 | merge_pr 的合并方式（默认 merge） |
| `query` | `string` | 否 | graphql 动作的 GraphQL 查询 |
| `variables` | `object` | 否 | graphql 动作的变量 |

### `todo`

任务清单：规划多步任务并跟踪进度。单活跃不变量——任何时刻至多一个 in_progress，start 新任务自动闭环旧的。多步任务（≥3 步）开工前先 write/start 建清单，每完成一步即 complete 并 start 下一步；被外因卡住用 block（写明原因）。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `op` | `string` | 否 | view 查看；write/init 全量替换；append 追加；start 新任务(自动闭环旧 in_progress)；update 改内容；complete/done 完成；abandon/drop 放弃；rm/remove 删除条目；block/unblock 阻塞管理；pending 重新排队（omp 别名已对齐，H32） |
| `items` | `array` | 否 | write 专用：完整清单（替换全部），phase 可省略(默认 pending)，至多一个 in_progress |
| `content` | `string` | 否 | start/update：任务内容 |
| `id` | `string` | 否 | update/complete/abandon/block/unblock/pending：目标 id（如 t3） |
| `reason` | `string` | 否 | block：阻塞原因 |

### `ask`

向用户提一个需要决策的问题：当任务指令含糊、存在多种 materially 不同做法、或缺少必要信息（密钥/账号/偏好）时使用。返回用户的文本回答（含选项 label 或自由输入）。用户拒绝时返回错误——此时应基于现有信息选择最保守做法继续，不要重复追问。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `question` | `string` | 否 | 要问的问题（自包含，用户不看上下文也能理解） |
| `questions` | `array` | 否 | 多问题形态（对齐 omp `ask.questions`）：逐条提问并汇总回答；与 `question` 二选一，`question` 优先 |
| `options` | `array` | 否 | 2-5 个候选项（可选；给出时用户可回 label 或自由输入） |
| `header` | `string` | 否 | 2-6 词的意图概括（显示在问题上方，如「选择认证方案」） |

### `checkpoint`

在当前对话位置打一个检查点（单活跃：新的覆盖旧的）。适合在开始一段探索性改动前建立基线；走偏后可用 rewind 回卷到检查点（被回卷的消息折叠为摘要）。note 说明该位置的意义。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `note` | `string` | 否 | 检查点说明（如「重构前基线」） |
| `status` | `boolean` | 否 | true 时仅查看当前检查点，不创建 |

### `rewind`

回卷到最近一次 checkpoint 的位置：检查点之后的消息折叠为 handoff 摘要并继续。没有检查点时返回错误。回卷消耗检查点（需重新 checkpoint）。适合探索走偏后的整体回退。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `confirm` | `boolean` | 否 | 确认回卷（防误触；true 才执行） |

### `security_scan`

扫描工作区中的常见安全问题：密钥/令牌泄漏（API key、私钥、JWT、.env 明文）、密钥文件权限过宽、危险代码模式（rm -rf /、eval 外部输入、SQL 拼接）。v1 为本地规则扫描（无远程知识库、无 SARIF）。path 可选：缺省扫整个工作区（上限 2000 文件）。

- 分组：会话 · 能力：`write` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 否 | 要扫描的相对路径（文件或目录）；缺省扫工作区根 |
| `limit` | `integer` | 否 | 最多报告条数（防刷屏） |

### `hub`

进程内消息总线、子任务监督面与进程托管：代理消息（send 定向投递 / recv 拉取收件箱 / list 在册代理 / agents 名册（含状态与积压）/ broadcast 广播给除自己外全部 / wait 阻塞等下一条消息 / inbox 列队中消息，peek 不消费）、回执（send tracked=true 签发 ack_id；acks 查看未兑现数 / ack 确认 / unwatch 撤销，撤销后等待方立即得到明确错误）、子任务观测（jobs 快照 / cancel 取消，需装配层接入监督句柄）、长驻进程托管（start 拉起 / ps / logs / stop / restart / describe；send / wait 入参含 name 时作用于进程，与 to / from 互斥）。进程托管为进程内最小实现，有意偏差：无 broker 子进程、无磁盘持久化、不做 detached/persist 生命周期、无 PTY（stdin/stdout 为管道）；进程退出不主动推送消息，用 wait（name + for=exit）或 logs（follow=true）轮询。消息、回执与进程记录不跨会话保留；监督句柄/进程托管未接入时相应 op 明确报错而非静默忽略。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `op` | `string` | 是 | send 投递 / recv 拉取收件箱（drain）/ list 在册代理 / agents 名册（含状态与积压，H41）/ broadcast 广播给除自己外全部 / wait 阻塞等下一条消息 / inbox 列队中消息 / jobs 子任务快照 / cancel 取消在途子任务；进程托管：start 拉起 / ps 快照 / logs 读日志 / stop 停止 / restart 重启 / describe 规格+状态 |
| `to` | `string` | 否 | send：目标代理 id（list 可查） |
| `message` | `string` | 否 | send：消息体（纯文本） |
| `from` | `string` | 否 | wait：只接受来自该代理 id 的消息（其余消息缓存不丢） |
| `timeout_ms` | `integer` | 否 | wait：超时毫秒（默认 120000；0=无限等待） |
| `peek` | `boolean` | 否 | inbox：true 时仅列出队中消息，不消费 |
| `ack_id` | `integer` | 否 | ack：确认某回执（send 带 tracked=true 时返回 id）；unwatch：撤销该回执 |
| `tracked` | `boolean` | 否 | send：true 时签发回执 id（接收方消费后自动确认，可用 wait_ack 语义核对） |
| `ids` | `array` | 否 | cancel：要取消的子任务 id 列表（jobs 可查） |
| `name` | `string` | 否 | 进程名；send/wait 携带 name 时分流到进程语义（与 to/from 互斥） |
| `application` | `string` | 否 | start：可执行文件 |
| `args` | `array` | 否 | start：参数列表 |
| `env` | `object` | 否 | start：附加环境变量 |
| `cwd` | `string` | 否 | start：工作目录（默认当前目录） |
| `ready` | `object` | 否 | start：就绪条件（log/port 至少其一；超时→状态 failed 但进程保留可 stop） |
| `restart` | `string` | 否 | start：重启策略（退避 500ms→8s，最多 5 次；显式 stop 不算失败） |
| `lines` | `integer` | 否 | logs：行数（默认 100，上限 1000） |
| `head` | `boolean` | 否 | logs：从头取（默认取尾部） |
| `grep` | `string` | 否 | logs：行过滤正则（Rust regex） |
| `follow` | `boolean` | 否 | logs：等待新输出（配合 timeout，超时即返回当前结果） |
| `cursor` | `integer` | 否 | logs：起始行序号（上次返回的 cursor） |
| `for` | `string` | 否 | wait(name)：等待就绪或退出（默认 exit） |
| `pattern` | `string` | 否 | wait(name)：等待输出匹配该正则（follow 语义） |
| `text` | `string` | 否 | send(name)：写入 stdin 的文本 |
| `enter` | `boolean` | 否 | send(name)：文本后追加换行（默认 true） |
| `keys` | `array` | 否 | send(name)：按键序列（ENTER TAB ESCAPE CTRL_C CTRL_D UP DOWN LEFT RIGHT） |
| `signal` | `string` | 否 | send(name)：向进程组发送信号（给出 signal 时忽略 text/keys） |
| `timeout` | `number` | 否 | 进程 op 超时秒数（wait/logs follow 默认 30；stop 默认 5） |

### `recall`

检索跨会话长期记忆：按查询返回最相关的过往记忆（带相关度分数）。在回答关于过往会话、项目历史或用户偏好的问题前主动使用。仅 structured 记忆后端支持语义检索；local 后端返回空。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `query` | `string` | 是 | 检索查询（自然语言，如「用户偏好哪个包管理器」） |
| `limit` | `integer` | 否 | 返回条数上限（默认 8） |

### `retain`

把一条事实（决策、偏好、项目约定）保存到跨会话长期记忆，供未来会话 recall。importance 0..=5（默认 1）：高价值事实用 3-5（如用户偏好、关键架构决策）。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `content` | `string` | 是 | 要记住的事实（陈述句，自包含，不依赖当前上下文） |
| `importance` | `integer` | 否 | 重要性 0..=5（默认 1） |

### `reflect`

基于跨会话长期记忆综合回答一个开放问题（只读，不写入）：检索记忆库中与问题相关的过往记忆并综合为回答。适合「我们从过往会话中学到了什么」「关于 X 的既有结论/偏好是什么」类提问；要保存新知识请改用 retain 或 learn。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `query` | `string` | 是 | 要回答的问题 |
| `context` | `string` | 否 | 可选补充上下文（帮助限定检索范围） |

### `memory_edit`

管理跨会话长期记忆：search 检索（命中带记录 id）、update 就地改写内容/重要性、invalidate 标记失效（可指向替代记录，失效后不再参与检索但保留可追溯）、forget 按 id 删除、banks 列出记忆库、clear 清空全部（须 confirm: true）。先 search 拿 id 再 update/invalidate/forget；仅 structured 后端支持逐条修改/删除。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `op` | `string` | 是 | 操作 |
| `query` | `string` | 否 | search：检索查询（自然语言） |
| `limit` | `integer` | 否 | search：返回条数上限（默认 8） |
| `id` | `string` | 否 | update/invalidate/forget：记录 id（来自 search 命中） |
| `content` | `string` | 否 | update：替换正文（与 importance 至少给一个） |
| `importance` | `integer` | 否 | update：替换重要性 0-5（与 content 至少给一个） |
| `replacement_id` | `string` | 否 | invalidate：替代本条的记录 id（可省略） |
| `confirm` | `boolean` | 否 | clear：须显式传 true 才执行清空 |

### `learn`

把本会话学到的可复用知识存入跨会话记忆：fact/lesson（默认 fact）存为可检索记录，mental_model 存为心智模型（下次会话自动注入）。内容须自包含、不依赖当前上下文（是什么、何时、为何）。

- 分组：会话 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `content` | `string` | 是 | 要记住的知识（自包含陈述句：是什么、何时、为何） |
| `kind` | `string` | 否 | 知识类型：fact=事实、lesson=经验教训（均入检索库）；mental_model=心智模型（注入式，不参与检索） |

### `goal`

管理持续目标（objective）：op=create 新建、get 查看、replace 替换、pause 暂停、resume 恢复、complete 完成、drop 放弃。目标活跃时引擎会在停止边界自动续跑，直到 complete 或达到续跑上限；调用 complete 前必须核对当前仓库状态并给出直接证据。

- 分组：宿主 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `op` | `string` | 是 | 操作：create/replace 需要 objective；complete 表示已核实完成 |
| `objective` | `string` | 否 | 目标描述（create/replace 必填）；应可验证、含明确交付物 |
| `token_budget` | `integer` | 否 | 目标级 token 预算（可选；缺省沿用会话 [goals] 预算） |

### `task`

委派子任务给独立子 Agent（独立上下文，不继承当前对话），返回其最终结果。支持单任务（task）或并行多任务（tasks，按并发护栏并行）。单任务可给 output_schema（JSON Schema）要求结构化 JSON 输出——校验失败自动带纠错反馈重试，成功返回紧凑 JSON。适合并行处理或分片复杂任务。

- 分组：宿主 · 能力：`read_only` · 启用：恒开

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `task` | `string` | 否 | 单个子任务描述（须自包含，含必要上下文；与 tasks 二选一） |
| `tasks` | `array` | 否 | 多个可并行的子任务描述（与 task 二选一；按并发护栏并行执行后聚合结果） |
| `agent` | `string` | 否 | 命名子代理（缺省 task = 通用全工具）。可用名与各自能力见 system prompt 的 <subagents> 段；只读代理无写/执行工具。 |
| `output_schema` | `object` | 否 | typed 输出契约（仅与 task 单任务联用）：JSON Schema（type/properties/required/items/enum/界限等子集）。给出时子 Agent 须只输出一个符合 schema 的 JSON 值，校验通过后父级收到紧凑 JSON |
