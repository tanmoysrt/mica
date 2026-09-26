use anyhow::Result;

/// Runs file I/O on tokio's blocking pool, so it does not stall async tasks.
pub async fn blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work).await?
}
