//! Priority Queue for Download Scheduling
//!
//! Implements a priority-based queue for managing concurrent downloads.
//! Downloads are scheduled based on priority (Critical > High > Normal > Low),
//! with FIFO ordering within the same priority level.

use crate::protocol::DownloadId;
use parking_lot::Mutex;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

// Re-export DownloadPriority for backward compatibility
pub use crate::protocol::DownloadPriority;

/// Entry in the priority queue
#[derive(Debug, Clone, Eq, PartialEq)]
struct QueueEntry {
    id: DownloadId,
    priority: DownloadPriority,
    /// Sequence number for FIFO ordering within same priority
    sequence: u64,
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Higher priority first, then lower sequence (earlier) first
        match self.priority.cmp(&other.priority) {
            std::cmp::Ordering::Equal => other.sequence.cmp(&self.sequence), // Lower sequence = higher priority
            other => other,
        }
    }
}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A permit that allows a download to proceed
/// When dropped, releases the slot back to the queue
pub struct PriorityPermit {
    // Option so Drop can release the semaphore permit *before* notifying:
    // fields drop after the Drop body, and a waiter woken before the permit
    // returns would fail try_acquire and sleep with no further notification.
    permit: Option<OwnedSemaphorePermit>,
    id: DownloadId,
    queue: Arc<PriorityQueue>,
}

impl Drop for PriorityPermit {
    fn drop(&mut self) {
        // Remove from active set
        self.queue.inner.lock().active.remove(&self.id);
        // Return the slot to the semaphore before waking waiters
        drop(self.permit.take());
        // Notify all waiting downloads so the highest priority can acquire
        self.queue.notify.notify_waiters();
    }
}

/// Internal state of the priority queue
struct PriorityQueueInner {
    /// Downloads waiting for a slot
    waiting: BinaryHeap<QueueEntry>,
    /// Currently active downloads
    active: HashMap<DownloadId, DownloadPriority>,
    /// Priority of each waiting download (for quick lookup)
    waiting_priorities: HashMap<DownloadId, DownloadPriority>,
}

/// Priority-based download queue
///
/// Manages concurrent download slots with priority ordering.
/// Higher priority downloads are started before lower priority ones,
/// with FIFO ordering within the same priority level.
pub struct PriorityQueue {
    /// Semaphore for limiting concurrent downloads
    semaphore: Arc<Semaphore>,
    /// Current concurrency ceiling for new acquisitions.
    max_concurrent: AtomicUsize,
    /// Internal queue state
    inner: Mutex<PriorityQueueInner>,
    /// Sequence counter for FIFO ordering
    sequence: AtomicU64,
    /// Notification for waiting downloads
    notify: Notify,
}

