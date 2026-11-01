//! The embedded libtailscale node and its loopback forwarders.

use std::{
    collections::HashMap,
    fs::OpenOptions,
    net::Ipv4Addr,
    os::unix::{
        fs::OpenOptionsExt,
        io::{FromRawFd, IntoRawFd},
    },
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::Context;
use serde::Deserialize;
use tokio::{net::TcpListener, sync::OnceCell};

use crate::platform::application_data_directory;

/// How long a connection waits for the user to finish signing in.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a connection waits for an already signed-in node to come up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
const NODE_HOSTNAME: &str = "astesia";

/// The node lives for the rest of the process: libtailscale 0.2.2 crashes when
/// a server is closed, and every forwarder borrows it.
static NODE: OnceCell<Node> = OnceCell::const_new();
static FORWARDERS: Mutex<Option<HashMap<String, u16>>> = Mutex::new(None);
static LOGIN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Node {
    tailscale: libtailscale::Tailscale,
    local_api: LocalApi,
}

struct LocalApi {
    client: reqwest::Client,
    status_url: String,
    credential: String,
}

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "AuthURL", default)]
    auth_url: String,
}

/// Returns a loopback port that forwards to `host:port` over the tailnet.
pub(super) async fn forward(host: &str, port: u16) -> anyhow::Result<u16> {
    let node = node().await?;
    node.wait_until_running().await?;

    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    if let Some(port) = forwarders()
        .as_ref()
        .and_then(|map| map.get(&target).copied())
    {
        return Ok(port);
    }

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("无法为 Tailscale 连接创建本地端口")?;
    let local_port = listener.local_addr()?.port();
    {
        let mut map = forwarders();
        let map = map.get_or_insert_with(HashMap::new);
        if let Some(existing) = map.get(&target) {
            return Ok(*existing);
        }
        map.insert(target.clone(), local_port);
    }
    tokio::spawn(accept_loop(node, listener, target));
    Ok(local_port)
}

fn forwarders() -> std::sync::MutexGuard<'static, Option<HashMap<String, u16>>> {
    FORWARDERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Forgets the forwarder when its task ends, including when its runtime shuts
/// down, so the next connection creates a fresh one.
struct ForwarderRegistration(String);

impl Drop for ForwarderRegistration {
    fn drop(&mut self) {
        if let Some(map) = forwarders().as_mut() {
            map.remove(&self.0);
        }
    }
}

async fn accept_loop(node: &'static Node, listener: TcpListener, target: String) {
    let _registration = ForwarderRegistration(target.clone());
    loop {
        let inbound = match listener.accept().await {
            Ok((inbound, _)) => inbound,
            Err(error) => {
                log::warn!("Tailscale forwarder for {target} stopped accepting: {error}");
                return;
            }
        };
        let target = target.clone();
        tokio::spawn(async move {
            if let Err(error) = relay(node, inbound, &target).await {
                log::warn!("Tailscale connection to {target} failed: {error:#}");
            }
        });
    }
}

async fn relay(
    node: &'static Node,
    mut inbound: tokio::net::TcpStream,
    target: &str,
) -> anyhow::Result<()> {
    let dial_target = target.to_string();
    let dial = tokio::task::spawn_blocking(move || node.tailscale.dial("tcp", &dial_target));
    let stream = tokio::time::timeout(DIAL_TIMEOUT, dial)
        .await
        .map_err(|_| anyhow::anyhow!("通过 Tailscale 连接 {target} 超时"))??
        .map_err(|error| anyhow::anyhow!("无法通过 Tailscale 连接 {target}: {error}"))?;
    // libtailscale hands back one end of a Unix socket pair wrapped as a
    // TcpStream; treat it as the Unix socket it is.
    let fd = stream.into_raw_fd();
    // SAFETY: `fd` was just released by `into_raw_fd`, so this is its only owner.
    let outbound = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    outbound.set_nonblocking(true)?;
    let mut outbound = tokio::net::UnixStream::from_std(outbound)?;
    let _ = inbound.set_nodelay(true);
    tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
    Ok(())
}

async fn node() -> anyhow::Result<&'static Node> {
    NODE.get_or_try_init(|| async {
        tokio::task::spawn_blocking(Node::start)
            .await
            .context("启动内置 Tailscale 失败")?
    })
    .await
}

impl Node {
    fn start() -> anyhow::Result<Self> {
        let directory = state_directory()?;
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("无法创建 Tailscale 状态目录 {}", directory.display()))?;
        let directory_text = directory
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Tailscale 状态目录路径不是有效的 UTF-8"))?;

