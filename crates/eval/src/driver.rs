//! 内嵌 Python 驱动：`python3 -u -c <driver>` 启动的持久内核。
//!
//! 协议：宿主经 stdin 逐行写 NDJSON 请求 `{"id":N,"code":"…","token":"…"}`（token 按 run
//! 轮换，经请求帧下发）；内核经 stdout 逐行写回 NDJSON 帧：
//!
//! - `{"type":"ready"}` —— 内核就绪
//! - `{"type":"stdout","id":N,"text":"…"}` / `{"type":"stderr","id":N,"text":"…"}` —— 用户输出
//! - `{"type":"result","id":N,"ok":true,"output":repr}` —— 执行成功（output 为末表达式 repr，无则 null）
//! - `{"type":"error","id":N,"message":"…"}` —— 执行异常（traceback，前后各截 30 行）
//!
//! 内置 `tool` 代理对象：`tool.<name>(**kwargs)` 经 `GYRE_BRIDGE_URL` + `GYRE_BRIDGE_TOKEN`
//! 发 POST /call 调用宿主的同名工具（401 时给出「桥未启用/过期」中文提示）；`tool.reset()`
//! 清空共享命名空间。

/// Python 持久内核驱动源码（作为 `python3 -u -c <driver>` 的代码参数传入）。
pub const PYTHON_DRIVER: &str = r#""""Gyre eval 持久 Python 内核驱动。

