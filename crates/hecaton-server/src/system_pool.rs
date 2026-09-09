//! The daemon pool's single owner (Spec F). One actor, one writer: fleet
//! actors used to install the system level from N parallel threads.

use std::sync::Arc;
use std::time::Duration;

use hecaton_core::SystemToolchain;
use tokio::sync::watch;

/// What the daemon pool is doing. Read live at the top of every fleet pass —
/// never latched (Spec F, F-4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPoolState {
    Pending,
    Ready,
    Unready { reason: String },
}

#[derive(Debug, Clone, Copy)]
pub struct SystemPoolConfig {
    /// How often the pool is re-checked. A matching marker makes this a file
    /// read and a hash, so it is cheap.
    pub tick: Duration,
    /// Per-attempt budget. Carried here so the policy lives with the actor,
    /// but not yet wired anywhere: the runtime enforces a fixed 600s via its
    /// own `SYSTEM_POOL_INSTALL_TIMEOUT` constant
    /// (`hecaton-runtime/src/materializer.rs`), which happens to match this
    /// field's default. Threading this value down to replace that constant
    /// is a follow-up, out of scope for this task.
    pub attempt_timeout: Duration,
}

impl Default for SystemPoolConfig {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(30),
            attempt_timeout: Duration::from_secs(600),
        }
    }
}

/// Runs until aborted. One attempt per tick, sequential, retried forever; a
/// failure is published, never fatal (F-7).
pub fn spawn(
    toolchain: Arc<dyn SystemToolchain>,
    tx: watch::Sender<SystemPoolState>,
    config: SystemPoolConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let tc = toolchain.clone();
            // The port is synchronous; a pass must not block the runtime.
            let outcome = tokio::task::spawn_blocking(move || tc.ensure_system_pool()).await;
            let next = match outcome {
                Ok(Ok(())) => SystemPoolState::Ready,
                Ok(Err(e)) => SystemPoolState::Unready {
                    reason: e.to_string(),
                },
                Err(e) => SystemPoolState::Unready {
                    reason: format!("system pool task failed: {e}"),
                },
            };
            if let SystemPoolState::Unready { reason } = &next {
                tracing::warn!(%reason, "daemon mise pool is not ready");
            }
            // `send_if_modified` keeps `changed()` meaningful: only a real
            // transition wakes the fleet actors waiting on it.
            tx.send_if_modified(|cur| {
                if *cur == next {
                    false
                } else {
                    *cur = next.clone();
                    true
                }
            });
            tokio::time::sleep(config.tick).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_core::fakes::FakeSystemToolchain;
    use std::sync::Arc;
    use std::time::Duration;

    fn fast() -> SystemPoolConfig {
        SystemPoolConfig {
            tick: Duration::from_millis(20),
            attempt_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn a_healthy_pool_goes_pending_then_ready_and_stays_ready() {
        let tc = Arc::new(FakeSystemToolchain::ready());
        let (tx, mut rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        assert_eq!(*rx.borrow(), SystemPoolState::Pending);
        let handle = spawn(tc, tx, fast());

        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), SystemPoolState::Ready);

        // Several more ticks must not disturb it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*rx.borrow(), SystemPoolState::Ready);
        handle.abort();
    }

    #[tokio::test]
    async fn a_failing_pool_publishes_the_reason_and_recovers_without_dying() {
        let tc = Arc::new(FakeSystemToolchain::failing(2));
        let (tx, mut rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        let handle = spawn(tc.clone(), tx, fast());

        rx.changed().await.unwrap();
        match &*rx.borrow() {
            SystemPoolState::Unready { reason } => {
                assert!(!reason.is_empty(), "an unready state must carry a reason")
            }
            other => panic!("expected Unready first, got {other:?}"),
        }

        // It keeps trying and eventually succeeds — no fatal path (F-7).
        loop {
            rx.changed().await.unwrap();
            if *rx.borrow() == SystemPoolState::Ready {
                break;
            }
        }
        assert!(tc.calls() >= 3, "one attempt per tick until it succeeds");
        assert!(!handle.is_finished(), "the actor must not exit on failure");
        handle.abort();
    }

    #[tokio::test]
    async fn readiness_drops_again_when_a_later_attempt_fails() {
        // Ready, then failing: proves readiness is live, not a latch (F-4).
        let tc = Arc::new(FakeSystemToolchain::ready_then_failing(1));
        let (tx, mut rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        let handle = spawn(tc, tx, fast());

        loop {
            rx.changed().await.unwrap();
            if *rx.borrow() == SystemPoolState::Ready {
                break;
            }
        }
        loop {
            rx.changed().await.unwrap();
            if matches!(*rx.borrow(), SystemPoolState::Unready { .. }) {
                break;
            }
        }
        handle.abort();
    }

    #[tokio::test]
    async fn a_permanently_failing_pool_never_terminates_the_actor() {
        let tc = Arc::new(FakeSystemToolchain::always_failing());
        let (tx, _rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        let handle = spawn(tc.clone(), tx, fast());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!handle.is_finished(), "never fatal (F-7)");
        assert!(tc.calls() >= 3, "it keeps retrying");
        handle.abort();
    }
}
