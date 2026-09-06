//! 回环回调服务器：PKCE / 授权码流的本地接收端（omp `callback-server.ts` 截枝移植）。
//!
//! 仅绑 `127.0.0.1`（无 IPv6 双栈、无 `/launch` 短链——headless/SSH 场景本就有粘贴流兜底）。
//! 语义对齐：state 校验（CSRF）、带 state 的 `error` 参数立即失败、
//! 成功/错误极简 HTML 页、5 分钟超时、与手动粘贴输入赛跑。

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// CSPRNG 随机字节（workspace rand 0.8）。
#[must_use]
pub fn random_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

/// base64url 无填充编码（PKCE/state 用）。
#[must_use]
pub fn b64url(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// PKCE 对：verifier（base64url，96 随机字节 → 128 字符）+ S256 challenge。
#[derive(Debug, Clone)]
pub struct Pkce {
    /// code_verifier（128 字符 base64url）。
    pub verifier: String,
    /// code_challenge（S256(verifier) base64url）。
    pub challenge: String,
}

/// 生成 PKCE S256 对（omp `pkce.ts`：96 字节 CSPRNG → base64url）。
#[must_use]
pub fn generate_pkce() -> Pkce {
    let verifier_bytes = random_bytes(96);
    let verifier = b64url(&verifier_bytes);
    use sha2::Digest;
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    Pkce {
        verifier,
        challenge: b64url(&digest),
    }
}

/// 16 随机字节 hex（omp `generateState`）。
#[must_use]
pub fn generate_state() -> String {
    random_bytes(16)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 等待回调的结果页（`__OAUTH_STATE__` 占位替换）。极简暗色版（omp oauth.html 截枝）。
const RESULT_PAGE_TEMPLATE: &str = r#"<!doctype html><html><head><meta charset="utf-8">
<title>Gyre OAuth</title><style>body{background:#111;color:#eee;font-family:sans-serif;
display:grid;place-items:center;height:100vh;margin:0}main{text-align:center}
h1{font-size:1.4rem}.muted{color:#999;font-size:.85rem}</style></head><body><main>
<h1>__OAUTH_STATE__</h1><p class="muted">可关闭此窗口回到终端。</p></main></body></html>"#;

fn result_page(ok: bool, detail: &str) -> String {
    let headline = if ok { "登录成功" } else { "登录失败" };
    RESULT_PAGE_TEMPLATE.replacen(
        "__OAUTH_STATE__",
        &format!(
            "{headline}<br><span class=\"muted\">{}</span>",
            escape_html(detail)
        ),
        1,
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 回调服务器：`bind` 后 [`CallbackServer::wait_for_code`] 消费等待授权码。
pub struct CallbackServer {
    listener: TcpListener,
    port: u16,
    path: String,
}

impl CallbackServer {
    /// 绑定回环端口。`preferred_port` 被占时：`allow_fallback` → 随机端口兜底
    /// （redirect_uri 动态构造，安全）；否则报错（provider 服务端注册了精确回调，
    /// 如 zai 9999 / codex 1455，端口漂移会被拒绝——omp 同语义）。
    ///
    /// # Errors
    /// 两个端口都绑不上时返回 IO 错误。
    pub async fn bind(
        preferred_port: u16,
        path: &str,
        allow_fallback: bool,
    ) -> std::io::Result<Self> {
        let listener = match TcpListener::bind(("127.0.0.1", preferred_port)).await {
            Ok(l) => l,
            Err(e) if allow_fallback && preferred_port != 0 => {
                TcpListener::bind(("127.0.0.1", 0)).await.map_err(|_| e)?
            }
            Err(e) => return Err(e),
        };
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            port,
            path: path.to_owned(),
        })
    }

    /// 实际监听端口（兜底后可能与 preferred 不同）。
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 本流对外使用的 redirect_uri。
    #[must_use]
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path)
    }

    /// 等待授权码：消费服务器，直到带正确 state 的回调 / 显式 error / 超时。
    ///
    /// state 不匹配的请求回 400 并**继续等待**（浏览器重复刷新不致失败）。
    ///
    /// # Errors
    /// 超时、授权端点回传 error、或连接处理出现不可恢复 IO 错误。
    pub async fn wait_for_code(self, state: &str, timeout: Duration) -> anyhow::Result<String> {
        let expected = state.to_owned();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match self.listener.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx.send(Err(anyhow::anyhow!("回调服务器接受连接失败：{e}")));
                        return;
                    }
                };
                // 单次读取足够（回调请求 < 8KB）；上限防恶意长连接。
                let mut buf = vec![0u8; 8192];
                let n = match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf))
                    .await
                {
                    Ok(Ok(n)) if n > 0 => n,
                    _ => continue,
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let Some(line) = req.lines().next() else {
                    continue;
                };
                // "GET /callback?code=..&state=.. HTTP/1.1"
                let mut parts = line.split_whitespace();
                let _method = parts.next();
                let Some(target) = parts.next() else { continue };
                let (path, query) = match target.split_once('?') {
                    Some((p, q)) => (p, q),
                    None => (target, ""),
                };
                if path != self.path {
                    respond(&mut sock, 404, "not found").await;
                    continue;
                }
                let params = parse_query(query);
                let get = |k: &str| {
                    params
                        .iter()
                        .find(|(key, _)| key == k)
                        .map(|(_, v)| v.clone())
                };
                // 带 state 的 error：用户在授权页点了拒绝——立即失败（omp 同语义）。
                if let Some(err) = get("error") {
                    let desc = get("error_description").unwrap_or_default();
                    respond(
                        &mut sock,
                        500,
                        &result_page(false, &format!("{err}: {desc}")),
                    )
                    .await;
                    let _ = tx.send(Err(anyhow::anyhow!("授权被拒绝：{err} {desc}")));
                    return;
                }
                let req_state = get("state").unwrap_or_default();
                if req_state != expected {
                    respond(
                        &mut sock,
                        400,
                        &result_page(false, "state 不匹配——可能的 CSRF 攻击"),
                    )
                    .await;
                    continue;
                }
                let Some(code) = get("code").filter(|c| !c.is_empty()) else {
                    respond(&mut sock, 400, &result_page(false, "缺少 code 参数")).await;
                    continue;
                };
                respond(&mut sock, 200, &result_page(true, "凭据已接收")).await;
                let _ = tx.send(Ok(code));
                return;
            }
        });
        let code = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| anyhow::anyhow!("等待授权回调超时（{}s）", timeout.as_secs()))?
            .map_err(|_| anyhow::anyhow!("回调通道关闭"))??;
        Ok(code)
    }
}

