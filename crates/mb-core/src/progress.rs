//! Shared progress / logging primitives.

use std::sync::Arc;

pub use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

/// Log sink passed into long-running jobs.
pub type LogFn = Arc<dyn Fn(Level, String) + Send + Sync>;

pub fn null_log() -> LogFn {
    Arc::new(|_, _| {})
}

/// Run `fut` unless `cancel` fires first.
pub async fn cancellable<T>(
    cancel: &CancellationToken,
    fut: impl std::future::Future<Output = crate::Result<T>>,
) -> crate::Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(crate::Error::Cancelled),
        r = fut => r,
    }
}
