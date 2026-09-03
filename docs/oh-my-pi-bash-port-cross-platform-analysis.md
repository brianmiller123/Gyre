# oh-my-pi bash 工具移植与跨平台一致性分析报告

日期：2026-09-02
范围：`third/oh-my-pi` bash 工具族 → Gyre（`crates/tools`、`crates/shell`、`crates/pty`、`vendor/*`）的移植完整度与 Linux/macOS/Windows（Git Bash / WSL / Cygwin / PowerShell 场景）行为一致性。
方法：四路并行代码审计（TS 基线盘点、Gyre 侧盘点、vendor↔上游 diff 分类、vendor 跨平台审计）+ 主线代码交叉核验。所有结论附 file:line。

---

## 1. 架构关系：移植的本质

`oh-my-pi` 的 bash 能力分三层，Gyre 对三层的移植程度不同：

```
┌─ TS 工具层  packages/coding-agent：bash.ts(1780 ln) + exec/bash-executor.ts(776 ln)
│             + session/{streaming-output,bash-runner}.ts + tools/{bash-interceptor,
│             shell-tokenize,bash-interactive,bash-skill-urls}.ts + prompts/tools/bash.md
│
├─ Rust 引擎层 crates/{pi-shell, pi-builtins, pi-walker, vendor/brush-core}
│             brush 进程内 shell + 125 个内建模块 + 取消/进程树 + 输出 minimizer
│             （TS 层的实际执行体：命令不 spawn bash，直接在内嵌 brush 里跑）
│
└─ Gyre 移植层 crates/tools/src/shell.rs (run_command)
              crates/shell (agent-shell 门面) → vendor/pi-shell + vendor/pi-builtins + vendor/brush-core
              crates/pty (run_pty_command) + crates/tools/{intercept,minimizer,security_scan}.rs
```

关键事实（决定后文一切结论）：

1. **上游 bash 工具不依赖系统 shell**。命令在进程内嵌 brush-core 执行；`resolveWindowsShell`（procmgr.ts:130-176）解析出的 bash.exe 仅服务 spawn-a-shell 场景（交互 PTY、ACP 终端、`SHELL` env），且注释明确 **"Never wrap in cmd.exe"**（bash-executor.ts:493）。
2. **Gyre `run_command` 是双引擎**：命中内建表的命令走进程内 brush（vendor 栈，与上游同一引擎）；其余走子进程——Unix `/bin/sh -c`、Windows `cmd /C`（shell.rs:391-419）。**子进程引擎是 Gyre 自创路径，上游不存在**，也是跨平台差异的最大来源。
3. **vendor 四 crate = 上游 18.0.0 时代快照的纯格式化重排，零 Gyre 本地补丁**（272 个 .rs 文件规范化等价校验，仅 pi-builtins/jq.rs 一处依赖代差）。上游已演进到 18.1.2（windows.rs GitDiscovery DI、xutf 迁移、aarch64 hash asm、自足进程测试）。vendor 已随 `ffc88c5` 入库（415 文件，`.gitignore:22-26` 精确放行），先前基准报告中的 "fresh clone 断链" P0 已修复。

---

## 2. 上游 bash 工具功能清单与覆盖对照表

图例：✅ 完整/等价实现 · 🟡 部分实现 · ❌ 未实现
（上游位置以 `packages/coding-agent/src` 为基；Gyre 位置以 crate 相对路径）