impl PriorityQueue {
    /// Create a new priority queue with the given concurrency limit
    pub fn new(max_concurrent: usize) -> Arc<Self> {
        Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent: AtomicUsize::new(max_concurrent),
            inner: Mutex::new(PriorityQueueInner {
                waiting: BinaryHeap::new(),
                active: HashMap::new(),
                waiting_priorities: HashMap::new(),
            }),
            sequence: AtomicU64::new(0),
            notify: Notify::new(),
        })
    }

    /// Acquire a permit for the download to proceed (blocking).
    ///
    /// This method **adds the download to the waiting queue** and blocks until:
    /// 1. A slot becomes available, AND
    /// 2. This download is the highest priority in the waiting queue
    ///
    /// The download remains in the queue until a permit is granted, ensuring fair
    /// ordering based on priority and arrival time (FIFO within same priority).
    ///
    /// # Difference from `try_acquire`
    /// - `acquire`: Adds to queue, waits for turn, guarantees eventual permit
    /// - `try_acquire`: Does NOT add to queue, immediate success or failure
    ///
    /// Use `acquire` for downloads that should wait their turn.
    /// Use `try_acquire` for opportunistic slot acquisition (e.g., resuming paused downloads).
    pub async fn acquire(
        self: &Arc<Self>,
        id: DownloadId,
        priority: DownloadPriority,
    ) -> PriorityPermit {
        // Add to waiting queue
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        {
            let mut inner = self.inner.lock();
            inner.waiting.push(QueueEntry {
                id,
                priority,
                sequence,
            });
            inner.waiting_priorities.insert(id, priority);
        }

        loop {
            // Register for notifications BEFORE checking the condition:
            // `notify_waiters` only wakes futures that are already enabled, so a
            // permit released between the check and the await would otherwise be
            // lost and this task could hang forever.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            // Check if we're next in line
            {
                let mut inner = self.inner.lock();
                if let Some(next) = inner.waiting.peek() {
                    if next.id == id
                        && inner.active.len() < self.max_concurrent.load(Ordering::Relaxed)
                    {
                        // `try_acquire_owned` is synchronous, so we hold the lock
                        // across it: peek and pop stay atomic, and a concurrent
                        // higher-priority push can't make us pop the wrong entry.
                        if let Ok(permit) = self.semaphore.clone().try_acquire_owned() {
                            inner.waiting.pop();
                            inner.waiting_priorities.remove(&id);
                            inner.active.insert(id, priority);

                            return PriorityPermit {
                                permit: Some(permit),
                                id,
                                queue: Arc::clone(self),
                            };
                        }
                    }
                }
            }

            // Wait for notification (either slot freed or priority changed)
            notified.await;
        }
    }

    /// Try to acquire a permit immediately without waiting (non-blocking).
    ///
    /// This method does **NOT** add the download to the waiting queue. It either
    /// succeeds immediately or returns `None`.
    ///
    /// Returns `None` if:
    /// - No slot is currently available, OR
    /// - Higher priority downloads are already waiting in the queue
    ///
    /// # Difference from `acquire`
    /// - `try_acquire`: Does NOT add to queue, immediate success or failure
    /// - `acquire`: Adds to queue, waits for turn, guarantees eventual permit
    ///
    /// # Use Cases
    /// - Opportunistic slot acquisition (e.g., checking if a paused download can resume)
    /// - Avoiding queue position for downloads that shouldn't block others
    /// - Non-async contexts where blocking is not possible
    ///
    /// # Warning
    /// If you call `try_acquire` and it fails, the download is NOT queued.
    /// You must call `acquire` if you want the download to wait for a slot.
    pub fn try_acquire(
        self: &Arc<Self>,
        id: DownloadId,
        priority: DownloadPriority,
    ) -> Option<PriorityPermit> {
        let mut inner = self.inner.lock();
        if inner.active.len() >= self.max_concurrent.load(Ordering::Relaxed) {
            return None;
        }

        // Check if there are higher priority downloads waiting
        if let Some(next) = inner.waiting.peek() {
            if next.priority > priority {
                return None; // Higher priority download is waiting
            }
        }

        // Try to acquire permit
        match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => {
                inner.active.insert(id, priority);
                Some(PriorityPermit {
                    permit: Some(permit),
                    id,
                    queue: Arc::clone(self),
                })
            }
            Err(_) => None,
        }
    }

    /// Update the priority of a waiting download
    ///
    /// If the download is already active, this has no effect on scheduling.
    /// Returns true if the priority was updated.
    pub fn set_priority(&self, id: DownloadId, new_priority: DownloadPriority) -> bool {
        let mut inner = self.inner.lock();

        // Check if download is waiting
        if inner.waiting_priorities.contains_key(&id) {
            // Remove and re-add with new priority
            let entries: Vec<_> = inner.waiting.drain().collect();
            for entry in entries {
                if entry.id == id {
                    inner.waiting.push(QueueEntry {
                        id: entry.id,
                        priority: new_priority,
                        sequence: entry.sequence,
                    });
                } else {
                    inner.waiting.push(entry);
                }
            }
            inner.waiting_priorities.insert(id, new_priority);
            drop(inner);

            // Notify waiting downloads to re-check their position
            self.notify.notify_waiters();
            return true;
        }

        // Check if download is active (update tracking but doesn't affect scheduling)
        if let Some(priority) = inner.active.get_mut(&id) {
            *priority = new_priority;
            return true;
        }

        false
    }

    /// Remove a download from the waiting queue
    ///
    /// Call this if a download is cancelled before acquiring a permit.
    pub fn remove(&self, id: DownloadId) {
        let mut inner = self.inner.lock();
        inner.waiting_priorities.remove(&id);
        // Rebuild heap without the removed entry
        let entries: Vec<_> = inner.waiting.drain().filter(|e| e.id != id).collect();
        for entry in entries {
            inner.waiting.push(entry);
        }
        drop(inner);
        // The removed entry may have been at the head of the queue; wake the
        // remaining waiters so the new head can re-check its position.
        self.notify.notify_waiters();
    }

    /// Get the priority of a download (waiting or active)
    pub fn get_priority(&self, id: DownloadId) -> Option<DownloadPriority> {
        let inner = self.inner.lock();
        inner
            .waiting_priorities
            .get(&id)
            .or_else(|| inner.active.get(&id))
            .copied()
    }

    /// Update the concurrency ceiling for future acquisitions.
    pub fn set_max_concurrent(&self, max_concurrent: usize) {
        let previous = self.max_concurrent.swap(max_concurrent, Ordering::Relaxed);
        if max_concurrent > previous {
            self.semaphore.add_permits(max_concurrent - previous);
        }
        self.notify.notify_waiters();
    }

    /// Get the number of active downloads
    pub fn active_count(&self) -> usize {
        self.inner.lock().active.len()
    }

    /// Get the number of waiting downloads
    pub fn waiting_count(&self) -> usize {
        self.inner.lock().waiting.len()
    }

    /// Get the position in queue for a waiting download (1-indexed, None if not waiting)
    pub fn queue_position(&self, id: DownloadId) -> Option<usize> {
        let inner = self.inner.lock();
        if !inner.waiting_priorities.contains_key(&id) {
            return None;
        }
        // Count entries with higher priority or same priority but lower sequence
        let mut sorted: Vec<_> = inner.waiting.iter().cloned().collect();
        sorted.sort_by(|a, b| b.cmp(a)); // Reverse to get descending order
        sorted.iter().position(|e| e.id == id).map(|p| p + 1)
    }

    /// Get statistics about the queue
    pub fn stats(&self) -> PriorityQueueStats {
        let inner = self.inner.lock();
        let mut by_priority = HashMap::new();
        for priority in inner.waiting_priorities.values() {
            *by_priority.entry(*priority).or_insert(0) += 1;
        }
        PriorityQueueStats {
            active: inner.active.len(),
            waiting: inner.waiting.len(),
            waiting_by_priority: by_priority,
        }
    }
}

