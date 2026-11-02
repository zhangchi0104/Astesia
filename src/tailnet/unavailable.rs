//! Stand-in for the embedded node in builds without the `tailscale` feature.

use super::TailnetState;

fn unavailable<T>() -> anyhow::Result<T> {
    anyhow::bail!("当前版本不包含内置 Tailscale")
}

pub(super) async fn state() -> TailnetState {
    TailnetState::Unavailable
}

pub(super) async fn start() -> anyhow::Result<()> {
    unavailable()
}

pub(super) async fn sign_in() -> anyhow::Result<()> {
    unavailable()
}

pub(super) async fn sign_out() -> anyhow::Result<()> {
    unavailable()
}

pub(super) async fn apply_hostname(_hostname: &str) -> anyhow::Result<()> {
    Ok(())
}

pub(super) async fn forward(_host: &str, _port: u16) -> anyhow::Result<u16> {
    unavailable()
}