| # | 功能 | 上游位置 | Gyre 状态 | Gyre 位置 / 差异 |
|---|------|---------|-----------|------------------|
| 1 | 内嵌 brush 执行引擎（进程内，不 fork shell） | exec/bash-executor.ts | ✅ | crates/shell → `pi_shell::execute_shell_streams`（lib.rs:103-122），与上游同引擎 |
| 2 | 进程内内建工具表（POSIX/bash 内建 + 50+ coreutils，125 模块） | crates/pi-builtins（同一份代码） | ✅ | vendor/pi-builtins 125 文件与上游集合一致；`BashMode+utility+process` 全注册（crates/shell/src/lib.rs:196-215） |
| 3 | 内建开关 env 门 | `PI_DISABLE_UUTILS_BUILTINS`（bash.ts:156-163） | ✅ | `GYRE_DISABLE_INPROC_BUILTINS`（shell.rs:345-348），语义等价 |
| 4 | 危险内建 withholding（rm/mv/ln 不进程内执行） | —（上游进程内含 rm/mv/ln，见 bash.md:11） | ➕ 超出上游 | `WITHHELD_UTILITIES`（crates/shell/src/lib.rs:191）走系统二进制，安全取向更强 |
| 5 | 命令意图拦截器（cat/grep/find→专用工具） | tools/bash-interceptor.ts (148 ln) | ✅ | crates/tools/src/intercept.rs：7 条默认规则 + `cd X &&` 前缀归一化防绕过（:118-135）+ 引号感知重定向扫描（:144-225）；RE2 无环视 → 手写扫描器替代（:8-10 已注释） |
| 6 | 审批门禁（allow/deny/ask + 元字符守卫） | bash.ts:219-312, config rules | 🟡 | crates/config/src/rules.rs:396-408 有元字符守卫 + yolo 优先级；但缺上游的分段（`;`/`&&`/`\|\|`/管道）tokenizer 审批与「allow 须为整条命令背书」的引号感知校验（bash.ts:66-124） |
| 7 | 危险命令硬拦截清单（rm -rf /、fork bomb、dd、mkfs、curl\|bash…） | bash.ts:172-215 `CRITICAL_BASH_PATTERNS` | ❌ | Gyre 仅有 config 示例 deny globs（config.rs:1418-1422），无内置硬清单 |
| 8 | shell tokenize（tree-sitter 级分段） | tools/shell-tokenize.ts (311 ln) | 🟡 | 无 tokenizer；regex + 手写引号感知扫描近似替代（intercept.rs:144-225） |
| 9 | 超时机制（默认 300s，clamp 1–3600，`timeout:0`=无限） | tool-timeouts.ts, bash.ts:995-1004 | 🟡 | 固定 `CMD_TIMEOUT=120s`（shell.rs:386），无参数、无 0=无限语义；进程内路径同 120s（shell.rs:281） |
| 10 | 持久会话（cwd/env/exports/functions/aliases 跨调用保持；会话键 + 互斥 + 隔离检疫） | bash-executor.ts:143-208, 520-600 | ❌（工具层）/ 🟡（库层） | `InProcShell` 明确 oneshot "无会话状态"（crates/shell/src/lib.rs:69-71）；vendor 的持久 `Shell`+snapshot API 在库内但 `snapshot_path: None` 硬关（lib.rs:113）；`PtyShell` 持久会话已实现但注册工具是一次性 `run_pty_command` |
| 11 | shell 快照（source ~/.bashrc/zshrc 捕获函数/别名/PATH；win32 跳过） | utils/shell-snapshot.ts | ❌ | snapshot_path 恒 None |
| 12 | NON_INTERACTIVE_ENV 基线（pagers→cat、TERM=dumb、NO_COLOR、编辑器→true、GIT_TERMINAL_PROMPT=0、包管理器非交互…） | exec/non-interactive-env.ts:6-47 | ❌ | Gyre 仅注入 locale（见 #14） |
| 13 | direnv 集成（.envrc 上溯 + export json + allow-list） | exec/direnv.ts, bash-executor.ts:74-140 | ❌ | 无 |
| 14 | 编码/UTF-8 强制 | Windows-only：LANG/LC_ALL=C.UTF-8 + PYTHONUTF8（non-interactive-env.ts:50-58，大小写不敏感键检查 :60-79）；POSIX 不强制 | ➕ 分歧（增强） | Unix 侧也强制 C.UTF-8（shell.rs:397-402 + core/platform.rs:46-82）；Windows 侧 `chcp 65001` 包装 + GBK 兜底解码（shell.rs:355-383）。比上游激进，行为不同于上游 |
| 15 | 输出管线（3000 行 / 50KiB / 512 列，head+tail 中段省略，UTF-8 边界安全，artifact 无损外溢，CR/CRLF→LF 三层归一，sixel 门控，notices 体系） | session/streaming-output.ts (1503 ln), output-meta.ts | 🟡 | 256KiB 硬截断 + UTF-8 字符边界回退（shell.rs:250-258）；无 head+tail、无 artifact 外溢、无列上限、无 notices；子进程路径无 CRLF 归一化（PTY 路径有：pty/src/session.rs:384-401 ✅） |
| 16 | 输出 minimizer（git/cargo/go/docker/gh/jq/js/python…约 25 个过滤模块） | vendor/pi-shell/src/minimizer/（上游在用，bash-executor.ts:237-250） | 🟡 | 自研 5 过滤器（git status/diff/log、cargo、python，minimizer.rs:70-80）；vendor 引擎在库但 `minimizer: None` 硬关（crates/shell/src/lib.rs:114） |
| 17 | 后台执行 / auto-background（60s 阈值转后台 + 作业管理 + 尾部预览）/ `async` 参数 | async/auto-background.ts, bash.ts:1042-1102 | ❌ | 工具 schema 仅 `command`（shell.rs:53-61）；无作业返回/轮询机制 |
| 18 | PTY 执行 | `pty:true` 交互叠加层（bash-interactive.ts：xterm 无头、kitty 输入归一、Esc 强杀） | 🟡 | `run_pty_command`（crates/pty，opt-in 注册）：一次性捕获、marker 协议取退出码、8MiB 上限、CRLF+ANSI 归一；无交互叠加层；Windows 用 `cmd`（session.rs:417），上游走 Git Bash 发现链 |
| 19 | 工具接口参数（`cwd`/`env`/`timeout`/`pty`/`async`） | bash.ts:297-332 | ❌ | 仅 `command`；cwd 固定 workspace.root（shell.rs:121）；`cd X &&` 也不会被提升为结构化 cwd（上游 shell-tokenize.ts:212-311 有 cd-hoist） |
| 20 | 内部 URL 展开（skill:// agent:// artifact:// …→ 转义路径，含遍历防护） | tools/bash-skill-urls.ts | ❌ | run_command 无此预处理 |
| 21 | gh 缓存联动（bash 内变更 gh issue/PR → 失效 github 缓存） | tools/gh-cache-invalidation.ts | ❌ | Gyre 有 github 工具（tools/src/github.rs）但无 bash 钩子联动 |
| 22 | eval 工具（内联 JS/Python 脚本，承接 heredoc/复杂控制流） | tools/eval.ts (1010 ln) | ❌ | Gyre 注册表无 eval 工具（lib.rs:9-31） |
| 23 | 退出码/错误约定 | 非零→isError 结果 + `Command exited with code N`；超时→`[Command timed out after N seconds]` | ✅（自有体系，自洽） | `[exit N]` 前缀（shell.rs:163-168）、`(命令成功，无输出)`（:153-158）、超时/取消为 ToolError 文本（:131-142）；两引擎输出格式对齐（:322-342） |
| 24 | Windows shell 解析（Git Bash 发现链：ProgramFiles Git/scoop/GIT_INSTALL_ROOT/PATH bash.exe/sh.exe→cmd 兜底） | procmgr.ts:130-176 | ❌ | 无发现链：子进程恒 `cmd`（shell.rs:406）、PTY 恒 `cmd`（session.rs:417）；Git Bash/MSYS2/Cygwin bash 完全不可达 |
| 25 | 提示词契约（bash.md：内建清单、平台差异、async 语义） | prompts/tools/bash.md | 🟡 | platform_section 给出按 OS 的 shell 指引（prompt/src/lib.rs:55-93），但 Windows 宣称 PowerShell 而 `run_command` 实跑 cmd（见 G1）；无内建清单注入模型提示 |