async fn respond(sock: &mut tokio::net::TcpStream, status: u16, html: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let body = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = sock.write_all(body.as_bytes()).await;
    let _ = sock.shutdown().await;
}

/// 极简 query 解析：`k=v&k2=v2`，值做 percent-decode 与 `+`→空格。
#[must_use]
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ");
    let src = bytes.as_bytes();
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        match src[i] {
            b'%' => match src.get(i + 1..i + 3).and_then(|h| {
                std::str::from_utf8(h)
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            }) {
                Some(b) => {
                    out.push(b);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 手动粘贴输入解析（omp `parseCallbackInput`）：完整 URL / query 串 / 裸 `code#state`。
/// 返回 `(code, Option<state>)`。
#[must_use]
pub fn parse_callback_input(input: &str) -> Option<(String, Option<String>)> {
    let s = input.trim();
    if let Some((_, query)) = s.split_once('?') {
        // URL / 完整 query 形态：#fragment 不属于 query。
        let query = query.split('#').next().unwrap_or(query);
        let params = parse_query(query);
        let code = params
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone());
        let state = params
            .iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.clone());
        return code.filter(|c| !c.is_empty()).map(|c| (c, state));
    }
    if s.contains('=') {
        // 纯 query 串形态。
        let params = parse_query(s);
        let code = params
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone());
        let state = params
            .iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.clone());
        return code.filter(|c| !c.is_empty()).map(|c| (c, state));
    }
    // 裸 code 形态（可带 #state 尾巴）。
    match s.split_once('#') {
        Some((code, st)) if !code.is_empty() => {
            Some((code.to_owned(), (!st.is_empty()).then(|| st.to_owned())))
        }
        Some(_) => None,
        None if !s.is_empty() && !s.starts_with('/') => Some((s.to_owned(), None)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn callback_captures_code_with_matching_state() {
        let server = CallbackServer::bind(0, "/callback", false).await.unwrap();
        let url = server.redirect_uri();
        let check_url = url.clone();
        let state = "abc123";
        let port = server.port();
        tokio::spawn(async move {
            // 略等绑妥后模拟浏览器回调。
            let wait = tokio::time::sleep(Duration::from_millis(50));
            wait.await;
            let _ = reqwest::get(format!("{url}?code=xyz&state={state}")).await;
        });
        let code = server
            .wait_for_code(state, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(code, "xyz");
        // 端口兜底关闭后 redirect 与实际端口一致。
        assert_eq!(check_url, format!("http://127.0.0.1:{port}/callback"));
    }

    #[tokio::test]
    async fn callback_rejects_error_param_and_state_mismatch_keeps_waiting() {
        let server = CallbackServer::bind(0, "/callback", false).await.unwrap();
        let url = server.redirect_uri();
        let wrong_state = tokio::spawn({
            let url = url.clone();
            async move {
                let wait = tokio::time::sleep(Duration::from_millis(50));
                wait.await;
                // 错误 state + 缺 code：均应被无视（连接层 4xx），服务器继续等待。
                let _ = reqwest::get(format!("{url}?code=x&state=WRONG")).await;
                let _ = reqwest::get(format!("{url}?state=WRONG2")).await;
            }
        });
        let code = server.wait_for_code("good", Duration::from_secs(1)).await;
        wrong_state.await.unwrap();
        assert!(code.is_err(), "无正确回调应超时报错");
    }

    #[tokio::test]
    async fn callback_error_param_fails_immediately() {
        let server = CallbackServer::bind(0, "/callback", false).await.unwrap();
        let url = server.redirect_uri();
        tokio::spawn(async move {
            let wait = tokio::time::sleep(Duration::from_millis(50));
            wait.await;
            let _ = reqwest::get(format!("{url}?error=access_denied&state=s")).await;
        });
        let err = server
            .wait_for_code("s", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("access_denied"), "{err}");
    }

    #[test]
    fn parse_callback_input_accepts_url_query_and_bare() {
        let (c, s) =
            parse_callback_input("http://127.0.0.1:54545/callback?code=a%20b&state=st#").unwrap();
        assert_eq!((c.as_str(), s.as_deref()), ("a b", Some("st")));
        let (c, s) = parse_callback_input("code=qq&state=jp").unwrap();
        assert_eq!((c.as_str(), s.as_deref()), ("qq", Some("jp")));
        let (c, s) = parse_callback_input("barecode#st123").unwrap();
        assert_eq!((c.as_str(), s.as_deref()), ("barecode", Some("st123")));
        let (c, s) = parse_callback_input("justacode").unwrap();
        assert_eq!((c.as_str(), s.as_deref()), ("justacode", None));
        assert!(parse_callback_input("").is_none());
    }
}
