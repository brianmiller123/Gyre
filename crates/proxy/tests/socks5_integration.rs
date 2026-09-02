//! SOCKS5 代理集成测试：最小 SOCKS5 服务器 + 回环 HTTP 服务器，验证
//! 「启用走代理 / 禁用直连 / 同一 client 运行中切换 / 代理不可达优雅失败」。
//!
//! 不依赖外部代理软件；SOCKS5 服务器仅实现 no-auth + CONNECT（认证路径由
//! `lib.rs` 的 URL 构造单测覆盖）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_config::Socks5Config;
use agent_proxy::{Socks5Controller, build_http_client};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 最小 SOCKS5 服务器：记录成功 CONNECT 的目标（host, port）与隧道计数。
struct Socks5Server {
    addr: SocketAddr,
    connects: Arc<AtomicUsize>,
    targets: Arc<Mutex<Vec<(String, u16)>>>,
}

impl Socks5Server {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 代理");
        let addr = listener.local_addr().expect("代理地址");
        let connects = Arc::new(AtomicUsize::new(0));
        let targets = Arc::new(Mutex::new(Vec::new()));
        let (c, t) = (Arc::clone(&connects), Arc::clone(&targets));
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let (c, t) = (Arc::clone(&c), Arc::clone(&t));
                tokio::spawn(async move {
                    let _ = handle_conn(sock, c, t).await;
                });
            }
        });
        Self {
            addr,
            connects,
            targets,
        }
    }

    fn connect_count(&self) -> usize {
        self.connects.load(Ordering::Relaxed)
    }

    fn targets(&self) -> Vec<(String, u16)> {
        self.targets.lock().expect("锁").clone()
    }
}

/// 单条 SOCKS5 连接：greeting(no-auth) → CONNECT → 隧道双向转发。
async fn handle_conn(
    mut sock: TcpStream,
    connects: Arc<AtomicUsize>,
    targets: Arc<Mutex<Vec<(String, u16)>>>,
) -> std::io::Result<()> {
    // greeting：VER=5, NMETHODS, METHODS…
    let mut buf = [0u8; 2];
    sock.read_exact(&mut buf).await?;
    assert_eq!(buf[0], 5, "仅支持 SOCKS5");
    let mut methods = vec![0u8; buf[1] as usize];
    sock.read_exact(&mut methods).await?;
    assert!(methods.contains(&0), "客户端应提议 no-auth");
    sock.write_all(&[5, 0]).await?; // 选择 no-auth

    // request：VER=5, CMD=1(CONNECT), RSV=0, ATYP, ADDR, PORT
    let mut head = [0u8; 4];
    sock.read_exact(&mut head).await?;
    assert_eq!((head[0], head[1], head[2]), (5, 1, 0));
    let host = match head[3] {
        1 => {
            let mut a = [0u8; 4];
            sock.read_exact(&mut a).await?;
            format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
        }
        3 => {
            let mut len = [0u8; 1];
            sock.read_exact(&mut len).await?;
            let mut h = vec![0u8; len[0] as usize];
            sock.read_exact(&mut h).await?;
            String::from_utf8(h).expect("域名应为 UTF-8")
        }
        4 => {
            let mut a = [0u8; 16];
            sock.read_exact(&mut a).await?;
            format!(
                "{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}",
                u16::from_be_bytes([a[0], a[1]]),
                u16::from_be_bytes([a[2], a[3]]),
                u16::from_be_bytes([a[4], a[5]]),
                u16::from_be_bytes([a[6], a[7]]),
                u16::from_be_bytes([a[8], a[9]]),
                u16::from_be_bytes([a[10], a[11]]),
                u16::from_be_bytes([a[12], a[13]]),
                u16::from_be_bytes([a[14], a[15]]),
            )
        }
        other => panic!("未知 ATYP: {other}"),
    };
    let mut port_buf = [0u8; 2];
    sock.read_exact(&mut port_buf).await?;
    let port = u16::from_be_bytes(port_buf);

    let mut upstream = TcpStream::connect((host.clone(), port)).await?;
    // success reply：VER=5, REP=0, RSV=0, ATYP=1, BND.ADDR=0.0.0.0, BND.PORT=0
    sock.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    connects.fetch_add(1, Ordering::Relaxed);
    targets.lock().expect("锁").push((host, port));

    // 隧道：双向转发直到任一方关闭。
    let (mut ar, mut aw) = sock.split();
    let (mut br, mut bw) = upstream.split();
    let _ = tokio::join!(
        tokio::io::copy(&mut ar, &mut bw),
        tokio::io::copy(&mut br, &mut aw),
    );
    Ok(())
}

/// 回环 HTTP 服务器：固定 200 OK。
async fn start_http() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind HTTP");
    let addr = listener.local_addr().expect("HTTP 地址");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                    )
                    .await;
            });
        }
    });
    addr
}

fn socks5_cfg(host: &str, port: u16, timeout_secs: u64) -> Socks5Config {
    Socks5Config {
        enabled: false,
        host: host.into(),
        port: Some(port),
        username: None,
        password: secrecy::SecretString::from(""),
        connect_timeout_secs: timeout_secs,
    }
}

/// 无连接复用的测试客户端（每次请求新建连接 → 路由方向可被代理计数精确观测）。
fn test_client(ctrl: Option<Arc<Socks5Controller>>) -> reqwest::Client {
    build_http_client(ctrl, None, |b| b.pool_max_idle_per_host(0)).expect("构建客户端")
}