**统计**：✅ 9 项 · 🟡 8 项 · ❌ 8 项（另 1 项超出上游）。引擎与内建层移植完整；**工具接口层、会话层、输出管线层是主要缺口**；vendor 层完整但落后上游一个版本窗。

**已核实不属于 bash 工具族**（不计入差距）：上游 security-scan.ts（安全运营编排工具，零 shell 引用）、file-write-fallback.ts（沙箱写代理）、run-scope.ts（浏览器 eval 承诺域）。Gyre 的 `security_scan` 是自有的工作区密钥扫描工具，与上游同名模块无对应关系。

---

## 3. 跨平台差异清单

### 3.1 Gyre 自身层（移植引入的差异，按危害排序）

| # | 位置 | 平台 | 等级 | 差异与后果 |
|---|------|------|------|-----------|
| G1 | prompt/src/lib.rs:58-68 vs tools/shell.rs:404-415 | Windows | **高** | **引擎矛盾**：系统提示词告诉模型「默认 Shell：PowerShell」，并教 `$env:VAR`、`Get-ChildItem`；实际 `run_command` 用 `cmd /C` 执行。PowerShell 语法在 cmd 下失败，cmd 语法（`%VAR%`）模型又没被告知。上游明确不落 cmd（bash-executor.ts:493），全部命令走内嵌 brush 的 POSIX 语义 |
| G2 | tools/shell.rs:392-395 | Linux(Debian/Ubuntu) | **高** | `/bin/sh -c` 在 Debian 系是 dash：数组、`[[ ]]`、`==`、`$'…'`、`local` 外用等 bash 语法直接失败。上游从不经系统 sh——同一命令在内嵌 brush（bash 兼容）恒可执行。模型按 bash.md 风格写命令 → 平台相关失败 |
| G3 | tools/shell.rs:394（硬编码） | Unix | 中 | 无 shell 覆盖手段。上游有 shellPath 设置层（settings.ts:955-968）+ `$SHELL`/bash→sh 探测链（procmgr.ts:89-110, 206-213） |
| G4 | pty/src/session.rs:410-418 | Windows | 中 | PTY 引擎恒 `cmd`。上游 Git Bash 发现链 + cmd 末位兜底（procmgr.ts:130-176）；Git Bash/MSYS2/Cygwin 的 bash 不可达，`sudo`/`ssh` 等交互场景体验断裂 |
| G5 | tools/shell.rs:404-414 | Windows | 中 | `chcp 65001` 只对查询控制台代码页的程序生效，写管道的程序各自为政；`&&` 前置包装使命令串被 cmd 二次解析，引号/特殊字符（`^`、`&`、`%`）转义语义与 bash 完全不同。上游对应物是 env 注入（PYTHONUTF8/LANG/LC_ALL，带 win32 大小写不敏感键检查，non-interactive-env.ts:60-79），Gyre 的 `c.env()` 注入不做大小写共存检查（G5b，低概率踩 `Path`/`PATH`） |
| G6 | tools/shell.rs:239-249 | Windows 子进程路径 | 中低 | 合并输出无 CRLF 归一化（PTY 路径已归一 ✅，session.rs:384-401）：cmd 内建（echo/dir）输出含 `\r\n`，模型侧可见 `\r`；上游在 OutputSink/PTY 捕获/会话键三层归一（streaming-output.ts:818-851） |
| G7 | tools/shell.rs:360-383 | Windows | 低 | GBK 启发式兜底：非 UTF-8 且非 GBK 字节（如 Shift-JIS）误判面；上游恒 lossy UTF-8。属有意增强，但两侧行为不同 |
| G8 | tools/security_scan.rs:195-215 | Windows | 低 | 私有文件权限检查 `#[cfg(unix)]` 静默跳过——功能性降级但已文档化 |
| G9 | config/src/rules.rs:396-408 | Windows | 低 | 审批 glob 元字符守卫按 bash 语义（`$`、反引号等）；cmd/PowerShell 元字符（`%VAR%`、`` ` ``、`|` 已覆盖部分）识别不全，allow 规则在 Windows 语义下判定口径不同 |
| G10 | tools/shell.rs:416-419 | 其他目标 | 信息 | 非 unix/windows 显式 `compile_error!`——失败显式，优于静默 |

WSL：全链路（Gyre 与上游）均无 WSL 检测/互操作分支（vendor 审计确认）。Gyre 以 Linux 二进制跑在 WSL 内时行为=Linux；以 Windows 二进制互操作调 `wsl.exe` 的场景两侧都不支持——属共同边界而非移植差距。

### 3.2 vendor 继承层（内嵌 brush 路径，代码与上游相同 → 上游同样存在；Windows 面最重）

合计 **38 项**（高危 7 / 中危 12 / 低危 19），完整清单见审计附录（file:line 均已核）。关键项：

| 位置 | 平台 | 等级 | 后果 |
|------|------|------|------|
| brush-core/src/sys/stubs/signal.rs:46-152 + pi-shell/src/process.rs:986-997 | Windows | **高** | 一切信号= `TerminateProcess(handle,1)`：无优雅 TERM 窗口、无进程组、退出码恒 1（非 128+N）；取消/超时全部硬杀 |
| pi-builtins/src/{kill.rs:237-263, proc_snapshot.rs:952-976} | Windows | **高** | `kill -TERM/-KILL/-STOP` 语义全同（信号类型被丢弃）；`kill -0` 之外的探测性用法失真 |
| pi-builtins/src/host.rs:924-956 | Windows | **高** | 进程替换 `<(...)` 仅 Unix 物化：`sort <(cmd)` 等在 Windows 收到字面 `/dev/fd/63` 必败 |
| brush-core/src/commands.rs:377-440 + sys/windows/commands.rs | Windows | **高** | 全栈无 shebang/解释器回退：`./script.sh` 在 Windows 直接 `CreateProcess` 失败（os error 193 → exit 126）。脚本不可直接执行 |
| brush-core/src/sys/windows/commands.rs:121-128 | Windows | 中 | `nohup cmd &` 的分离为 no-op 且会话销毁时被杀——后台服务活不过会话，违反上游文档承诺的 nohup 契约 |
| 其余中危 | Windows | 中 | 作业控制 stub（Ctrl+Z/fg 不可用）；fd>2 注入拒绝（`3>file`）；`/dev/stdout\|stderr\|tty` 未映射（仅 `/dev/null`→`NUL`）；UNC glob 根丢弃 `//server/share`；PATHEXT 首次使用后缓存；env 大小写敏感 HashMap（`export path=` 与 `PATH` 共存且子进程胜者不确定）；`exec`/`ulimit`/`umask`/`suspend` unix-gated（Windows 127）；trap 仅 TERM/KILL/INT 三信号；`ps/pgrep/pkill/top` 元数据大面积空白；`read -t` 直接 Unsupported |
| 跨 Unix 差异 | Linux/macOS | 低 | `kill -N` 合法范围按 OS 不同（Linux SIGRTMAX vs macOS 0..=31 vs 其他 0..=64，kill.rs:680-686）；`top` 两侧方言不同（top.rs:117-144）；`ps -o args` 形状差异（Windows 单串 vs argv NUL 分割） |

