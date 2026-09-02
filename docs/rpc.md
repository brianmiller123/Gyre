# `agent --rpc`：NDJSON 行协议

`agent --rpc` 提供基于 stdin/stdout 的行协议服务面，供外部语言 / 机器人 / CI 集成（对标
oh-my-pi `--mode rpc` 的最小集）。协议与 ACP（JSON-RPC over stdio）互斥，stdout 仅承载协议行。

## 用法

```bash
agent --rpc [--cwd <dir>] [--approval-mode yolo] [--model <alias>]
```

- 一个进程一个 Agent 实例：首次 `prompt` 前惰性构建，模型 / 模式取配置默认。
- 会话持久：Agent 与 Context 跨多个 `prompt` 请求复用（同一会话累积上下文；会话按 cwd
  项目隔离，落盘于 session store，与 `agent` 单次任务 / REPL 同通道）。
- 互斥：与 `--serve`、`--acp` 同时给出时报错退出（stdout 是协议通道）。
- 审批：stdin 是协议通道，无法交互审批——运行中触发的审批一律自动拒绝（写工具会失败并
  反映在 `tool_result` 事件中）。需要全自动写权限时以 `--approval-mode yolo` 启动。
- 所有日志（telemetry / 状态行）走 stderr，不影响协议。
- Ctrl-C（SIGINT）：运行中取消当前 turn（以 `done ok=false error="cancelled"` 收尾）后
  优雅退出；空闲时直接退出。

## 请求（stdin 逐行 JSON）

| type      | 字段                     | 说明                                                               |
|-----------|--------------------------|--------------------------------------------------------------------|
| `prompt`  | `id`、`text`、`model?`   | 执行一轮完整 run（text 作为用户消息注入，从当前上下文继续）。`model` 非空时先尝试切换 profile（失败 → `error` 响应，不执行本轮）。 |
| `cancel`  | `id`                     | 取消当前运行中的 turn（该 turn 以 `done ok=false error="cancelled"` 收尾）。无运行中 turn → `error no running turn`。 |
| `ping`    | `id`                     | 应答 `pong`。                                                     |

未知 `type` → `{"type":"error","id":N,"message":"unknown rpc message type"}`。
无法解析的 JSON 行 → `{"type":"error","id":0,"message":"invalid json request"}`。
turn 运行中收到重复 `prompt` → `error a turn is already running`（先 `cancel`）。

## 响应 / 事件（stdout 逐行 JSON，单行无内嵌换行）

turn 执行期间事件流式输出（同 `id`），最终恰好一条 `done`（成功或失败）收尾：

| type    | 形状                                                                                          | 说明 |
|---------|-----------------------------------------------------------------------------------------------|------|
| `event` | `{"type":"event","id":1,"event":{"kind":"text_delta","text":"…"}}`                            | 事件（见下）。 |
| `done`  | `{"type":"done","id":1,"ok":true,"usage":{…},"turns":N}`                                      | 成功收尾。 |
| `done`  | `{"type":"done","id":1,"ok":false,"error":"…"}`                                               | 失败 / 取消收尾。 |
| `pong`  | `{"type":"pong","id":3}`                                                                      | `ping` 应答（turn 运行中也可收到）。 |
| `error` | `{"type":"error","id":N,"message":"…"}`                                                       | 请求层错误（未知类型 / 模型切换失败 / 无效 JSON 等）。 |

`event.kind` 取值：

| kind             | 形状                                                                                          | 对应 |
|------------------|-----------------------------------------------------------------------------------------------|------|
| `text_delta`     | `{"kind":"text_delta","text":"…"}`                                                            | 流式文本增量 |
| `thinking_delta` | `{"kind":"thinking_delta","text":"…"}`                                                        | 思考增量 |
| `tool_call`      | `{"kind":"tool_call","name":"…","args":{…}}`                                                  | 工具开始（含参数） |
| `tool_result`    | `{"kind":"tool_result","name":"…","ok":bool}`                                                 | 工具结束（成败） |
| `status`         | `{"kind":"status","text":"…"}`                                                                | 信息性状态 |
| `usage`          | `{"kind":"usage","input":N,"output":N,"cache_read":N,"cache_write":N,"cost":f}`               | 用量更新 |

`done` 的 `usage` 同 `usage` 事件形状（`input/output/cache_read/cache_write/cost`）。

## 示例

```bash
printf '%s\n' \
  '{"type":"ping","id":3}' \
  '{"type":"prompt","id":1,"text":"用一句话介绍你自己"}' \
  '{"type":"prompt","id":2,"text":"继续上一条，展开说"}' \
  | agent --rpc --cwd /path/to/project
```

## Python 客户端

```python
#!/usr/bin/env python3
"""agent --rpc 客户端示例：启动子进程，发送 prompt 并流式消费事件。"""
import json
import subprocess
import sys

BIN = ["agent", "--rpc", "--approval-mode", "yolo"]


class RpcClient:
    def __init__(self, argv=BIN):
        self.proc = subprocess.Popen(
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1
        )
        self._id = 0

    def send(self, **payload):
        self._id += 1
        payload.setdefault("id", self._id)
        self.proc.stdin.write(json.dumps(payload) + "\n")
        self.proc.stdin.flush()
        return payload["id"]

    def read(self):
        line = self.proc.stdout.readline()
        return json.loads(line) if line else None

    def prompt(self, text, model=None):
        rid = self.send(type="prompt", text=text, model=model)
        done = None
        while True:
            msg = self.read()
            if msg is None:
                break
            if msg.get("id") != rid:
                continue
            if msg["type"] == "event":
                ev = msg["event"]
                if ev["kind"] == "text_delta":
                    sys.stdout.write(ev["text"])
                    sys.stdout.flush()
                elif ev["kind"] == "tool_call":
                    print(f"\n[工具] {ev['name']} {json.dumps(ev.get('args'), ensure_ascii=False)}")
            elif msg["type"] == "done":
                done = msg
                break
            elif msg["type"] == "error":
                print(f"\n[错误] {msg.get('message')}", file=sys.stderr)
                break
        return done

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=5)


if __name__ == "__main__":
    c = RpcClient()
    try:
        for q in sys.argv[1:] or ["用一句话介绍你自己"]:
            done = c.prompt(q)
            if done:
                print(f"\n== done ok={done.get('ok')} turns={done.get('turns')} "
                      f"usage={done.get('usage')}")
    finally:
        c.close()
```

运行：`python3 rpc_client.py "写一个 hello world 的 Python 脚本并保存到 /tmp/hello.py"`
（要求 `agent` 在 PATH；或把 `BIN` 改为二进制绝对路径）。
