//! 内嵌 JavaScript（Node 运行时）驱动：`node --input-type=module -e <driver>` 启动的持久内核。
//!
//! 协议与 [`crate::PYTHON_DRIVER`] 完全同构（NDJSON 同帧格式）：宿主经 stdin 逐行写请求
//! `{"id":N,"code":"…","token":"…"}`；内核经 stdout 逐行写回 `ready` / `stdout` / `stderr` /
//! `result` / `error` 帧。驱动源码保持短小（约 4KB，远低于单参数 128KiB 上限），可安全经
//! `-e` 命令行传入。
//!
//! 结果语义（与 Python 驱动对齐）：
//!
//! - 同步代码取脚本完成值（末表达式，REPL 语义），声明（`var`/`let`/`const`/`function`/`class`）
//!   在共享 vm 上下文中跨 run 持久；
//! - 含顶层 `await`/`return` 的代码（同步编译抛 `SyntaxError`）自动包一层 async IIFE，
//!   显式 `return` 产生结果，状态经 `globalThis` 共享；
//! - 表达式值为 Promise 时自动 await；`undefined`/`null` 输出 `null`，字符串加引号，其余
//!   JSON 序列化（近似 Python `repr`）。
//!
//! 内置全局 `tool` 代理：`tool.call(name, args)` 或 `tool.<name>(args)` 经 `GYRE_BRIDGE_URL`
//! + 当次请求帧下发的 token POST /call 回调宿主工具（401 时给出「桥未启用/过期」中文提示）；
//!   `tool.reset()` 重建上下文（清空命名空间）。`console.log/info/debug` → stdout 帧，
//!   `console.warn/error/trace` → stderr 帧。

/// JavaScript 持久内核驱动源码（作为 `node --input-type=module -e <driver>` 的代码参数传入）。
pub const JS_DRIVER: &str = r#"// Gyre eval 持久 JavaScript 内核驱动（Node 运行时）。
//
// 经 stdin 逐行读取 NDJSON 请求（{"id":N,"code":"...","token":"..."}），在共享 vm 上下文
// 中执行用户代码，经 stdout 逐行写回与 Python 内核同构的 NDJSON 帧。内置全局 tool 代理
// 回调宿主工具（tool.call(name, args) / tool.<name>(args)）。

import readline from 'node:readline';
import util from 'node:util';
import vm from 'node:vm';

// 环回桥地址与当前 run 的 token（token 随请求帧按 run 轮换）。
const BRIDGE_URL = (process.env.GYRE_BRIDGE_URL || '').replace(/\/+$/, '');
let token = process.env.GYRE_BRIDGE_TOKEN || '';
// 当前请求 id（供 console 捕获逐条打帧）。
let currentId = null;
// 共享 vm 上下文（tool.reset() 时重建）。
let context = null;

function emit(frame) {
    process.stdout.write(JSON.stringify(frame) + '\n');
}

function fmt(value) {
    if (typeof value === 'string') return value;
    try {
        return util.inspect(value, { depth: 4 });
    } catch {
        return String(value);
    }
}

async function callTool(name, args) {
    if (!BRIDGE_URL) throw new Error('桥未启用：GYRE_BRIDGE_URL 未设置');
    let resp;
    try {
        resp = await fetch(BRIDGE_URL + '/call', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + token },
            body: JSON.stringify({ token, tool: name, args: args ?? {} }),
        });
    } catch (err) {
        throw new Error('桥调用失败：' + fmt(err));
    }
    if (resp.status === 401) {
        throw new Error(
            '桥未启用/过期：当前 eval 运行的桥 token 无效或已注销（HTTP 401），请确认 eval 工具仍在执行中'
        );
    }
    if (!resp.ok) {
        throw new Error('桥调用失败：HTTP ' + resp.status + ' ' + resp.statusText);
    }
    let data;
    try {
        data = await resp.json();
    } catch (err) {
        throw new Error('桥调用失败：' + fmt(err));
    }
    if (data && data.ok) return data.output;
    let err = data === null || data === undefined ? undefined : data.error;
    if (err !== null && typeof err === 'object') {
        err = err.message || err.error || JSON.stringify(err);
    } else if (typeof err !== 'string') {
        err = '未知错误';
    }
    throw new Error('工具 ' + name + ' 执行失败：' + err);
}

const tool = new Proxy(
    {
        call: (name, args) => callTool(String(name), args ?? {}),
        reset: () => {
            freshContext();
            return '命名空间已重置';
        },
    },
    {
        get(target, prop) {
            if (typeof prop !== 'string' || prop.startsWith('_')) return undefined;
            if (Object.prototype.hasOwnProperty.call(target, prop)) return target[prop];
            return async (args) => callTool(prop, args ?? {});
        },
    }
);