**归因**：这一层是 vendored 上游代码的固有平台差异，不是移植引入的；但它决定了 Gyre 内嵌路径（Windows 上的主路径）与 Linux 体验的落差上限。上游 TS 层之所以体验尚可，是因为其持久会话/输出管线在 TS 层做了大量补偿；Gyre 恰好尚未移植这些补偿层。

---

## 4. 潜在兼容性风险评级

| 风险 | 等级 | 触发条件 | 影响 |
|------|------|---------|------|
| R1 Windows 语义混乱（G1+G5）：模型被教 PowerShell、实跑 cmd、语法规则不明 | **P0** | 任何 Windows 会话执行命令 | 命令高频失败/误执行；审批判定口径漂移（G9）；与上游「POSIX 语义恒定」的核心卖点相反 |
| R2 Unix dash bashism（G2） | **P0** | Debian/Ubuntu 容器/服务器（最常见部署面）执行含 bash 语法的命令 | 失败模式平台相关：本机（bash 兼容 sh）通过、部署机失败，难复现 |
| R3 长任务不可完成：无 `timeout` 参数且 120s 上限（#9/#19） | **P1** | 构建、测试套件、安装 | 上游 300s 默认/3600s 上限/0=无限；Gyre 场景直接超时失败且无解 |
| R4 会话缺失（#10/#11/#13） | **P1** | 多步骤工作流（export→使用、cd→构建、activate venv→pip） | 每次调用状态归零；模型被迫拼单条超长命令，而单条命令又受限 120s/256KiB（与 R3 复合） |
| R5 Windows 内嵌路径硬边界（vendor 高危簇：信号/进程替换/nohup/脚本执行） | **P1** | Windows 下取消长命令、后台服务、`<( )`、直接跑脚本 | 取消即硬杀、后台服务死亡、脚本不可执行；上游同代码同样存在，但上游 TS 层持久会话补偿在 Gyre 缺位使暴露面更大 |
| R6 输出管线薄弱（#15）：256KiB 一刀切截断，无 head+tail | **P2** | 冗长构建/测试输出 | 截断砍掉的是**尾部**——恰是错误摘要所在；上游保留头尾并给 artifact 兜底 |
| R7 minimizer 覆盖 5/25（#16） | **P2** | git/ cargo /python 之外的冗长命令（gh、docker、go、js 工具链） | 上下文浪费；vendor 引擎已在库内却未接线 |
| R8 vendor 落后上游 18.0.0→18.1.2（#3.2） | **P2** | — | 零本地补丁 → 可无痛快进；上游 windows.rs GitDiscovery DI 对 G4 修复有直接价值 |