/// 启用 → 请求经代理到达（代理侧记录目标 host:port）。
#[tokio::test]
async fn enabled_routes_through_proxy() {
    let proxy = Socks5Server::start().await;
    let http_addr = start_http().await;
    let ctrl = Socks5Controller::new(
        &socks5_cfg("127.0.0.1", proxy.addr.port(), 5),
        None,
        Some(true),
    )
    .expect("已配置");
    let client = test_client(Some(ctrl));

    let resp = client
        .get(format!("http://{http_addr}/via-proxy"))
        .send()
        .await
        .expect("请求应成功");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(proxy.connect_count(), 1, "应恰好建立 1 条代理隧道");
    assert!(
        proxy.targets().iter().any(|(_, p)| *p == http_addr.port()),
        "代理应记录到目标端口 {}，实际: {:?}",
        http_addr.port(),
        proxy.targets()
    );
}

/// 禁用 → 直连到达（代理侧零连接）。
#[tokio::test]
async fn disabled_connects_directly() {
    let proxy = Socks5Server::start().await;
    let http_addr = start_http().await;
    let ctrl = Socks5Controller::new(
        &socks5_cfg("127.0.0.1", proxy.addr.port(), 5),
        None,
        Some(false),
    )
    .expect("已配置");
    let client = test_client(Some(ctrl));

    let resp = client
        .get(format!("http://{http_addr}/direct"))
        .send()
        .await
        .expect("直连应成功");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(proxy.connect_count(), 0, "禁用时不得走代理");
}

/// 核心验收：同一 client 运行中切换开关 → 下一次请求路由随之变化。
#[tokio::test]
async fn runtime_toggle_reroutes_next_request() {
    let proxy = Socks5Server::start().await;
    let http_addr = start_http().await;
    let ctrl = Socks5Controller::new(&socks5_cfg("127.0.0.1", proxy.addr.port(), 5), None, None)
        .expect("已配置");
    assert!(!ctrl.enabled(), "配置默认关");
    let client = test_client(Some(Arc::clone(&ctrl)));

    // 1) 开启 → 走代理。
    ctrl.set_enabled(true);
    let resp = client
        .get(format!("http://{http_addr}/on"))
        .send()
        .await
        .expect("请求应成功");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(proxy.connect_count(), 1);

    // 2) 关闭 → 直连（代理计数不变）。
    ctrl.set_enabled(false);
    let resp = client
        .get(format!("http://{http_addr}/off"))
        .send()
        .await
        .expect("请求应成功");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(proxy.connect_count(), 1, "关闭后不得新增代理隧道");

    // 3) 再开启 → 重新走代理。
    ctrl.set_enabled(true);
    let resp = client
        .get(format!("http://{http_addr}/on-again"))
        .send()
        .await
        .expect("请求应成功");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(proxy.connect_count(), 2, "重新开启后应再次经代理");
}

/// 代理不可达（端口未监听）→ 请求在 `connect_timeout` 内失败，不 panic、不阻塞。
#[tokio::test]
async fn unreachable_proxy_fails_gracefully() {
    // 占一个端口后立刻释放 → 得到必然拒绝连接的端口。
    let dead = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let dead_port = dead.local_addr().expect("addr").port();
    drop(dead);

    let ctrl = Socks5Controller::new(&socks5_cfg("127.0.0.1", dead_port, 1), None, Some(true))
        .expect("已配置");
    let client = test_client(Some(ctrl));

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.get("http://127.0.0.1:9/").send(),
    )
    .await;
    let result = result.expect("不得挂起超过 5s");
    assert!(result.is_err(), "代理不可达时请求应失败（而非成功）");
}

/// 未配置控制器 → 工厂行为与现状一致（直连可用）。
#[tokio::test]
async fn no_controller_falls_back_to_direct() {
    let http_addr = start_http().await;
    let client = test_client(None);
    let resp = client
        .get(format!("http://{http_addr}/plain"))
        .send()
        .await
        .expect("直连应成功");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

/// 回环 HTTP 服务器：捕获每个请求的 User-Agent 头（供 UA 断言）。
async fn start_http_capture_ua() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind HTTP");
    let addr = listener.local_addr().expect("HTTP 地址");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let s = Arc::clone(&s);
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let ua = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("user-agent:")
                            .or_else(|| l.strip_prefix("User-Agent:"))
                    })
                    .map(|v| v.trim().to_string());
                s.lock().expect("锁").push(ua.unwrap_or_default());
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                    )
                    .await;
            });
        }
    });
    (addr, seen)
}

/// LLM 客户端携带 User-Agent：默认 = OMP 对齐 UA，配置覆盖 / 空串 = 默认。
#[tokio::test]
async fn llm_client_sends_user_agent_with_config_override() {
    let (http_addr, seen) = start_http_capture_ua().await;
    let url = format!("http://{http_addr}/chat");

    // 1) 缺省 → OMP 对齐默认 UA。
    let client = build_http_client(None, None, |b| b).expect("构建客户端");
    client.get(&url).send().await.expect("请求应成功");
    let ua = seen.lock().expect("锁").pop().expect("应捕获 UA");
    assert_eq!(ua, agent_core::platform::default_llm_user_agent());

    // 2) 配置覆盖生效。
    let client = build_http_client(None, Some("my-agent/1.0"), |b| b).expect("构建客户端");
    client.get(&url).send().await.expect("请求应成功");
    let ua = seen.lock().expect("锁").pop().expect("应捕获 UA");
    assert_eq!(ua, "my-agent/1.0");

    // 3) 空字符串视同缺省（不产生空 UA 头）。
    let client = build_http_client(None, Some(""), |b| b).expect("构建客户端");
    client.get(&url).send().await.expect("请求应成功");
    let ua = seen.lock().expect("锁").pop().expect("应捕获 UA");
    assert_eq!(ua, agent_core::platform::default_llm_user_agent());
}
