//! Bounded MPSC write buffer backed by a lock-free `ArrayQueue`.
//!
//! Write operations are enqueued here so the hot write path never blocks on
//! the policy mutex.  A maintenance pass drains the queue and applies all
//! pending operations under a single lock acquisition.
//!
//! If the queue is full when a push is attempted, the operation is returned
//! to the caller as `Err(op)` so it can be applied synchronously — write
//! operations must never be lost because they drive capacity accounting.

use crossbeam_queue::ArrayQueue;
use std::time::Instant;

/// Bounded capacity of the write queue.  128 is Caffeine's default.
const WRITE_BUFFER_CAPACITY: usize = 128;

/// Operations deferred for policy maintenance.
pub enum WriteOp<K> {
    Add {
        key: K,
        weight: u64,
        /// Absolute expiry time, if any.  `None` = immortal.
        expires_at: Option<Instant>,
    },
    Update {
        key: K,
        old_weight: u64,
        new_weight: u64,
        /// New absolute expiry time (re-set on value update).
        expires_at: Option<Instant>,
    },
    Remove {
        key: K,
    },
    /// TTI: an existing entry was read; reset its expiry deadline.
    ///
    /// Does **not** change the entry's weight or position in the policy.
    Reschedule {
        key: K,
        expires_at: Instant,
    },
}

/// Bounded MPSC write buffer.
///
/// Multiple producer threads may call [`push`] concurrently.  A single
/// consumer (the maintenance thread) drains the queue via [`drain`].
///
/// [`push`]: WriteBuffer::push
/// [`drain`]: WriteBuffer::drain
pub struct WriteBuffer<K> {
    queue: ArrayQueue<WriteOp<K>>,
}

impl<K: Send> WriteBuffer<K> {
    /// Creates a new write buffer with the default capacity.
    pub fn new() -> Self {
        WriteBuffer {
            queue: ArrayQueue::new(WRITE_BUFFER_CAPACITY),
        }
    }

    /// Enqueues `op`.
    ///
    /// Returns `Ok(())` if the operation was accepted, or `Err(op)` if the
    /// queue is full.  The caller **must not drop** a returned `Err`.
    #[inline]
    pub fn push(&self, op: WriteOp<K>) -> Result<(), WriteOp<K>> {
        self.queue.push(op)
    }

    /// Returns `true` when the queue has reached its capacity.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.queue.is_full()
    }

    /// Drains all pending operations into `out`.
    ///
    /// Called exclusively from the maintenance thread.
    pub fn drain(&self, out: &mut Vec<WriteOp<K>>) {
        while let Some(op) = self.queue.pop() {
            out.push(op);
        }
    }
}

impl<K: Send> Default for WriteBuffer<K> {
    fn default() -> Self {
        Self::new()
    }
}