正面确认（无需处理）：PTY 输出已做 CRLF+ANSI 归一（session.rs:384-401）；非支持平台显式编译失败（shell.rs:416-419）；`rm/mv/ln` 不进程内执行（超出上游的安全取向）；vendor 已入库、fresh clone 可构建（ffc88c5，415 文件）。

---

## 5. 改进方案（优先级排序 · 工作量为人日估算）

### P0 — 统一执行引擎（消除 R1/R2，最高杠杆）
1. **子进程路径退役为兜底，内嵌 brush 成为唯一主引擎**：`run_command` 无条件走 `pi_shell::execute_shell_streams`（外部件命令由 brush 自身按 PATH spawn，本就是 vendor 栈的既有能力）；`/bin/sh -c` 与 `cmd /C` 仅在 `GYRE_DISABLE_INPROC_BUILTINS=1` 或 brush 失败时兜底。效果：三平台同一套 POSIX/bash 语义、退出码与输出字节路径；G1/G2/G3/G5/G9 一次性消解大半。
   改动：shell.rs 引擎分派（~100 ln）+ 测试。**3–5d**。
2. **platform_section 改口**：Windows 段从 PowerShell 教学改为「内嵌 bash 兼容 shell，POSIX 语法」（prompt/src/lib.rs:58-68）。**0.5d**。
3. **Windows PTY shell 发现链**：vendor 刷新后直接复用上游 `GitDiscovery`（windows.rs:96-105，18.1.2）顺序：Git Bash → PATH bash.exe/sh.exe（Cygwin/MSYS2 天然覆盖）→ cmd 兜底。**1d**。
4. **子进程路径 CRLF 归一化**：`combine_capped`/`combine_inproc` 解码后统一 `\r\n`→`\n`（对齐 PTY 路径既有行为）。**0.5d**。
5. **vendor 快进到上游 18.1.2**：零本地补丁，纯 pull + rustfmt；顺带取得 GitDiscovery DI、xutf、aarch64 asm。**0.5–1d**。

