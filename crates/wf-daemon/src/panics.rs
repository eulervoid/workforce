//! A panic in any task or thread of the daemon stops the daemon. A task that died would leave a
//! daemon that answers on its socket but no longer dispatches, merges or stores; one that exits
//! is seen not to run, and its sessions resume when it starts again.

use tokio::sync::watch;

/// The first panic of the process, once there is one.
pub struct Panics(watch::Receiver<Option<String>>);

/// Reports every panic of the process through the log, in place of the default report, and
/// keeps the first one for [`Panics::first`].
pub fn watch_panics() -> Panics {
    let (sender, receiver) = watch::channel(None);
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let message = info.payload_as_str().unwrap_or("a panic without a message");
        let report = match info.location() {
            Some(location) => format!(
                "thread '{}' panicked at {location}: {message}",
                thread.name().unwrap_or("unnamed")
            ),
            None => format!(
                "thread '{}' panicked: {message}",
                thread.name().unwrap_or("unnamed")
            ),
        };
        tracing::error!("{report}");
        sender.send_if_modified(|first| {
            let is_first = first.is_none();
            if is_first {
                *first = Some(report);
            }
            is_first
        });
    }));
    Panics(receiver)
}

impl Panics {
    /// Completes with the report of the first panic.
    pub async fn first(&mut self) -> String {
        let first = self
            .0
            .wait_for(Option::is_some)
            .await
            .map(|first| first.clone().unwrap_or_default());
        match first {
            Ok(report) => report,
            // The hook holds the sender for as long as it is installed.
            Err(_) => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicking_task_is_reported() {
        let mut panics = watch_panics();
        let task = tokio::spawn(async { panic!("a bug in a task") });
        assert!(task.await.is_err());
        let report = tokio::time::timeout(std::time::Duration::from_secs(5), panics.first())
            .await
            .expect("the panic is reported");
        assert!(report.contains("a bug in a task"), "{report}");
        assert!(report.contains("panics.rs"), "{report}");
        let _ = std::panic::take_hook();
    }
}
