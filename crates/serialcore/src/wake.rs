//! The wake signal: how a background thread tells the UI "look at your channel".
//!
//! Reader threads and the port enumerator produce events the UI must see, but
//! this crate must never depend on `egui` (see the crate docs). A [`Wake`] is a
//! one-way callback that lets a thread announce new state without knowing what
//! is listening: the desktop app hands in a closure that requests a repaint, a
//! headless consumer hands in [`Wake::none`].
//!
//! This is what lets the UI stay asleep. Without it the app's only way to learn
//! about a batch is to redraw and poll — i.e. to never idle at all.

use std::sync::Arc;

/// A callback invoked whenever a background thread produces something the UI
/// should redraw for.
///
/// Cheap to clone. The callback runs on the producing thread — including the
/// reader's read loop — so it must be thread-safe and must not block.
#[derive(Clone, Default)]
pub struct Wake(Option<Arc<dyn Fn() + Send + Sync>>);

impl Wake {
    /// Wrap a callback.
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Wake {
        Wake(Some(Arc::new(f)))
    }

    /// A wake that does nothing, for headless consumers and tests.
    pub fn none() -> Wake {
        Wake(None)
    }

    /// Announce that new state is available.
    pub fn signal(&self) {
        if let Some(f) = &self.0 {
            f();
        }
    }
}

impl std::fmt::Debug for Wake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Wake(set)" } else { "Wake(none)" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn signal_invokes_callback() {
        let hits = Arc::new(AtomicUsize::new(0));
        let w = Wake::new({
            let hits = hits.clone();
            move || {
                hits.fetch_add(1, Ordering::Relaxed);
            }
        });
        w.signal();
        w.clone().signal();
        assert_eq!(hits.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn none_is_inert() {
        Wake::none().signal();
        Wake::default().signal();
    }
}