小计 **≈ 1 周**。验收：同一命令脚本（含 bash 语法、管道、进程替换、非 ASCII 输出）在 Linux/macOS/Windows 三端输出逐字节一致（现有 crates/shell/tests/inproc.rs 10 例可直接扩展为三端矩阵）。

### P1 — 接口与会话（消除 R3/R4）
6. **工具参数补齐**：`cwd`（stat 校验 + 越界策略）、`env`（`^[A-Za-z_][A-Za-z0-9_]*$` 校验，对齐上游 bash.ts:375-388）、`timeout`（clamp 1–3600s，`0`=无限；接入既有 `CancelToken::with_timeout`）。**1–2d**。
7. **持久会话**：按 workspace+env 键控常驻 brush 会话（vendor `Shell` API 已在库，打开 `snapshot_path` 通道）；占用互斥 + 失败检疫（上游 bash-executor.ts:143-208 模式）；`cd` 持久化。win32 跳过 rc 快照（对齐上游）。**3–5d**。
8. **NON_INTERACTIVE_ENV 基线移植**（pagers/TERM=dumb/NO_COLOR/编辑器→true/包管理器非交互；win32 键大小写不敏感检查）——常量表平移，风险低收益高。**1d**。
9. **输出 head+tail + 截断通知**：中段省略（头部 60%/尾部 25%），`[Showing lines X-Y of N]` 样式通知；UTF-8 边界安全已有。artifact 外溢可后置。**2–3d**。
10. **CRITICAL_BASH_PATTERNS 硬清单**：bash.ts:172-215 的 20+ 模式平移为 deny 前置层（config deny globs 之上）。**1–2d**。

