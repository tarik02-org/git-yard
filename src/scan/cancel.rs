//! Cancellation that flows from the engine down to each job, and into gix
//! through a plain atomic flag.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

#[derive(Debug, Default)]
struct Node {
    flag: Arc<AtomicBool>,
    children: Mutex<Vec<Weak<Node>>>,
}

#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<Node>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// A token cancelled with this one, or on its own.
    pub fn child(&self) -> CancelToken {
        let child = CancelToken::new();
        let mut children = self
            .0
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        children.retain(|weak| weak.strong_count() > 0);
        children.push(Arc::downgrade(&child.0));
        drop(children);
        if self.is_cancelled() {
            child.cancel();
        }
        child
    }

    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::Release);
        let children: Vec<_> = self
            .0
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for child in children {
            CancelToken(child).cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::Acquire)
    }

    /// The flag gix polls as its interrupt.
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.0.flag.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_a_parent_reaches_existing_and_new_children() {
        let root = CancelToken::new();
        let job = root.child();
        let other = root.child();
        job.cancel();
        assert!(!root.is_cancelled() && !other.is_cancelled());
        root.cancel();
        assert!(other.flag().load(Ordering::Relaxed));
        assert!(root.child().is_cancelled());
    }
}