/// Statistics about the priority queue
#[derive(Debug, Clone)]
pub struct PriorityQueueStats {
    /// Number of active downloads
    pub active: usize,
    /// Total number of waiting downloads
    pub waiting: usize,
    /// Waiting downloads by priority level
    pub waiting_by_priority: HashMap<DownloadPriority, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_ordering() {
        assert!(DownloadPriority::Critical > DownloadPriority::High);
        assert!(DownloadPriority::High > DownloadPriority::Normal);
        assert!(DownloadPriority::Normal > DownloadPriority::Low);
    }

    #[test]
    fn test_priority_from_str() {
        assert_eq!(
            "low".parse::<DownloadPriority>().unwrap(),
            DownloadPriority::Low
        );
        assert_eq!(
            "normal".parse::<DownloadPriority>().unwrap(),
            DownloadPriority::Normal
        );
        assert_eq!(
            "high".parse::<DownloadPriority>().unwrap(),
            DownloadPriority::High
        );
        assert_eq!(
            "critical".parse::<DownloadPriority>().unwrap(),
            DownloadPriority::Critical
        );
    }

    #[test]
    fn test_queue_entry_ordering() {
        let entry1 = QueueEntry {
            id: DownloadId::new(),
            priority: DownloadPriority::Normal,
            sequence: 1,
        };
        let entry2 = QueueEntry {
            id: DownloadId::new(),
            priority: DownloadPriority::High,
            sequence: 2,
        };
        let entry3 = QueueEntry {
            id: DownloadId::new(),
            priority: DownloadPriority::Normal,
            sequence: 0,
        };

        // Higher priority should be greater
        assert!(entry2 > entry1);

        // Same priority, lower sequence should be greater
        assert!(entry3 > entry1);
    }

    #[tokio::test]
    async fn test_priority_queue_basic() {
        let queue = PriorityQueue::new(2);
        let id1 = DownloadId::new();
        let id2 = DownloadId::new();

        // Should be able to acquire 2 permits
        let permit1 = queue.clone().acquire(id1, DownloadPriority::Normal).await;
        let permit2 = queue.clone().acquire(id2, DownloadPriority::Normal).await;

        assert_eq!(queue.active_count(), 2);

        // Drop permits
        drop(permit1);
        drop(permit2);

        assert_eq!(queue.active_count(), 0);
    }

    #[tokio::test]
    async fn test_priority_queue_priority_ordering() {
        let queue = PriorityQueue::new(1);
        let id_low = DownloadId::new();
        let id_high = DownloadId::new();

        // Acquire first slot
        let permit1 = queue
            .clone()
            .acquire(DownloadId::new(), DownloadPriority::Normal)
            .await;

        // Add low priority to queue first
        let queue_clone = queue.clone();
        let low_handle =
            tokio::spawn(async move { queue_clone.acquire(id_low, DownloadPriority::Low).await });

        // Give it time to enter the queue
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Add high priority to queue
        let queue_clone = queue.clone();
        let high_handle =
            tokio::spawn(async move { queue_clone.acquire(id_high, DownloadPriority::High).await });

        // Give it time to enter the queue
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert_eq!(queue.waiting_count(), 2);

        // Release first permit - high priority should get the slot
        drop(permit1);

        // Wait for high priority to acquire
        let high_permit = tokio::time::timeout(std::time::Duration::from_millis(100), high_handle)
            .await
            .expect("timeout")
            .expect("join error");

        assert_eq!(queue.active_count(), 1);
        assert_eq!(queue.waiting_count(), 1);

        // Release high priority permit
        drop(high_permit);

        // Wait for low priority to acquire
        let _low_permit = tokio::time::timeout(std::time::Duration::from_millis(100), low_handle)
            .await
            .expect("timeout")
            .expect("join error");

        assert_eq!(queue.active_count(), 1);
        assert_eq!(queue.waiting_count(), 0);
    }

