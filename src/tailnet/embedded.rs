//! The embedded libtailscale node and its loopback forwarders.

use std::{
    collections::HashMap,
    fs::OpenOptions,
    net::Ipv4Addr,
    os::unix::{
        fs::OpenOptionsExt,
        io::{FromRawFd, IntoRawFd},
    },
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::Context;
use serde::Deserialize;
use tokio::{net::TcpListener, sync::OnceCell};

use super::{state_directory, TailnetIdentity, TailnetState};

/// How long a connection waits for the user to finish signing in.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a connection waits for an already signed-in node to come up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How long a node that needs sign-in may go without a sign-in URL before one is requested.
const NEW_LOGIN_GRACE: Duration = Duration::from_secs(3);
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
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
    /// `http://<loopback>/localapi/v0`
    base_url: String,
    credential: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    #[serde(default)]
    backend_state: String,
    #[serde(rename = "AuthURL", default)]
    auth_url: String,
    #[serde(rename = "TailscaleIPs", default, deserialize_with = "null_as_default")]
    tailscale_ips: Vec<String>,
    #[serde(rename = "Self", default)]
    self_node: Option<SelfNode>,
    #[serde(default)]
    current_tailnet: Option<CurrentTailnet>,
    #[serde(default, deserialize_with = "null_as_default")]
    user: HashMap<String, UserProfile>,
}