小计 **≈ 1.5–2 周**。

### P2 — 深度对齐（消除 R6/R7/R8）
11. **minimizer 换用 vendor 引擎**：把 `crates/shell` 的 `minimizer: None` 接到 `pi_shell::minimizer`（25 过滤器免费获得），自研 5 过滤器退役或保留为 fallback。**1–2d**。
12. **审批 tokenizer 化**：以 brush-parser（已在 vendor 依赖树）做分段审批 + 「allow 须整条背书」引号感知校验，替代 regex+手写扫描器。**2–3d**。
13. **internal URL 展开 + gh 缓存联动**（#20/#21）：依赖 Gyre 自身 internal-URL 协议成熟度，条件性。**2–3d**。
14. **auto-background/作业管理**（#17）：需作业管理器与事件回投（可评估复用 crates/supervisor），**5–8d，建议单独立项**。
15. **eval 工具**（#22）：与 bash 无关但有同样跨平台收益，独立立项。

### 明确不做（与上游共同边界）
WSL 互操作、Windows shebang 直接执行（内核层缺失， brush 已给出 126 语义）、非 unix/windows 目标。

---

## 6. 结论

- **引擎与内建层移植是完整的**：vendor 四 crate 与上游同源（18.0.0-era，零本地补丁），进程内路径在 Linux/macOS 上与上游体验等价；`run_command` 的拦截器、内建开关、退出码约定均为忠实移植且部分增强（危险内建 withholding、Unix UTF-8 强制、PTY 输出归一）。
- **差距集中在 Gyre 自创的子进程引擎与未移植的工具层补偿**：Windows `cmd /C` + PowerShell 提示词的自相矛盾（G1）与 Unix `/bin/sh` dash 化（G2）是仅有的两个 P0——两者都随「内嵌 brush 唯一主引擎」的 P0 方案消解；会话、超时参数、输出管线构成 P1 主体。
- **vendor 层的 Windows 硬边界**（信号硬杀、进程替换、nohup、脚本执行）上游同样存在，属共同边界；Gyre 的正确策略是随 P0 把更多命令收进内嵌路径并快进 vendor，而非在子进程层重复造平台分支。

---

## 7. 修复状态（2026-09-02 实施）

本节记录上文改进方案的落地情况。

### 已实施（P0 全部 + P1 大部）