function makeConsole() {
    const write = (kind) => (...args) => {
        if (currentId !== null) {
            emit({ type: kind, id: currentId, text: args.map(fmt).join(' ') + '\n' });
        }
    };
    const out = write('stdout');
    const err = write('stderr');
    return { log: out, info: out, debug: out, warn: err, error: err, trace: err };
}

// 持久内核中用户代码不可读 stdin（协议管道被 readline 占用）。
function guardProcess() {
    const guarded = Object.create(process);
    Object.defineProperty(guarded, 'stdin', {
        get() {
            throw new Error('持久内核中不支持读取 stdin');
        },
    });
    return guarded;
}

function freshContext() {
    context = vm.createContext({
        tool,
        console: makeConsole(),
        process: guardProcess(),
        setTimeout,
        clearTimeout,
        setInterval,
        clearInterval,
        setImmediate,
        clearImmediate,
        queueMicrotask,
        fetch,
        URL,
        URLSearchParams,
        TextEncoder,
        TextDecoder,
        Buffer,
        structuredClone,
    });
}

// 顶层 await/return 特征：同步编译抛此类 SyntaxError 时改走 async IIFE 包裹。
const ASYNC_HINT = /await|Illegal return|yield/;

async function runCell(code) {
    let script;
    try {
        script = new vm.Script(code, { filename: 'cell.js' });
    } catch (err) {
        if (err instanceof SyntaxError && ASYNC_HINT.test(err.message)) {
            const wrapped = new vm.Script('(async () => {\n' + code + '\n})()', {
                filename: 'cell.js',
            });
            return await wrapped.runInContext(context);
        }
        throw err;
    }
    const value = script.runInContext(context);
    if (value !== null && typeof value === 'object' && typeof value.then === 'function') {
        return await value;
    }
    return value;
}

// 近似 Python repr：undefined/null → null，字符串加引号，对象 JSON 序列化，其余 String。
function repr(value) {
    if (value === undefined || value === null) return null;
    if (typeof value === 'string') return JSON.stringify(value);
    if (typeof value === 'object') {
        try {
            return JSON.stringify(value);
        } catch {
            return String(value);
        }
    }
    return String(value);
}

// 堆栈截断：前后各保留 30 行，中间省略（与 Python 驱动一致）。
function truncateStack(text) {
    const lines = String(text).replace(/\n+$/, '').split('\n');
    if (lines.length <= 60) return lines.join('\n');
    const omitted = lines.length - 60;
    return lines
        .slice(0, 30)
        .concat(['......（中间省略 ' + omitted + ' 行）......'], lines.slice(-30))
        .join('\n');
}

async function handleLine(raw) {
    const line = raw.trim();
    if (!line) return;
    let req;
    try {
        req = JSON.parse(line);
    } catch {
        return;
    }
    if (req === null || typeof req !== 'object') return;
    const rid = req.id;
    if (rid === null || rid === undefined) return;
    if (typeof req.token === 'string') token = req.token;
    const code = typeof req.code === 'string' ? req.code : '';
    currentId = rid;
    try {
        const value = await runCell(code);
        emit({ type: 'result', id: rid, ok: true, output: repr(value) });
    } catch (err) {
        const message = err !== null && typeof err === 'object' && err.stack ? err.stack : fmt(err);
        emit({ type: 'error', id: rid, message: truncateStack(message) });
    } finally {
        currentId = null;
    }
}

// 浮动拒绝不杀死内核（Node 默认 unhandledRejections=throw）：挂到当前 run 的 stderr。
process.on('unhandledRejection', (reason) => {
    if (currentId !== null) {
        emit({ type: 'stderr', id: currentId, text: 'UnhandledRejection: ' + fmt(reason) + '\n' });
    }
});

freshContext();
emit({ type: 'ready' });