/// Go encodes empty slices and maps as `null`.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SelfNode {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "UserID", default)]
    user_id: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    tags: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CurrentTailnet {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UserProfile {
    #[serde(default)]
    login_name: String,
}

impl Status {
    fn into_state(self) -> TailnetState {
        match self.backend_state.as_str() {
            "Running" => {
                let account = self
                    .self_node
                    .as_ref()
                    .and_then(|node| self.user.get(&node.user_id.to_string()))
                    .map(|user| user.login_name.clone())
                    .filter(|name| !name.is_empty());
                let tags = self
                    .self_node
                    .as_ref()
                    .map(|node| node.tags.clone())
                    .unwrap_or_default();
                TailnetState::Running(TailnetIdentity {
                    account,
                    tailnet: self
                        .current_tailnet
                        .map(|tailnet| tailnet.name)
                        .filter(|name| !name.is_empty()),
                    dns_name: self
                        .self_node
                        .map(|node| node.dns_name.trim_end_matches('.').to_string())
                        .filter(|name| !name.is_empty()),
                    addresses: self.tailscale_ips,
                    tags,
                })
            }
            "NeedsLogin" => TailnetState::NeedsLogin,
            "NeedsMachineAuth" => TailnetState::NeedsApproval,
            _ => TailnetState::Starting,
        }
    }
}

/// The node's state, without starting it.
pub(super) async fn state() -> TailnetState {
    let Some(node) = NODE.get() else {
        return TailnetState::NotStarted;
    };
    match node.local_api.status().await {
        Ok(status) => status.into_state(),
        Err(error) => TailnetState::Failed(format!("{error:#}")),
    }
}

/// Starts the node if needed, without waiting for sign-in.
pub(super) async fn start() -> anyhow::Result<()> {
    node().await.map(|_| ())
}

/// Renames the running node on the tailnet. A node that has not started
/// picks the name up from the settings file when it starts.
pub(super) async fn apply_hostname(hostname: &str) -> anyhow::Result<()> {
    let Some(node) = NODE.get() else {
        return Ok(());
    };
    node.local_api
        .patch_prefs(serde_json::json!({ "Hostname": hostname, "HostnameSet": true }))
        .await
}

/// Starts the node if needed and waits until it is signed in.
pub(super) async fn sign_in() -> anyhow::Result<()> {
    node().await?.wait_until_running().await
}

/// Signs the node out of its tailnet. Existing forwarders stop working.
pub(super) async fn sign_out() -> anyhow::Result<()> {
    let node = node().await?;
    node.local_api.post("logout").await
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

        let hostname = super::settings().effective_hostname();
        let mut tailscale = libtailscale::Tailscale::new();
        let configured = (|| -> Result<(), String> {
            tailscale.set_dir(directory_text)?;
            tailscale.set_hostname(&hostname)?;
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
                base_url: format!("http://{}/localapi/v0", loopback.address),
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
        let mut requested_login = false;
        loop {
            let status = self.local_api.status().await?;
            // After a sign-out the node waits in NeedsLogin without a URL until
            // a new interactive login is requested.
            if status.backend_state == "NeedsLogin"
                && status.auth_url.is_empty()
                && !requested_login
                && started.elapsed() > NEW_LOGIN_GRACE
            {
                requested_login = true;
                self.local_api.post("login-interactive").await?;
            }
            match status.backend_state.as_str() {
                "Running" => return Ok(()),
                "NeedsMachineAuth" => anyhow::bail!(
                    "Tailscale 设备等待管理员批准，请在 Tailscale 管理后台批准名为 {} 的设备后重试",
                    super::settings().effective_hostname()
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
    async fn post(&self, endpoint: &str) -> anyhow::Result<()> {
        self.client
            .post(format!("{}/{endpoint}", self.base_url))
            .header("Sec-Tailscale", "localapi")
            .basic_auth("", Some(&self.credential))
            .send()
            .await
            .with_context(|| format!("内置 Tailscale 请求 {endpoint} 失败"))?
            .error_for_status()
            .with_context(|| format!("内置 Tailscale 请求 {endpoint} 失败"))?;
        Ok(())
    }

    async fn patch_prefs(&self, prefs: serde_json::Value) -> anyhow::Result<()> {
        self.client
            .patch(format!("{}/prefs", self.base_url))
            .header("Sec-Tailscale", "localapi")
            .basic_auth("", Some(&self.credential))
            .json(&prefs)
            .send()
            .await
            .context("内置 Tailscale 修改设备设置失败")?
            .error_for_status()
            .context("内置 Tailscale 修改设备设置失败")?;
        Ok(())
    }

    async fn status(&self) -> anyhow::Result<Status> {
        self.client
            .get(format!("{}/status?peers=false", self.base_url))
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
    use super::{Status, TailnetIdentity, TailnetState};

    #[test]
    fn running_status_reports_the_signed_in_identity() {
        let status: Status = serde_json::from_str(
            r#"{
                "BackendState": "Running",
                "AuthURL": "",
                "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"],
                "Self": {"DNSName": "astesia.example.ts.net.", "UserID": 42, "Tags": ["tag:db"]},
                "CurrentTailnet": {"Name": "example.com"},
                "User": {"42": {"LoginName": "alex@example.com"}}
            }"#,
        )
        .expect("status parses");
        assert_eq!(
            status.into_state(),
            TailnetState::Running(TailnetIdentity {
                account: Some("alex@example.com".into()),
                tailnet: Some("example.com".into()),
                dns_name: Some("astesia.example.ts.net".into()),
                addresses: vec!["100.101.102.103".into(), "fd7a:115c:a1e0::1".into()],
                tags: vec!["tag:db".into()],
            })
        );
    }

    #[test]
    fn signed_out_states_map_to_login_and_approval() {
        let parse = |json: &str| serde_json::from_str::<Status>(json).unwrap().into_state();
        assert_eq!(
            parse(r#"{"BackendState":"NeedsLogin"}"#),
            TailnetState::NeedsLogin
        );
        // A fresh node's status: Go encodes empty values as null.
        assert_eq!(
            parse(
                r#"{"BackendState":"NeedsLogin","TailscaleIPs":null,"Self":null,"CurrentTailnet":null,"User":null}"#
            ),
            TailnetState::NeedsLogin
        );
        assert_eq!(
            parse(r#"{"BackendState":"NeedsMachineAuth"}"#),
            TailnetState::NeedsApproval
        );
        assert_eq!(
            parse(r#"{"BackendState":"Starting"}"#),
            TailnetState::Starting
        );
    }
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
