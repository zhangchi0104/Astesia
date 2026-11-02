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

use std::sync::atomic::{AtomicBool, Ordering};

use crate::db::{ConnectionConfig, DbType};

#[cfg(all(unix, feature = "tailscale"))]
mod embedded;

static BROWSER_LOGIN: AtomicBool = AtomicBool::new(false);

/// Lets the embedded node open the Tailscale sign-in page in the browser.
///
/// The desktop app enables this. Headless processes such as the MCP server
/// leave it off and report the sign-in URL in the connection error instead.
pub(crate) fn enable_browser_login() {
    BROWSER_LOGIN.store(true, Ordering::Release);
}

fn browser_login_enabled() -> bool {
    BROWSER_LOGIN.load(Ordering::Acquire)
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
}

/// Reads the embedded node's state without starting it.
pub(crate) async fn state() -> TailnetState {
    #[cfg(all(unix, feature = "tailscale"))]
    {
        embedded::state().await
    }
    #[cfg(not(all(unix, feature = "tailscale")))]
    {
        TailnetState::Unavailable
    }
}

/// Starts the embedded node and waits for it to be signed in, opening the
/// sign-in page when needed.
pub(crate) async fn sign_in() -> anyhow::Result<()> {
    #[cfg(all(unix, feature = "tailscale"))]
    {
        embedded::sign_in().await
    }
    #[cfg(not(all(unix, feature = "tailscale")))]
    {
        anyhow::bail!("当前版本不包含内置 Tailscale")
    }
}

/// Signs the embedded node out of its tailnet.
pub(crate) async fn sign_out() -> anyhow::Result<()> {
    #[cfg(all(unix, feature = "tailscale"))]
    {
        embedded::sign_out().await
    }
    #[cfg(not(all(unix, feature = "tailscale")))]
    {
        anyhow::bail!("当前版本不包含内置 Tailscale")
    }
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

#[cfg(all(unix, feature = "tailscale"))]
async fn route_through_tailnet(config: ConnectionConfig) -> anyhow::Result<ConnectionConfig> {
    let target = TailnetTarget::from_config(&config)?;
    let port = embedded::forward(&target.host, target.port).await?;
    Ok(target.rewrite(config, port))
}

#[cfg(not(all(unix, feature = "tailscale")))]
async fn route_through_tailnet(_config: ConnectionConfig) -> anyhow::Result<ConnectionConfig> {
    anyhow::bail!("当前版本不包含内置 Tailscale（仅 macOS 与 Linux 版本支持），请关闭该连接的“通过内置 Tailscale 连接”")
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
