//! Routes database connections through an embedded Tailscale node.
//!
//! The embedded node (libtailscale, a build of Go `tsnet`) has its own tailnet
//! identity, separate from any Tailscale client installed on the machine. That
//! lets a work tailnet be used for databases while the system client stays
//! signed in to a personal account.
//!
//! Database drivers only accept a host and port, so each tailnet target gets a
//! loopback listener whose connections are dialed through the embedded node.

// Builds without the embedded node keep the target parsing for its tests.
#![cfg_attr(not(all(unix, feature = "tailscale")), allow(dead_code))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::db::{ConnectionConfig, DbType};
use crate::platform::application_data_directory;

#[cfg(all(unix, feature = "tailscale"))]
mod embedded;
#[cfg(not(all(unix, feature = "tailscale")))]
#[path = "tailnet/unavailable.rs"]
mod embedded;

/// Which process owns the node. Two processes must not share one tsnet state
/// directory, so the desktop app and the MCP server each run their own node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeRole {
    /// The desktop app: may open the sign-in page in the browser.
    Desktop,
    /// Headless processes such as the MCP server: report the sign-in URL in
    /// the connection error instead.
    Headless,
}

impl NodeRole {
    fn current() -> Self {
        if DESKTOP_NODE.load(Ordering::Acquire) {
            Self::Desktop
        } else {
            Self::Headless
        }
    }

    const fn state_directory_name(self) -> &'static str {
        match self {
            Self::Desktop => "tailscale",
            Self::Headless => "tailscale-mcp",
        }
    }

    const fn hostname_suffix(self) -> &'static str {
        match self {
            Self::Desktop => "astesia",
            Self::Headless => "astesia-mcp",
        }
    }

    const fn opens_browser(self) -> bool {
        matches!(self, Self::Desktop)
    }
}

static DESKTOP_NODE: AtomicBool = AtomicBool::new(false);

/// Makes this process the desktop app's node owner. Call it before any other
/// function in this module.
pub(crate) fn use_desktop_node() {
    DESKTOP_NODE.store(true, Ordering::Release);
}

/// Whether this build contains the embedded Tailscale node.
pub(crate) const fn is_available() -> bool {
    cfg!(all(unix, feature = "tailscale"))
}

/// What the embedded node is doing, for the settings window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TailnetState {
    /// This build does not include the embedded node.
    Unavailable,
    /// The node starts on the first Tailscale connection or sign-in.
    NotStarted,
    Starting,
    NeedsLogin,
    /// A tailnet admin has to approve the device.
    NeedsApproval,
    Running(TailnetIdentity),
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TailnetIdentity {
    pub(crate) account: Option<String>,
    pub(crate) tailnet: Option<String>,
    pub(crate) dns_name: Option<String>,
    pub(crate) addresses: Vec<String>,
    pub(crate) tags: Vec<String>,
}

/// Device settings for the desktop app's node, kept in its state directory.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct TailnetSettings {
    /// The machine name on the tailnet. `None` uses [`default_hostname`].
    pub(crate) hostname: Option<String>,
    /// Starts the node when Astesia opens, so the device stays online.
    pub(crate) connect_on_launch: bool,
}

impl TailnetSettings {
    pub(crate) fn effective_hostname(&self) -> String {
        self.hostname.clone().unwrap_or_else(default_hostname)
    }
}

fn state_directory() -> anyhow::Result<PathBuf> {
    let base =
        application_data_directory().ok_or_else(|| anyhow::anyhow!("无法确定 Astesia 数据目录"))?;
    Ok(base.join(NodeRole::current().state_directory_name()))
}

fn settings_path() -> anyhow::Result<PathBuf> {
    Ok(state_directory()?.join("astesia-settings.json"))
}