let chain = Promise.resolve();
readline
    .createInterface({ input: process.stdin, terminal: false })
    .on('line', (raw) => {
        chain = chain
            .then(() => handleLine(raw))
            .catch((err) => {
                process.stderr.write(
                    'gyre eval 内核致命错误：\n' + (err !== null && err.stack ? err.stack : fmt(err))
                );
                process.exit(1);
            });
    });
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    /// 探测可用的 node（缺失则返回 `None`，测试跳过）。
    fn find_node() -> Option<String> {
        let probe = Command::new("node").arg("--version").output().ok()?;
        probe.status.success().then(|| "node".to_string())
    }

    /// 用真实 node 做语法检查（`.mjs` 按 ESM 解析）；node 不存在则跳过。
    #[test]
    fn driver_js_syntax_ok() {
        if find_node().is_none() {
            return;
        }
        let workdir = std::env::temp_dir().join(format!("gyre_eval_js_{}", std::process::id()));
        std::fs::create_dir_all(&workdir).expect("创建临时目录失败");
        let path = workdir.join("driver.mjs");
        std::fs::write(&path, JS_DRIVER).expect("写入临时驱动失败");
        let status = Command::new("node")
            .arg("--check")
            .arg(path.to_str().expect("路径非 UTF-8"))
            .status()
            .expect("运行 node --check 失败");
        let _ = std::fs::remove_dir_all(&workdir);
        assert!(status.success(), "内嵌 JS_DRIVER 存在 JavaScript 语法错误");
    }

    /// 协议帧往返：ready → stdout/result → error（真实 node 子进程直连，不经 manager）。
    #[test]
    fn driver_js_protocol_round_trip() {
        if find_node().is_none() {
            return;
        }
        let mut child = Command::new("node")
            .arg("--input-type=module")
            .arg("-e")
            .arg(JS_DRIVER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动 node 内核失败");
        let mut stdin = child.stdin.take().expect("无法获取内核 stdin");
        let stdout = child.stdout.take().expect("无法获取内核 stdout");
        let mut reader = BufReader::new(stdout);

        // 就绪帧。
        let frame = read_frame(&mut reader);
        assert_eq!(frame.get("type").and_then(|t| t.as_str()), Some("ready"));

        // echo + 声明 + 末表达式：stdout 帧随后 result 帧（完成值 = 末表达式）。
        writeln!(
            stdin,
            r#"{{"id":1,"code":"var answer = 21; console.log('hi'); 1+1","token":"tok"}}"#
        )
        .expect("写入请求失败");
        stdin.flush().expect("flush 失败");
        let frame = read_frame(&mut reader);
        assert_eq!(frame.get("type").and_then(|t| t.as_str()), Some("stdout"));
        assert_eq!(frame.get("id"), Some(&serde_json::json!(1)));
        assert_eq!(frame.get("text").and_then(|t| t.as_str()), Some("hi\n"));
        let frame = read_frame(&mut reader);
        assert_eq!(frame.get("type").and_then(|t| t.as_str()), Some("result"));
        assert_eq!(frame.get("output").and_then(|o| o.as_str()), Some("2"));

        // 共享命名空间：上一 run 的 var 声明跨 run 持久。
        writeln!(stdin, r#"{{"id":2,"code":"answer * 2 + 1","token":"tok"}}"#)
            .expect("写入请求失败");
        stdin.flush().expect("flush 失败");
        let frame = read_frame(&mut reader);
        assert_eq!(frame.get("type").and_then(|t| t.as_str()), Some("result"));
        assert_eq!(frame.get("output").and_then(|o| o.as_str()), Some("43"));

        // 异常 → error 帧（含堆栈文本与错误消息）。
        writeln!(
            stdin,
            r#"{{"id":3,"code":"throw new Error('boom')","token":"tok"}}"#
        )
        .expect("写入请求失败");
        stdin.flush().expect("flush 失败");
        let frame = read_frame(&mut reader);
        assert_eq!(frame.get("type").and_then(|t| t.as_str()), Some("error"));
        let message = frame.get("message").and_then(|m| m.as_str()).unwrap_or("");
        assert!(
            message.contains("Error: boom"),
            "error 应含 Error: boom：{message}"
        );

        // 顶层 await（async IIFE 包裹，显式 return）。
        writeln!(
            stdin,
            r#"{{"id":4,"code":"const r = await Promise.resolve(6); return r * 7","token":"tok"}}"#
        )
        .expect("写入请求失败");
        stdin.flush().expect("flush 失败");
        let frame = read_frame(&mut reader);
        assert_eq!(frame.get("type").and_then(|t| t.as_str()), Some("result"));
        assert_eq!(frame.get("output").and_then(|o| o.as_str()), Some("42"));

        drop(stdin);
        let _ = child.wait();
    }

    /// 读取一行 NDJSON 帧并解析（阻塞直至读到非空行）。
    fn read_frame(reader: &mut BufReader<std::process::ChildStdout>) -> serde_json::Value {
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).expect("读取内核输出失败");
            assert!(!line.is_empty(), "内核 stdout 意外 EOF");
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(line.trim()).expect("帧应可解析");
        }
    }
}