| 方案项 | 状态 | 实现 |
|--------|------|------|
| P0-1 统一执行引擎 | ✅ | `crates/tools/src/shell.rs`：进程内 brush 为唯一主引擎；子进程（`GYRE_SHELL` 可覆盖）仅兜底（withheld 三件套 / `GYRE_DISABLE_INPROC_BUILTINS` / 引擎初始化失败）。进程内会话环境注入 `PI_DISABLE_UUTILS_DESTRUCTIVE=1`，管道内嵌 rm/mv/ln 同样回退系统二进制 |
| P0-2 platform_section 改口 | ✅ | `crates/prompt/src/lib.rs`：三平台统一宣称「bash 兼容内嵌 shell（POSIX 语法）」，Windows 明确禁用 PowerShell 语法 |
| P0-3 Windows PTY shell 发现链 | ✅ | `crates/pty/src/session.rs::windows_shell_program`：`GYRE_SHELL` → `GIT_INSTALL_ROOT` → Program Files/MinGit → scoop → LocalAppData → PATH `bash.exe`/`sh.exe` → `cmd` 兜底 |
| P0-4 CRLF 归一化 | ✅ | `normalize_newlines`：`\r\n`/裸 `\r` → `\n`，两条引擎路径统一应用 |
| P1-6 cwd/env/timeout 参数 | ✅ | schema 扩展 + `parse_cwd`/`parse_env_overrides`/`parse_timeout`（`0`=不限时，clamp 1–3600s） |
| P1-8 NON_INTERACTIVE_ENV | ✅ | `NON_INTERACTIVE_BASE`（40 项）+ `GYRE_BASH_NO_CI` 退出 + SSH_ASKPASS 按 PATH 探测；Windows UTF-8 组大小写不敏感 has-check、`env` 参数先摘除大小写变体 |
| P1-9 head+tail 中段省略 | ✅ | `elide_middle`：头 60% + 尾 25%，UTF-8 边界 + 行首对齐，附省略字节提示 |
| P1-10 CRITICAL 清单 | ✅ | `CRITICAL_PATTERNS` 21 条（无环视依赖的等价移植），先于意图拦截执行 |

测试：`agent-tools` 177 项（含引擎统一 bash 语法直证、超时/取消、参数解析、危险清单正反例、省略/归一化）+ `agent-shell`/`agent-prompt`/`agent-pty` 共 211 项全绿；workspace 编译零告警，rustfmt/clippy（CI 执法门）干净。

### 暂缓（明确决策，非遗漏）

- **P1-7 持久会话**：评估结论——vendor `Shell` 的会话 API 可用，但接线涉及会话键控、互斥、失败检疫与 rc 快照四件套（上游对应实现约 3–5 人日量级），本批次未接线；每次调用仍为 oneshot。作为独立批次实施。
- **P0-5 vendor 快进 18.1.2**：零本地补丁可无痛 pull，但引入新依赖（xutf、aarch64 asm）需同步 workspace 依赖图；其 GitDiscovery DI 对 G4 的增益已由 P0-3 自行实现覆盖，暂缓。
- **P2 项**（minimizer 换 vendor 引擎、审批 tokenizer 化、internal URL/gh 联动、auto-background、eval 工具）：维持原优先级排序，未动。

### 对风险表的消解

- R1（Windows 语义混乱）：消除（引擎统一 + 提示词改口）。
- R2（dash bashism）：消除（`inproc_runs_bash_syntax_unified` 测试直证数组/`[[ ]]` 可用）。
- R3（长任务 120s 死线）：消除（`timeout` 参数 + `0`=不限时）。
- R6（截断吃掉错误摘要）：消除（head+tail 省略）。
- R4（会话缺失）：部分缓解（`cwd`/`env` 参数减少状态拼接需求），持久会话待独立批次。
- R5（vendor Windows 硬边界）：维持（上游共同边界，策略=快进 vendor 而非本地补丁，见暂缓项）。
- R7（minimizer 覆盖）/ R8（vendor 落后）：维持（P2 / P0-5 暂缓，理由见上）。