/// Reads the node's device settings. A missing or unreadable file gives the defaults.
pub(crate) fn settings() -> TailnetSettings {
    settings_path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_settings(settings: &TailnetSettings) -> anyhow::Result<()> {
    let path = settings_path()?;
    crate::platform::write_json_atomically(&path, settings)
        .map_err(|error| anyhow::anyhow!("无法保存 Tailscale 设置 {}：{error}", path.display()))
}

/// The machine name used when none is set: this computer's name plus
/// `astesia`, so each computer running Astesia is its own tailnet device.
pub(crate) fn default_hostname() -> String {
    let suffix = NodeRole::current().hostname_suffix();
    let host = gethostname::gethostname();
    match host.to_str().and_then(normalize_hostname) {
        Some(host) => format!("{host}-{suffix}"),
        None => suffix.to_string(),
    }
}

/// Turns user input into a DNS label: lowercase letters, digits and dashes,
/// at most 63 characters. Returns `None` when nothing usable is left.
pub(crate) fn normalize_hostname(input: &str) -> Option<String> {
    let first_label = input.trim().split('.').next().unwrap_or_default();
    let mut label = String::new();
    for character in first_label.chars() {
        let character = character.to_ascii_lowercase();
        if character.is_ascii_alphanumeric() {
            label.push(character);
        } else if !label.ends_with('-') {
            label.push('-');
        }
    }
    let label: String = label.trim_matches('-').chars().take(63).collect();
    let label = label.trim_end_matches('-').to_string();
    (!label.is_empty()).then_some(label)
}

/// Saves the machine name and, when the node is running, renames the device.
/// Returns the name now in use.
pub(crate) async fn set_hostname(input: &str) -> anyhow::Result<String> {
    let default = default_hostname();
    let mut current = settings();
    current.hostname = normalize_hostname(input).filter(|name| *name != default);
    save_settings(&current)?;
    let hostname = current.hostname.unwrap_or(default);
    embedded::apply_hostname(&hostname).await?;
    Ok(hostname)
}

/// Saves whether the node starts with Astesia, and starts it when turned on.
pub(crate) async fn set_connect_on_launch(enabled: bool) -> anyhow::Result<()> {
    let mut current = settings();
    current.connect_on_launch = enabled;
    save_settings(&current)?;
    if enabled {
        start().await?;
    }
    Ok(())
}

/// Starts the node without waiting for sign-in. It only comes online once
/// it has been signed in.
pub(crate) async fn start() -> anyhow::Result<()> {
    embedded::start().await
}

/// Reads the embedded node's state without starting it.
pub(crate) async fn state() -> TailnetState {
    embedded::state().await
}

/// Starts the embedded node and waits for it to be signed in, opening the
/// sign-in page when needed.
pub(crate) async fn sign_in() -> anyhow::Result<()> {
    embedded::sign_in().await
}

/// Signs the embedded node out of its tailnet.
pub(crate) async fn sign_out() -> anyhow::Result<()> {
    embedded::sign_out().await
}

/// Returns the configuration a driver should use to reach `config`.
///
/// Connections that do not use Tailscale are returned unchanged. Tailscale
/// connections get a loopback host and port that forward to the original
/// target over the embedded node.
pub(crate) async fn route(config: ConnectionConfig) -> anyhow::Result<ConnectionConfig> {
    if !config.tailscale || config.db_type == DbType::SQLite {
        return Ok(config);
    }
    route_through_tailnet(config).await
}

async fn route_through_tailnet(config: ConnectionConfig) -> anyhow::Result<ConnectionConfig> {
    if !is_available() {
        anyhow::bail!("当前版本不包含内置 Tailscale（仅 macOS 与 Linux 版本支持），请关闭该连接的“通过内置 Tailscale 连接”")
    }
    let target = TailnetTarget::from_config(&config)?;
    let port = embedded::forward(&target.host, target.port).await?;
    Ok(target.rewrite(config, port))
}

/// The host and port a connection reaches on the tailnet.
#[derive(Debug, PartialEq, Eq)]
struct TailnetTarget {
    host: String,
    port: u16,
    /// ClickHouse keeps its URL scheme and path when the host is rewritten.
    clickhouse_url: Option<reqwest::Url>,
}

impl TailnetTarget {
    fn from_config(config: &ConnectionConfig) -> anyhow::Result<Self> {
        if config.db_type == DbType::ClickHouse {
            return Self::from_clickhouse(config);
        }
        let host = config.host.trim();
        if host.is_empty() {
            anyhow::bail!("Tailscale 目标主机不能为空");
        }
        if config.port == 0 {
            anyhow::bail!("Tailscale 目标端口不能为 0");
        }
        Ok(Self {
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            port: config.port,
            clickhouse_url: None,
        })
    }

    fn from_clickhouse(config: &ConnectionConfig) -> anyhow::Result<Self> {
        let host = config.host.trim().trim_end_matches('/');
        let endpoint = if host.starts_with("http://") || host.starts_with("https://") {
            host.to_string()
        } else {
            format!("http://{host}")
        };
        let url = reqwest::Url::parse(&endpoint)
            .map_err(|_| anyhow::anyhow!("无效的 ClickHouse HTTP 地址: {endpoint}"))?;
        if url.scheme() == "https" {
            anyhow::bail!("通过内置 Tailscale 连接 ClickHouse 暂不支持 HTTPS，请改用 HTTP 端口（流量已由 Tailscale 加密）");
        }
        let target_host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("ClickHouse 主机不能为空"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = if config.port > 0 {
            config.port
        } else {
            url.port_or_known_default().unwrap_or(8123)
        };
        Ok(Self {
            host: target_host,
            port,
            clickhouse_url: Some(url),
        })
    }

    fn rewrite(&self, mut config: ConnectionConfig, local_port: u16) -> ConnectionConfig {
        config.host = match &self.clickhouse_url {
            Some(url) => {
                let mut url = url.clone();
                let _ = url.set_host(Some("127.0.0.1"));
                let _ = url.set_port(None);
                url.as_str().trim_end_matches('/').to_string()
            }
            None => "127.0.0.1".to_string(),
        };
        config.port = local_port;
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_are_normalized_to_one_dns_label() {
        assert_eq!(
            normalize_hostname("Alex's MacBook Pro.local").as_deref(),
            Some("alex-s-macbook-pro")
        );
        assert_eq!(
            normalize_hostname("  db_gateway  ").as_deref(),
            Some("db-gateway")
        );
        assert_eq!(normalize_hostname("---").as_deref(), None);
        assert_eq!(normalize_hostname("").as_deref(), None);
        assert_eq!(
            normalize_hostname(&"a".repeat(80)).map(|name| name.len()),
            Some(63)
        );
    }

    #[test]
    fn settings_tolerate_missing_fields() {
        let settings: TailnetSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, TailnetSettings::default());
        let settings: TailnetSettings =
            serde_json::from_str(r#"{"hostname":"db-box","connect_on_launch":true}"#).unwrap();
        assert_eq!(settings.effective_hostname(), "db-box");
        assert!(settings.connect_on_launch);
    }

    fn config(db_type: DbType, host: &str, port: u16) -> ConnectionConfig {
        ConnectionConfig {
            id: "id".to_string(),
            name: "name".to_string(),
            db_type,
            host: host.to_string(),
            port,
            username: String::new(),
            password: String::new(),
            database: None,
            color: None,
            tailscale: true,
        }
    }

    #[tokio::test]
    async fn connections_without_tailscale_are_unchanged() {
        let mut direct = config(DbType::PostgreSQL, "db.internal", 5432);
        direct.tailscale = false;
        let routed = route(direct).await.expect("route");
        assert_eq!(routed.host, "db.internal");
        assert_eq!(routed.port, 5432);
    }

    #[tokio::test]
    async fn sqlite_ignores_the_tailscale_flag() {
        let routed = route(config(DbType::SQLite, "/tmp/a.db", 0))
            .await
            .expect("route");
        assert_eq!(routed.host, "/tmp/a.db");
    }

    #[test]
    fn plain_targets_are_rewritten_to_loopback() {
        let original = config(DbType::PostgreSQL, " pg.tail1234.ts.net ", 5432);
        let target = TailnetTarget::from_config(&original).expect("target");
        assert_eq!(target.host, "pg.tail1234.ts.net");
        assert_eq!(target.port, 5432);
        let routed = target.rewrite(original, 40000);
        assert_eq!(routed.host, "127.0.0.1");
        assert_eq!(routed.port, 40000);
        assert!(routed.tailscale);
    }

    #[test]
    fn ipv6_brackets_are_removed_from_the_dial_target() {
        let target =
            TailnetTarget::from_config(&config(DbType::MySQL, "[fd7a:115c:a1e0::1]", 3306))
                .expect("target");
        assert_eq!(target.host, "fd7a:115c:a1e0::1");
    }

    #[test]
    fn clickhouse_keeps_its_scheme_and_path() {
        let original = config(DbType::ClickHouse, "http://ch.internal/proxy", 0);
        let target = TailnetTarget::from_config(&original).expect("target");
        assert_eq!(target.host, "ch.internal");
        assert_eq!(target.port, 80);
        let routed = target.rewrite(original, 40001);
        assert_eq!(routed.host, "http://127.0.0.1/proxy");
        assert_eq!(routed.port, 40001);

        let bare = TailnetTarget::from_config(&config(DbType::ClickHouse, "ch.internal", 8123))
            .expect("bare host");
        assert_eq!(bare.port, 8123);
    }

    #[test]
    fn clickhouse_https_is_rejected() {
        let error = TailnetTarget::from_config(&config(DbType::ClickHouse, "https://ch", 8443))
            .expect_err("https");
        assert!(error.to_string().contains("HTTPS"));
    }

    #[test]
    fn missing_host_or_port_is_rejected() {
        assert!(TailnetTarget::from_config(&config(DbType::Redis, " ", 6379)).is_err());
        assert!(TailnetTarget::from_config(&config(DbType::Redis, "redis", 0)).is_err());
    }
}