    #[test]
    fn test_set_priority() {
        let queue = PriorityQueue::new(1);
        let id = DownloadId::new();

        // Add to waiting queue (can't acquire because no async context for test)
        {
            let mut inner = queue.inner.lock();
            inner.waiting.push(QueueEntry {
                id,
                priority: DownloadPriority::Low,
                sequence: 0,
            });
            inner.waiting_priorities.insert(id, DownloadPriority::Low);
        }

        assert_eq!(queue.get_priority(id), Some(DownloadPriority::Low));

        // Update priority
        assert!(queue.set_priority(id, DownloadPriority::High));

        assert_eq!(queue.get_priority(id), Some(DownloadPriority::High));
    }

    #[test]
    fn test_remove() {
        let queue = PriorityQueue::new(1);
        let id = DownloadId::new();

        // Add to waiting queue
        {
            let mut inner = queue.inner.lock();
            inner.waiting.push(QueueEntry {
                id,
                priority: DownloadPriority::Normal,
                sequence: 0,
            });
            inner
                .waiting_priorities
                .insert(id, DownloadPriority::Normal);
        }

        assert_eq!(queue.waiting_count(), 1);

        // Remove
        queue.remove(id);

        assert_eq!(queue.waiting_count(), 0);
        assert_eq!(queue.get_priority(id), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_acquire_release_stress_no_lost_wakeup() {
        // Many tasks contending for one slot: a lost wakeup or a wrong-entry
        // pop wedges the queue and this test times out.
        let queue = PriorityQueue::new(1);
        let mut handles = Vec::new();
        for i in 0..50 {
            let queue = queue.clone();
            let priority = match i % 3 {
                0 => DownloadPriority::Low,
                1 => DownloadPriority::Normal,
                _ => DownloadPriority::High,
            };
            handles.push(tokio::spawn(async move {
                let permit = queue.acquire(DownloadId::new(), priority).await;
                tokio::task::yield_now().await;
                drop(permit);
            }));
        }
        for handle in handles {
            tokio::time::timeout(std::time::Duration::from_secs(10), handle)
                .await
                .expect("queue wedged: waiter never acquired")
                .expect("join error");
        }
        assert_eq!(queue.active_count(), 0);
        assert_eq!(queue.waiting_count(), 0);
    }

    #[tokio::test]
    async fn test_remove_head_wakes_waiters() {
        let queue = PriorityQueue::new(1);
        let permit = queue
            .clone()
            .acquire(DownloadId::new(), DownloadPriority::Normal)
            .await;

        // Head of the waiting queue, which we will remove.
        let id_head = DownloadId::new();
        let queue_clone = queue.clone();
        let head_handle =
            tokio::spawn(async move { queue_clone.acquire(id_head, DownloadPriority::High).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Second waiter, behind the head.
        let id_second = DownloadId::new();
        let queue_clone = queue.clone();
        let second_handle =
            tokio::spawn(
                async move { queue_clone.acquire(id_second, DownloadPriority::Low).await },
            );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(queue.waiting_count(), 2);

        // Cancel the head; free the slot; the second waiter must proceed.
        head_handle.abort();
        queue.remove(id_head);
        drop(permit);

        let _second_permit = tokio::time::timeout(std::time::Duration::from_secs(5), second_handle)
            .await
            .expect("second waiter never woke after head removal")
            .expect("join error");
        assert_eq!(queue.active_count(), 1);
        assert_eq!(queue.waiting_count(), 0);
    }

    #[test]
    fn test_stats() {
        let queue = PriorityQueue::new(2);

        // Add some waiting entries
        {
            let mut inner = queue.inner.lock();
            for i in 0..3 {
                let id = DownloadId::new();
                let priority = match i % 3 {
                    0 => DownloadPriority::Low,
                    1 => DownloadPriority::Normal,
                    _ => DownloadPriority::High,
                };
                inner.waiting.push(QueueEntry {
                    id,
                    priority,
                    sequence: i,
                });
                inner.waiting_priorities.insert(id, priority);
            }
        }

        let stats = queue.stats();
        assert_eq!(stats.waiting, 3);
        assert_eq!(stats.active, 0);
    }
}
