//! Shares the desktop app's Tailscale node with other Astesia processes.
//!
//! tsnet state cannot be shared between processes, so only the desktop app
//! runs the signed-in node. It serves a small loopback API and records its
//! address and a random token in a discovery file next to the node's state.
//! Other processes on this machine, such as the MCP server, ask that API for a
//! loopback forwarder instead of starting a node of their own. That way they
//! reuse the desktop app's sign-in and need no separate login.

use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        Request, Response, StatusCode,
    },
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::platform::application_data_directory;

const FORWARD_PATH: &str = "/tailnet/forward";
/// The node may need to start and dial the target, each of which can take up
/// to 30 seconds on the desktop side.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
/// A stale discovery file points at a closed port; give up on it quickly.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the desktop app tells other processes how to reach its node.
#[derive(Debug, Deserialize, Serialize)]
struct Discovery {
    endpoint: String,
    token: String,
    pid: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct ForwardRequest {
    host: String,
    port: u16,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ForwardResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The discovery file lives in the desktop node's state directory, whichever
/// process is asking.
pub(super) fn discovery_path() -> anyhow::Result<PathBuf> {
    let base =
        application_data_directory().ok_or_else(|| anyhow::anyhow!("无法确定 Astesia 数据目录"))?;
    Ok(base
        .join(super::NodeRole::Desktop.state_directory_name())
        .join("shared-connection.json"))
}

type ForwardFuture = Pin<Box<dyn Future<Output = anyhow::Result<u16>> + Send>>;
type Forwarder = Arc<dyn Fn(String, u16) -> ForwardFuture + Send + Sync>;

#[derive(Clone)]
struct ServerContext {
    bearer: Arc<str>,
    forward: Forwarder,
}

/// Starts serving the desktop node to other processes and writes the
/// discovery file. The server runs for the rest of the process.
pub(super) async fn serve<F, Fut>(path: PathBuf, forward: F) -> anyhow::Result<()>
where
    F: Fn(String, u16) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<u16>> + Send + 'static,
{
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let forward: Forwarder = Arc::new(move |host, port| Box::pin(forward(host, port)));
    let context = ServerContext {
        bearer: Arc::from(format!("Bearer {token}")),
        forward,
    };
    let router = Router::new()
        .route(FORWARD_PATH, post(handle_forward))
        .with_state(context);

    // The temporary file behind this write is created with mode 0600, so
    // only this user can read the token.
    crate::platform::write_json_atomically(
        &path,
        &Discovery {
            endpoint: format!("http://{address}{FORWARD_PATH}"),
            token,
            pid: std::process::id(),
        },
    )
    .map_err(|error| anyhow::anyhow!("无法写入 {}：{error}", path.display()))?;

    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            log::warn!("Shared Tailscale connection server stopped: {error}");
        }
    });
    Ok(())
}

const MAX_REQUEST_BYTES: usize = 16 * 1024;

async fn handle_forward(
    State(context): State<ServerContext>,
    request: Request<Body>,
) -> Response<Body> {
    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|actual| {
            crate::mcp_auth::constant_time_eq(actual.as_bytes(), context.bearer.as_bytes())
        });
    if !authorized {
        return failure(StatusCode::UNAUTHORIZED, "Unauthorized".into());
    }
    let request = match to_bytes(request.into_body(), MAX_REQUEST_BYTES)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<ForwardRequest>(&body).ok())
    {
        Some(request) => request,
        None => return failure(StatusCode::BAD_REQUEST, "Invalid request".into()),
    };
    let host = request.host.trim().to_string();
    if host.is_empty() || request.port == 0 {
        return failure(
            StatusCode::BAD_REQUEST,
            "Tailscale 目标主机和端口不能为空".into(),
        );
    }
    match (context.forward)(host, request.port).await {
        Ok(port) => respond(
            StatusCode::OK,
            &ForwardResponse {
                port: Some(port),
                error: None,
            },
        ),
        Err(error) => failure(StatusCode::BAD_GATEWAY, format!("{error:#}")),
    }
}

fn failure(status: StatusCode, error: String) -> Response<Body> {
    respond(
        status,
        &ForwardResponse {
            port: None,
            error: Some(error),
        },
    )
}

fn respond(status: StatusCode, body: &ForwardResponse) -> Response<Body> {
    let body = serde_json::to_vec(body).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("valid shared Tailscale response")
}

/// Asks the desktop app for a loopback port that forwards to `host:port`
/// over its node.
///
/// Returns `Ok(None)` when no desktop app is reachable (no discovery file, or
/// a stale one), so the caller can fall back. An error means the desktop app
/// answered but could not reach the target, for example because its node is
/// not signed in.
pub(super) async fn forward(
    path: &std::path::Path,
    host: &str,
    port: u16,
) -> anyhow::Result<Option<u16>> {
    let Some(discovery) = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Discovery>(&bytes).ok())
    else {
        return Ok(None);
    };

    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let response = match client
        .post(&discovery.endpoint)
        .bearer_auth(&discovery.token)
        .json(&ForwardRequest {
            host: host.to_string(),
            port,
        })
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) if error.is_connect() => return Ok(None),
        Err(error) if error.is_timeout() => {
            anyhow::bail!("等待 Astesia 桌面应用建立 Tailscale 连接超时")
        }
        Err(error) => {
            log::warn!("Shared Tailscale connection request failed: {error}");
            return Ok(None);
        }
    };
    // A token mismatch means the file is left over from another app instance.
    if response.status() == StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    let Ok(body) = response.json::<ForwardResponse>().await else {
        return Ok(None);
    };
    match (body.port, body.error) {
        (Some(port), _) => Ok(Some(port)),
        (None, Some(error)) => Err(anyhow::anyhow!(error)),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clients_get_the_desktop_forwarder_port() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared-connection.json");
        serve(path.clone(), |host, port| async move {
            anyhow::ensure!(host == "pg.example.ts.net" && port == 5432);
            Ok(40_123)
        })
        .await
        .expect("serve");

        let port = forward(&path, "pg.example.ts.net", 5432)
            .await
            .expect("forward");
        assert_eq!(port, Some(40_123));
    }

    #[tokio::test]
    async fn desktop_errors_are_reported_to_the_client() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared-connection.json");
        serve(path.clone(), |_, _| async { anyhow::bail!("尚未登录") })
            .await
            .expect("serve");

        let error = forward(&path, "db", 5432).await.expect_err("not signed in");
        assert_eq!(error.to_string(), "尚未登录");
    }

    #[tokio::test]
    async fn missing_or_stale_discovery_falls_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared-connection.json");
        assert_eq!(forward(&path, "db", 5432).await.unwrap(), None);

        // A closed port, as left behind by an app that has quit.
        let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = closed.local_addr().unwrap();
        drop(closed);
        crate::platform::write_json_atomically(
            &path,
            &Discovery {
                endpoint: format!("http://{address}{FORWARD_PATH}"),
                token: "stale".into(),
                pid: 1,
            },
        )
        .unwrap();
        assert_eq!(forward(&path, "db", 5432).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_wrong_token_is_rejected_and_falls_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared-connection.json");
        serve(path.clone(), |_, _| async { Ok(1) }).await.unwrap();
        let mut discovery: Discovery =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        discovery.token = "wrong".into();
        crate::platform::write_json_atomically(&path, &discovery).unwrap();
        assert_eq!(forward(&path, "db", 5432).await.unwrap(), None);
    }
}
