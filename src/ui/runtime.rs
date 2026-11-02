use gpui_kit::{App, Global};
use std::future::Future;
struct Runtime(tokio::runtime::Runtime);
impl Global for Runtime {}
pub(super) fn init(cx: &mut App) {
    cx.set_global(Runtime(
        tokio::runtime::Runtime::new().expect("failed to create database task runtime"),
    ));
}
pub(super) fn spawn<F>(cx: &App, future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    cx.global::<Runtime>().0.spawn(future)
}

/// Runs a fallible future on the runtime and returns its result, with the
/// error (or a panic in the task) formatted for display.
pub(super) fn spawn_result<T, F>(
    cx: &App,
    future: F,
) -> impl Future<Output = Result<T, String>> + use<T, F>
where
    T: Send + 'static,
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
{
    let task = spawn(cx, future);
    async move {
        match task.await {
            Ok(result) => result.map_err(|error| format!("{error:#}")),
            Err(error) => Err(error.to_string()),
        }
    }
}