        let mut tailscale = libtailscale::Tailscale::new();
        let configured = (|| -> Result<(), String> {
            tailscale.set_dir(directory_text)?;
            tailscale.set_hostname(NODE_HOSTNAME)?;
            if let Ok(control_url) = std::env::var("ASTESIA_TAILSCALE_CONTROL_URL") {
                tailscale.set_control_url(control_url.trim())?;
            }
            // tsnet logs every few seconds; keep it out of the terminal.
            let log = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(directory.join("tailscale.log"));
            match log {
                Ok(file) => tailscale.set_logfd(file.into_raw_fd())?,
                Err(_) => tailscale.set_logfd(-1)?,
            }
            tailscale.start()
        })();
        let started = configured.and_then(|()| tailscale.loopback());
        let loopback = match started {
            Ok(loopback) => loopback,
            Err(error) => {
                // Closing an unstarted libtailscale server crashes the process.
                std::mem::forget(tailscale);
                anyhow::bail!("启动内置 Tailscale 失败: {error}");
            }
        };

        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .context("无法创建 Tailscale LocalAPI 客户端")?;
        Ok(Self {
            tailscale,
            local_api: LocalApi {
                client,
                status_url: format!("http://{}/localapi/v0/status?peers=false", loopback.address),
                credential: loopback.credential,
            },
        })
    }

    async fn wait_until_running(&self) -> anyhow::Result<()> {
        if self.local_api.status().await?.backend_state == "Running" {
            return Ok(());
        }
        // One connection drives sign-in; the rest wait for it.
        let _login = LOGIN_LOCK.lock().await;
        let started = Instant::now();
        let mut opened_url = None::<String>;
        loop {
            let status = self.local_api.status().await?;
            match status.backend_state.as_str() {
                "Running" => return Ok(()),
                "NeedsMachineAuth" => anyhow::bail!(
                    "Tailscale 设备等待管理员批准，请在 Tailscale 管理后台批准名为 {NODE_HOSTNAME} 的设备后重试"
                ),
                "NeedsLogin" if !status.auth_url.is_empty() => {
                    if !super::browser_login_enabled() {
                        anyhow::bail!(
                            "内置 Tailscale 尚未登录，请在浏览器打开以下地址登录后重试：{}",
                            status.auth_url
                        );
                    }
                    if opened_url.as_deref() != Some(status.auth_url.as_str()) {
                        open_in_browser(&status.auth_url);
                        opened_url = Some(status.auth_url);
                    }
                }
                _ => {}
            }
            let limit = if opened_url.is_some() {
                LOGIN_TIMEOUT
            } else {
                STARTUP_TIMEOUT
            };
            if started.elapsed() > limit {
                anyhow::bail!(match opened_url {
                    Some(url) =>
                        format!("等待 Tailscale 登录超时，请在浏览器完成登录后重试：{url}"),
                    None => format!(
                        "内置 Tailscale 未能就绪（状态：{}），请检查网络后重试",
                        status.backend_state
                    ),
                });
            }
            tokio::time::sleep(STATUS_POLL_INTERVAL).await;
        }
    }
}

impl LocalApi {
    async fn status(&self) -> anyhow::Result<Status> {
        self.client
            .get(&self.status_url)
            .header("Sec-Tailscale", "localapi")
            .basic_auth("", Some(&self.credential))
            .send()
            .await
            .context("无法读取内置 Tailscale 状态")?
            .error_for_status()
            .context("无法读取内置 Tailscale 状态")?
            .json::<Status>()
            .await
            .context("内置 Tailscale 返回了无法识别的状态")
    }
}

/// The desktop app and the MCP server each keep their own node: two processes
/// must not share one tsnet state directory.
fn state_directory() -> anyhow::Result<PathBuf> {
    let base =
        application_data_directory().ok_or_else(|| anyhow::anyhow!("无法确定 Astesia 数据目录"))?;
    let name = if super::browser_login_enabled() {
        "tailscale"
    } else {
        "tailscale-mcp"
    };
    Ok(base.join(name))
}

fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    if let Err(error) = std::process::Command::new(program).arg(url).spawn() {
        log::warn!("Could not open the Tailscale sign-in page: {error}");
    }
}

#[cfg(test)]
mod tests {
    /// Needs network access to the Tailscale control plane. Run with:
    /// `ASTESIA_DEBUG_DATA_DIR=<empty absolute dir> cargo test --features tailscale --lib -- --ignored tailnet`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real Tailscale node"]
    async fn unsigned_node_reports_a_sign_in_url() {
        let error = super::forward("100.64.0.1", 5432)
            .await
            .expect_err("a new node is not signed in");
        let message = error.to_string();
        assert!(
            message.contains("https://login.tailscale.com/"),
            "unexpected error: {message}"
        );
    }
}