经 stdin 读取 NDJSON 请求（每行一条 {"id":N,"code":"...","token":"..."}），在共享命名空间
_NS 中 exec 用户代码，经 stdout 逐行写回 NDJSON 帧。内置 tool 代理对象供回调宿主工具。
"""

import ast
import io
import json
import os
import sys
import traceback
import urllib.error
import urllib.request

# 真实进程 stdout/stderr：仅 _emit 使用，避免与用户输出捕获互相干扰。
_OUT = sys.stdout
_ERR = sys.stderr

# 环回桥地址与当前 run 的 token（token 随请求帧按 run 轮换）。
_BRIDGE_URL = os.environ.get("GYRE_BRIDGE_URL", "").rstrip("/")
_token = os.environ.get("GYRE_BRIDGE_TOKEN", "")

# 当前请求 id（供 _Capture 逐行打帧）。
_CURRENT_ID = None


def _emit(frame):
    """向宿主写一行 NDJSON 并立即 flush。"""
    _OUT.write(json.dumps(frame, ensure_ascii=False) + "\n")
    _OUT.flush()


def _call_bridge(tool, args):
    """POST /call 调用宿主工具，返回解析后的 JSON 值（通常为字符串）。"""
    if not _BRIDGE_URL:
        raise RuntimeError("桥未启用：GYRE_BRIDGE_URL 未设置")
    body = json.dumps({"token": _token, "tool": tool, "args": args}).encode("utf-8")
    req = urllib.request.Request(
        _BRIDGE_URL + "/call",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        if exc.code == 401:
            raise RuntimeError(
                "桥未启用/过期：当前 eval 运行的桥 token 无效或已注销（HTTP 401），"
                "请确认 eval 工具仍在执行中"
            ) from exc
        raise RuntimeError("桥调用失败：HTTP %s %s" % (exc.code, exc.reason)) from exc
    except Exception as exc:  # noqa: BLE001 网络/解析错误统一转为可读错误
        raise RuntimeError("桥调用失败：%s" % (exc,)) from exc
    if data.get("ok"):
        return data.get("output")
    err = data.get("error")
    if isinstance(err, dict):
        err = err.get("message") or err.get("error") or json.dumps(err, ensure_ascii=False)
    elif not isinstance(err, str):
        err = json.dumps(err, ensure_ascii=False) if err is not None else "未知错误"
    raise RuntimeError("工具 %s 执行失败：%s" % (tool, err))


class _ToolCall:
    """一次工具调用的可调用对象：tool.<name>(**kwargs)。"""

    def __init__(self, name):
        self._name = name

    def __call__(self, **kwargs):
        return _call_bridge(self._name, kwargs)


class _ToolProxy:
    """内核内暴露给用户代码的 tool 代理对象。

    属性访问即工具名（tool.read_file(path="...")）；另有 tool.reset() 清空命名空间。
    """

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        return _ToolCall(name)

    def reset(self):
        """清空共享命名空间（保留 tool 代理与下划线内建）。"""
        ns = globals()["_NS"]
        for key in [k for k in list(ns) if not k.startswith("__") and k != "tool"]:
            del ns[key]
        return "命名空间已重置"


class _Capture(io.TextIOBase):
    """用户 stdout/stderr 捕获器：每次 write 立即打帧，实现逐行流式回传。"""

    def __init__(self, kind):
        super().__init__()
        self._kind = kind

    def write(self, text):
        if text:
            _emit({"type": self._kind, "id": _CURRENT_ID, "text": text})
        return len(text)

    def flush(self):
        pass


class _NoStdin(io.TextIOBase):
    """持久内核中用户代码不可读 stdin（协议管道被驱动占用）。"""

    def read(self, *args):
        raise RuntimeError("持久内核中不支持读取 stdin")

    def readline(self, *args):
        raise RuntimeError("持久内核中不支持读取 stdin")

    def readlines(self, *args):
        raise RuntimeError("持久内核中不支持读取 stdin")

    def __iter__(self):
        return self

    def __next__(self):
        raise StopIteration


def _truncate_traceback(text):
    """traceback 截断：前后各保留 30 行，中间省略。"""
    lines = text.rstrip("\n").splitlines()
    if len(lines) <= 60:
        return "\n".join(lines)
    head, tail = lines[:30], lines[-30:]
    omitted = len(lines) - 60
    return "\n".join(head + ["......（中间省略 %d 行）......" % omitted] + tail)


def _exec_run(rid, code):
    """执行一段用户代码，返回 (ok, output|message)。"""
    global _CURRENT_ID
    _CURRENT_ID = rid
    real_stdout, real_stderr, real_stdin = sys.stdout, sys.stderr, sys.stdin
    sys.stdout, sys.stderr, sys.stdin = _Capture("stdout"), _Capture("stderr"), _NoStdin()
    try:
        # 将代码最末的顶层表达式改写为 `_ = <expr>`，获得 REPL 式结果值（_NS 内共享）。
        tree = ast.parse(code, mode="exec")
        if tree.body and isinstance(tree.body[-1], ast.Expr):
            last = tree.body.pop()
            assign = ast.Assign(
                targets=[ast.Name(id="_", ctx=ast.Store())],
                value=last.value,
            )
            tree.body.append(assign)
            ast.fix_missing_locations(tree)
        compiled = compile(tree, "<eval>", "exec")
        exec(compiled, _NS)
    except BaseException:
        return False, _truncate_traceback(traceback.format_exc())
    finally:
        sys.stdout, sys.stderr, sys.stdin = real_stdout, real_stderr, real_stdin
        _CURRENT_ID = None
    result = _NS.get("_")
    output = None if result is None else repr(result)
    _NS["_"] = None
    return True, output


def _main():
    global _token
    _emit({"type": "ready"})
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        rid = req.get("id")
        if rid is None:
            continue
        code = req.get("code") or ""
        token = req.get("token")
        if token is not None:
            _token = token
        ok, output = _exec_run(rid, code)
        if ok:
            _emit({"type": "result", "id": rid, "ok": True, "output": output})
        else:
            _emit({"type": "error", "id": rid, "message": output})


# 共享命名空间：用户代码与内置 tool 代理的栖息地。
_NS = {"tool": _ToolProxy()}
_NS.setdefault("__name__", "__main__")
_NS.setdefault("__builtins__", __builtins__)
_NS["_"] = None

if __name__ == "__main__":
    try:
        _main()
    except Exception:  # noqa: BLE001 驱动自身致命错误：写真实 stderr 并退出（宿主据此判内核死亡）
        _ERR.write("gyre eval 内核致命错误：\n" + traceback.format_exc())
        _ERR.flush()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// 用真实 python3 编译内嵌驱动做语法检查；python3 不存在则跳过。
    #[test]
    fn driver_python_syntax_ok() {
        let probe = std::process::Command::new("python3")
            .arg("--version")
            .output();
        let Ok(probe) = probe else {
            return;
        };
        if !probe.status.success() {
            return;
        }
        let workdir = std::env::temp_dir().join(format!("gyre_eval_driver_{}", std::process::id()));
        std::fs::create_dir_all(&workdir).expect("创建临时目录失败");
        let path = workdir.join("driver.py");
        std::fs::write(&path, PYTHON_DRIVER).expect("写入临时驱动失败");
        let status = std::process::Command::new("python3")
            .args(["-m", "py_compile", path.to_str().expect("路径非 UTF-8")])
            .status()
            .expect("运行 python3 -m py_compile 失败");
        let _ = std::fs::remove_dir_all(&workdir);
        assert!(status.success(), "内嵌 PYTHON_DRIVER 存在 Python 语法错误");
    }
}
