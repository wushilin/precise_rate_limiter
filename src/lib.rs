//! A high-performance, precise rate limiter using tokio channels and atomic operations.
//!
//! This crate provides a token bucket rate limiter that:
//! - Uses a single background task for token refilling at fixed intervals
//! - Uses a single background task for processing acquire requests in FIFO order
//! - Supports flexible acquire and refill amounts (up to `max_buffer`)
//! - Provides fair, FIFO queueing for acquire requests
//!
//! # Example
//!
//! ```no_run
//! use precise_rate_limiter::Quota;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() {
//!     // Create a quota: max 1000 tokens, refill 100 tokens every 100ms
//!     let quota = Quota::new(1000, 100, Duration::from_millis(100));
//!
//!     // Acquire 50 tokens (will wait if not enough available)
//!     quota.acquire(50).await;
//!
//!     // Acquire more tokens
//!     quota.acquire(200).await;
//! }
//! ```

use std::{
    sync::{
        Arc, atomic::{AtomicUsize, Ordering}
    },
    time::Duration,
};
use tokio::sync::{Notify, RwLock, mpsc, oneshot};
use tokio::time::{interval, timeout, MissedTickBehavior};

struct AcquireRequest {
    required: usize,
    notify: oneshot::Sender<()>,
}

/// A high-performance rate limiter with fast-path optimization for low-contention scenarios.
///
/// `FastQuota` uses a two-path design:
/// - **Fast path**: Direct token acquisition when there's no contention (no queued requests)
/// - **Slow path**: FIFO-queued processing when there is contention
///
/// The quota starts with `max_buffer` tokens available. Tokens are automatically
/// refilled at fixed intervals by a background task. Acquire requests use the fast path
/// when possible, falling back to the slow path (FIFO queue) when there's contention.
///
/// The quota is designed to be shared across many threads/tasks. Clone the `Arc<FastQuota>`
/// to share it between tasks.
///
/// # Performance
///
/// When there's no contention, the fast path can achieve approximately **4 million operations
/// per second**, which is roughly **10x faster** than the `Quota` implementation. When there
/// is contention, requests are processed in FIFO order through the slow path, ensuring fairness.
///
/// # Fairness Trade-offs
///
/// The fast path optimization prioritizes performance over perfect fairness. In rare edge cases,
/// a fast-path acquire may succeed even when a request is being queued. However, once requests
/// are queued, the write lock mechanism ensures they are processed in FIFO order before fast
/// path can proceed again.
///
/// # Panics
///
/// The `acquire` method will panic if `n > max_buffer`.
pub struct FastQuota {
    tokens: Arc<AtomicUsize>,
    max_buffer: usize,
    acquire_sender: mpsc::Sender<AcquireRequest>,
    lock: Arc<RwLock<()>>,
}

impl FastQuota {
    /// Creates a new fast quota with the specified parameters.
    ///
    /// The quota starts with `max_buffer` tokens available. A background task
    /// automatically refills `refill_amount` tokens every `refill_interval`,
    /// up to the `max_buffer` limit.
    ///
    /// # Arguments
    ///
    /// * `max_buffer` - Maximum number of tokens that can be stored. The quota
    ///   starts with this many tokens available. Acquire requests cannot exceed
    ///   this value.
    /// * `refill_amount` - Number of tokens to add per refill interval. Can be
    ///   larger than `max_buffer` (will be capped to available space).
    /// * `refill_interval` - Time interval between refills. The first refill happens
    ///   after the first interval (not immediately).
    ///
    /// # Returns
    ///
    /// Returns an `Arc<FastQuota>` that can be cloned and shared across tasks.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use precise_rate_limiter::FastQuota;
    /// use std::time::Duration;
    ///
    /// let quota = FastQuota::new(
    ///     1000,                        // max_buffer: 1000 tokens
    ///     100,                          // refill_amount: 100 tokens
    ///     Duration::from_millis(100),  // refill_interval: every 100ms
    /// );
    /// ```
    pub fn new(max_buffer: usize, refill_amount: usize, refill_interval: Duration) -> Arc<Self> {
        let (acquire_sender, acquire_receiver) = mpsc::channel(500);
        
        let quota = Arc::new(Self {
            tokens: Arc::new(AtomicUsize::new(max_buffer)),
            max_buffer,
            acquire_sender,
            lock: Arc::new(RwLock::new(())),
        });

        let tokens_available = Arc::new(Notify::new());
        let tokens_available_clone = tokens_available.clone();
        
        let rwlock = quota.lock.clone();
        let tokens_refill = quota.tokens.clone();
        let tokens = quota.tokens.clone();
        tokio::spawn(refill_task(
            tokens_refill,
            max_buffer,
            refill_amount,
            refill_interval,
            tokens_available_clone,
        ));

        tokio::spawn(FastQuota::acquire_task(tokens, acquire_receiver, tokens_available, rwlock));
        

        quota
    }

    /// Acquires `n` tokens from the quota.
    ///
    /// This method attempts a fast-path acquisition first. If the fast path fails
    /// (due to lock contention or insufficient tokens), it falls back to the slow
    /// path, which queues the request for FIFO processing.
    ///
    /// # Arguments
    ///
    /// * `n` - Number of tokens to acquire. Must be `<= max_buffer`.
    ///
    /// # Panics
    ///
    /// Panics if `n > max_buffer`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use precise_rate_limiter::FastQuota;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let quota = FastQuota::new(100, 10, Duration::from_millis(100));
    ///
    ///     // Attempts fast path first, falls back to slow path if needed
    ///     quota.acquire(50).await;
    ///     quota.acquire(30).await;
    /// }
    /// ```
    pub async fn acquire(self: &Arc<Self>, n: usize) {
        if self.fast_path_acquire(n).await {
            return;
        } else {
            self.slow_path_acquire(n).await;
        }
    }
    async fn slow_path_acquire(self: &Arc<Self>, n: usize) {
        if n > self.max_buffer {
            panic!("requested tokens {} exceed max buffer size {}", n, self.max_buffer);
        }
        let (sender, receiver) = oneshot::channel();
        let request = AcquireRequest {
            required: n,
            notify: sender,
        };
        
        self.acquire_sender.send(request).await.expect("acquire channel closed");
        
        receiver.await.unwrap();
    }

    async fn fast_path_acquire(self: &Arc<Self>, n: usize) -> bool {
        if n > self.max_buffer {
            panic!("requested tokens {} exceed max buffer size {}", n, self.max_buffer);
        }
        
        let _read_lck = self.lock.try_read(); // get a read lock first
        if _read_lck.is_err() {
            // can't acuire lock now, let's go to the slow path
            return false; // someone is writing, go to slow path
        }
        let _read_lck = _read_lck.unwrap();
        let success = self.tokens.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| {
                if current >= n {
                        Some(current - n)
                    } else {
                        None
                    }
                }
        ).is_ok();
            
        return success
        
    }

    async fn acquire_task(
        tokens: Arc<AtomicUsize>,
        mut receiver: mpsc::Receiver<AcquireRequest>,
        tokens_available: Arc<Notify>,
        lock: Arc<RwLock<()>>,
    ) {
        let mut _write_lock = None;
        while let Some(request) = receiver.recv().await {
            if _write_lock.is_none() {
                // dequeue was successful, we need to hold the lock until no more waiters
                _write_lock = Some(lock.write().await);
            }
            loop {
                let success = {
                    let result = tokens.fetch_update(
                    Ordering::AcqRel,
                    Ordering::Acquire,
                    |current| {
                        if current >= request.required {
                            Some(current - request.required)
                        } else {
                            None
                        }
                    }).is_ok();
                    result
                };
                
                if success {
                    let _ = request.notify.send(());
                    break;
                } else {
                    let _ = timeout(Duration::from_millis(10), tokens_available.notified()).await;
                }
            }
            let has_more_waiters = !receiver.is_empty();
            if !has_more_waiters {
                // no more waiters, we can release the write lock
                // we will block fast path again until there are more waiters
                _write_lock = None;
            }
        }
    }
}


/// A rate limiter that uses a token bucket algorithm with automatic refilling.
///
/// The quota starts with `max_buffer` tokens available. Tokens are automatically
/// refilled at fixed intervals by a background task. Acquire requests are processed
/// in FIFO order by a separate background task, ensuring fairness.
///
/// The quota is designed to be shared across many threads/tasks. Clone the `Arc<Quota>`
/// to share it between tasks.
///
/// # Performance
///
/// The acquire operation can handle roughly 400,000 operations per second when not
/// blocked on token availability. For higher throughput, acquire multiple tokens at
/// once and buffer them locally.
///
/// # Panics
///
/// The `acquire` method will panic if `n > max_buffer`.
pub struct Quota {
    tokens: Arc<AtomicUsize>,
    max_buffer: usize,
    acquire_sender: mpsc::Sender<AcquireRequest>,
}

impl Quota {
    /// Creates a new quota with the specified parameters.
    ///
    /// The quota starts with `max_buffer` tokens available. A background task
    /// automatically refills `refill_amount` tokens every `refill_interval`,
    /// up to the `max_buffer` limit.
    ///
    /// # Arguments
    ///
    /// * `max_buffer` - Maximum number of tokens that can be stored. The quota
    ///   starts with this many tokens available. Acquire requests cannot exceed
    ///   this value.
    /// * `refill_amount` - Number of tokens to add per refill interval. Can be
    ///   larger than `max_buffer` (will be capped to available space).
    /// * `refill_interval` - Time interval between refills. The first refill happens
    ///   after the first interval (not immediately).
    ///
    /// # Returns
    ///
    /// Returns an `Arc<Quota>` that can be cloned and shared across tasks.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use precise_rate_limiter::Quota;
    /// use std::time::Duration;
    ///
    /// let quota = Quota::new(
    ///     1000,                        // max_buffer: 1000 tokens
    ///     100,                         // refill_amount: 100 tokens
    ///     Duration::from_millis(100),  // refill_interval: every 100ms
    /// );
    /// ```
    pub fn new(max_buffer: usize, refill_amount: usize, refill_interval: Duration) -> Arc<Self> {
        let (acquire_sender, acquire_receiver) = mpsc::channel(500);
        
        // Shared notify for refill task to wake up acquire task when tokens are added
        let tokens_available = Arc::new(Notify::new());

        let quota = Arc::new(Self {
            tokens: Arc::new(AtomicUsize::new(max_buffer)),
            max_buffer,
            acquire_sender,
        });

        // Spawn background task 1: Refill tokens automatically at fixed intervals
        let tokens_refill = quota.tokens.clone();
        let max_buffer = quota.max_buffer;
        let tokens_available_refill = tokens_available.clone();
        tokio::spawn(async move {
            refill_task(tokens_refill, max_buffer, refill_amount, refill_interval, tokens_available_refill).await;
        });

        // Spawn background task 2: Process acquire requests
        let tokens_acquire = quota.tokens.clone();
        let tokens_available_acquire = tokens_available.clone();
        tokio::spawn(async move {
            acquire_task(tokens_acquire, acquire_receiver, tokens_available_acquire).await;
        });

        quota
    }

    /// Acquires `n` tokens from the quota.
    ///
    /// This method will wait until enough tokens are available. The request is
    /// queued in FIFO order, ensuring fair processing of concurrent acquire requests.
    ///
    /// # Arguments
    ///
    /// * `n` - Number of tokens to acquire. Must be `<= max_buffer`.
    ///
    /// # Panics
    ///
    /// Panics if `n > max_buffer`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use precise_rate_limiter::Quota;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let quota = Quota::new(100, 10, Duration::from_millis(100));
    ///
    ///     // Acquire 50 tokens (will wait if not enough available)
    ///     quota.acquire(50).await;
    ///
    ///     // Acquire more tokens
    ///     quota.acquire(30).await;
    /// }
    /// ```
    pub async fn acquire(self: &Arc<Self>, n: usize) {
        if n > self.max_buffer {
            panic!("requested tokens {} exceed max buffer size {}", n, self.max_buffer);
        }
        let (sender, receiver) = oneshot::channel();
        let request = AcquireRequest {
            required: n,
            notify: sender,
        };
        
        // All acquire requests go through the mpsc channel
        self.acquire_sender.send(request).await.expect("acquire channel closed");
        
        // Wait for completion of acquire request
        // but we also check the flag in case we missed the notification

        receiver.await.unwrap();
        //while !completed.load(Ordering::Acquire) {
        //    tokio::time::timeout(Duration::from_millis(100), notify.notified()).await.unwrap();
        //}
    }
}

/// Background task that automatically refills tokens at fixed intervals.
///
/// Uses `fetch_update` to atomically check and add tokens, ensuring we never
/// exceed the capacity limit. Notifies the acquire task when tokens are added.
async fn refill_task(
    tokens: Arc<AtomicUsize>,
    capacity: usize,
    refill_amount: usize,
    refill_interval: Duration,
    tokens_available: Arc<Notify>,
) {
    let mut interval_timer = interval(refill_interval);
    interval_timer.set_missed_tick_behavior(MissedTickBehavior::Burst);
    interval_timer.tick().await; // tick immediately to only start refill after the first interval
    loop {
        interval_timer.tick().await;
        
        // Try to atomically add tokens if we're not at capacity
        let added = tokens.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| {
                if current >= capacity {
                    None // Already at capacity, don't update
                } else {
                    let to_add = (capacity - current).min(refill_amount);
                    if to_add > 0 {
                        Some(current + to_add)
                    } else {
                        None
                    }
                }
            }
        ).is_ok();
        
        if added {
            // Successfully added tokens, notify acquire task
            tokens_available.notify_one();
        }
    }
}

/// Background task that processes acquire requests in FIFO order.
///
/// Receives acquire requests from the mpsc channel and waits until enough tokens
/// are available. Uses `fetch_update` to atomically check and subtract tokens.
/// Includes a 10ms timeout as a fail-safe for missed notifications.
async fn acquire_task(
    tokens: Arc<AtomicUsize>,
    mut receiver: mpsc::Receiver<AcquireRequest>,
    tokens_available: Arc<Notify>,
) {
    while let Some(request) = receiver.recv().await {
        // Wait until we can fulfill this request
        loop {
            // Try to atomically subtract tokens if we have enough
            let success = tokens.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |current| {
                    if current >= request.required {
                        Some(current - request.required)
                    } else {
                        None // Not enough tokens, don't update
                    }
                }
            ).is_ok();
            
            if success {
                // Successfully acquired tokens atomically
                let _ = request.notify.send(());
                break;
            } else {
                // Not enough tokens, wait for refill task to notify us
                // Use timeout as fail-safe in case notifications are missed
                let _ = timeout(Duration::from_millis(10), tokens_available.notified()).await;
                // Continue loop to try again (either notified or timed out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use tokio::time::Instant;

    #[tokio::test]
    async fn test_basic_acquire() {
        let quota = Quota::new(10,  1, Duration::from_millis(1));
        
        // Should acquire immediately since we start with capacity
        let start = Instant::now();
        quota.acquire(10).await;
        let elapsed = start.elapsed();
        
        // Should be very fast (no waiting)
        assert!(elapsed < Duration::from_millis(50));
    }


        #[tokio::test]
    async fn test_basic_acquire_unfair() {
        let quota = FastQuota::new(10,  1, Duration::from_millis(1));
        
        // Should acquire immediately since we start with capacity
        let start = Instant::now();
        quota.acquire(10).await;
        let elapsed = start.elapsed();
        
        // Should be very fast (no waiting)
        assert!(elapsed < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn test_capacity_limit_unfair() {
        let quota = FastQuota::new(10, 1, Duration::from_millis(1000));
        
        // Acquire all tokens
        let start = Instant::now();
        quota.acquire(10).await;
        
        let elapsed = start.elapsed();
        assert!(elapsed <= Duration::from_millis(20)); // acquire should be very fast
        // Next acquire should wait
        let start = Instant::now();
        let handle = tokio::spawn({
            let quota = quota.clone();
            async move {
                quota.acquire(1).await;
            }
        });
        
        // Wait a bit to ensure it's waiting
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished());
        
        // Wait for refill (should happen after ~100ms)
        handle.await.unwrap();
        let elapsed = start.elapsed();
        
        // Should have waited for refill
        assert!(elapsed >= Duration::from_millis(900));
    }
    #[tokio::test]
    async fn test_capacity_limit() {
        let quota = Quota::new(10, 1, Duration::from_millis(1000));
        
        // Acquire all tokens
        let start = Instant::now();
        quota.acquire(10).await;
        
        let elapsed = start.elapsed();
        assert!(elapsed <= Duration::from_millis(20)); // acquire should be very fast
        // Next acquire should wait
        let start = Instant::now();
        let handle = tokio::spawn({
            let quota = quota.clone();
            async move {
                quota.acquire(1).await;
            }
        });
        
        // Wait a bit to ensure it's waiting
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished());
        
        // Wait for refill (should happen after ~100ms)
        handle.await.unwrap();
        let elapsed = start.elapsed();
        
        // Should have waited for refill
        assert!(elapsed >= Duration::from_millis(900));
    }


        #[tokio::test]
    async fn test_one_per_ms_100_acquirers_unfair() {
        let quota = FastQuota::new(
            1,                    // capacity: 1 token
            1,                    // refill amount: 1 token
            Duration::from_millis(1), // refill interval: 1 second
        );
        
        let start = Instant::now();
        let acquired_count = Arc::new(AtomicU64::new(0));
        let acquired_times = Arc::new(std::sync::Mutex::new(Vec::new()));
        
        println!("Starting test: 1 token per millisecond, 100 acquirers");
        
        // Spawn 100 acquirer tasks
        let mut handles = vec![];
        for i in 0..100 {
            let quota = quota.clone();
            let count = acquired_count.clone();
            let times = acquired_times.clone();
            handles.push(tokio::spawn(async move {
                let acquire_start = Instant::now();
                quota.acquire(1).await;
                let elapsed = acquire_start.elapsed();
                let total_elapsed = start.elapsed();
                
                count.fetch_add(1, AtomicOrdering::Relaxed);
                times.lock().unwrap().push((i, total_elapsed, elapsed));
                
                println!(
                    "Acquirer {} acquired token at {:.2}s (waited {:.2}ms)",
                    i,
                    total_elapsed.as_secs_f64(),
                    elapsed.as_millis()
                );
            }));
        }
        
        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }
        
        let total_time = start.elapsed();
        let times = acquired_times.lock().unwrap();
        
        println!("\nTest Results:");
        println!("Total time: {:.2}ms", total_time.as_millis());
        println!("Total acquired: {}", acquired_count.load(AtomicOrdering::Relaxed));
        println!("Expected time: ~100s (100 tokens at 1 per second)");
        
        // Verify all 100 acquired
        assert_eq!(acquired_count.load(AtomicOrdering::Relaxed), 100);
        
        // Should take approximately 100 seconds (allowing some variance)
        assert!(total_time >= Duration::from_secs(0));
        assert!(total_time <= Duration::from_millis(120));
        
        // Verify tokens were acquired roughly 1 per second
        let mut prev_time = Duration::ZERO;
        for (i, time, _) in times.iter().take(10) {
            if i > &0 {
                let interval = *time - prev_time;
                println!("Interval between acquirer {} and {}: {:.2}ms", i - 1, i, interval.as_millis());
                // Should be roughly 1 second apart (allowing 0.5s variance)
                assert!(interval >= Duration::from_millis(0));
                assert!(interval <= Duration::from_millis(20));
            }
            prev_time = *time;
        }
        
        println!("Test passed!");
    }
    #[tokio::test]
    async fn test_multiple_concurrent_acquires() {
        let quota = Quota::new(10, 1, Duration::from_millis(50));
        let acquired_count = Arc::new(AtomicU64::new(0));
        
        // Spawn 20 tasks trying to acquire 1 token each
        let mut handles = vec![];
        for i in 0..20 {
            let quota = quota.clone();
            let count = acquired_count.clone();
            handles.push(tokio::spawn(async move {
                quota.acquire(1).await;
                count.fetch_add(1, AtomicOrdering::Relaxed);
                i
            }));
        }
        
        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }
        
        // All 20 should eventually acquire
        assert_eq!(acquired_count.load(AtomicOrdering::Relaxed), 20);
    }

    #[tokio::test]
    async fn test_multiple_concurrent_acquires_unfair() {
        let quota = FastQuota::new(10, 1, Duration::from_millis(50));
        let acquired_count = Arc::new(AtomicU64::new(0));
        
        // Spawn 20 tasks trying to acquire 1 token each
        let mut handles = vec![];
        for i in 0..20 {
            let quota = quota.clone();
            let count = acquired_count.clone();
            handles.push(tokio::spawn(async move {
                quota.acquire(1).await;
                count.fetch_add(1, AtomicOrdering::Relaxed);
                i
            }));
        }
        
        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }
        
        // All 20 should eventually acquire
        assert_eq!(acquired_count.load(AtomicOrdering::Relaxed), 20);
    }
    

        #[tokio::test]
    async fn test_one_per_ms_100_acquirers() {
        let quota = Quota::new(
            1,                    // capacity: 1 token
            1,                    // refill amount: 1 token
            Duration::from_millis(1), // refill interval: 1 second
        );
        
        let start = Instant::now();
        let acquired_count = Arc::new(AtomicU64::new(0));
        let acquired_times = Arc::new(std::sync::Mutex::new(Vec::new()));
        
        println!("Starting test: 1 token per millisecond, 100 acquirers");
        
        // Spawn 100 acquirer tasks
        let mut handles = vec![];
        for i in 0..100 {
            let quota = quota.clone();
            let count = acquired_count.clone();
            let times = acquired_times.clone();
            handles.push(tokio::spawn(async move {
                let acquire_start = Instant::now();
                quota.acquire(1).await;
                let elapsed = acquire_start.elapsed();
                let total_elapsed = start.elapsed();
                
                count.fetch_add(1, AtomicOrdering::Relaxed);
                times.lock().unwrap().push((i, total_elapsed, elapsed));
                
                println!(
                    "Acquirer {} acquired token at {:.2}s (waited {:.2}ms)",
                    i,
                    total_elapsed.as_secs_f64(),
                    elapsed.as_millis()
                );
            }));
        }
        
        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }
        
        let total_time = start.elapsed();
        let times = acquired_times.lock().unwrap();
        
        println!("\nTest Results:");
        println!("Total time: {:.2}ms", total_time.as_millis());
        println!("Total acquired: {}", acquired_count.load(AtomicOrdering::Relaxed));
        println!("Expected time: ~100s (100 tokens at 1 per second)");
        
        // Verify all 100 acquired
        assert_eq!(acquired_count.load(AtomicOrdering::Relaxed), 100);
        
        // Should take approximately 100 seconds (allowing some variance)
        assert!(total_time >= Duration::from_secs(0));
        assert!(total_time <= Duration::from_millis(120));
        
        // Verify tokens were acquired roughly 1 per second
        let mut prev_time = Duration::ZERO;
        for (i, time, _) in times.iter().take(10) {
            if i > &0 {
                let interval = *time - prev_time;
                println!("Interval between acquirer {} and {}: {:.2}ms", i - 1, i, interval.as_millis());
                // Should be roughly 1 second apart (allowing 0.5s variance)
                assert!(interval >= Duration::from_millis(0));
                assert!(interval <= Duration::from_millis(20));
            }
            prev_time = *time;
        }
        
        println!("Test passed!");
    }
    #[tokio::test]
    async fn test_rate_limiting_unfair() {
        let quota = FastQuota::new(5, 1, Duration::from_millis(100));
        let start = Instant::now();
        let acquired_times = Arc::new(std::sync::Mutex::new(Vec::new()));
        
        // Acquire all initial tokens
        quota.acquire(5).await;
        
        // Try to acquire 10 more (should be rate limited)
        let mut handles = vec![];
        for i in 0..10 {
            let quota = quota.clone();
            let times = acquired_times.clone();
            handles.push(tokio::spawn(async move {
                quota.acquire(1).await;
                let elapsed = start.elapsed();
                times.lock().unwrap().push((i, elapsed));
            }));
        }
        
        for handle in handles {
            handle.await.unwrap();
        }
        
        let times = acquired_times.lock().unwrap();
        // Should have taken at least 1 second (10 tokens / 1 per 100ms)
        let total_time = times.iter().map(|(_, t)| *t).max().unwrap();
        assert!(total_time >= Duration::from_millis(900));
    }
    #[tokio::test]
    async fn test_rate_limiting() {
        let quota = Quota::new(5, 1, Duration::from_millis(100));
        let start = Instant::now();
        let acquired_times = Arc::new(std::sync::Mutex::new(Vec::new()));
        
        // Acquire all initial tokens
        quota.acquire(5).await;
        
        // Try to acquire 10 more (should be rate limited)
        let mut handles = vec![];
        for i in 0..10 {
            let quota = quota.clone();
            let times = acquired_times.clone();
            handles.push(tokio::spawn(async move {
                quota.acquire(1).await;
                let elapsed = start.elapsed();
                times.lock().unwrap().push((i, elapsed));
            }));
        }
        
        for handle in handles {
            handle.await.unwrap();
        }
        
        let times = acquired_times.lock().unwrap();
        // Should have taken at least 1 second (10 tokens / 1 per 100ms)
        let total_time = times.iter().map(|(_, t)| *t).max().unwrap();
        assert!(total_time >= Duration::from_millis(900));
    }


 

        #[tokio::test]
    async fn test_large_acquire_request_unfair() {
        let quota = FastQuota::new(2000, 1, Duration::from_millis(1));
        quota.acquire(2000).await;
        // Try to acquire more than capacity
        let start = Instant::now();
        let result = tokio::time::timeout(Duration::from_millis(1000), quota.acquire(1500)).await;
        assert!(result.is_err());
        let elapsed = start.elapsed();
        
        // Should wait until enough tokens accumulate
        // 15 tokens - 10 initial = 5 more needed
        // At 1 per 100ms, that's ~500ms
        assert!(elapsed >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn test_large_acquire_request() {
        let quota = Quota::new(2000, 1, Duration::from_millis(1));
        quota.acquire(2000).await;
        // Try to acquire more than capacity
        let start = Instant::now();
        let result = tokio::time::timeout(Duration::from_millis(1000), quota.acquire(1500)).await;
        assert!(result.is_err());
        let elapsed = start.elapsed();
        
        // Should wait until enough tokens accumulate
        // 15 tokens - 10 initial = 5 more needed
        // At 1 per 100ms, that's ~500ms
        assert!(elapsed >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn test_throughput_100m_per_100ms() {
        // Refill: 100 million tokens per 100ms
        // Max buffer: 10 billion tokens
        let quota = Quota::new(
            10_000_000_000,              // max_buffer: 10 billion
            100_000_000,                  // refill_amount: 100 million
            Duration::from_millis(100),   // refill_interval: 100ms
        );
        
        let start = Instant::now();
        let acquired_count = Arc::new(AtomicU64::new(0));
        
        let threads = 5;
        println!("Starting throughput test: {} threads acquiring tokens for 10 seconds...", threads);
        
        // Spawn 500 tasks, each acquiring 1 token in a loop
        let mut handles = vec![];
        for _ in 0..threads {
            let quota = quota.clone();
            let count = acquired_count.clone();
            handles.push(tokio::spawn(async move {
                let mut local_count = 0u64;
                while start.elapsed() < Duration::from_secs(10) {
                    quota.acquire(1).await;
                    local_count += 1;
                }
                count.fetch_add(local_count, AtomicOrdering::Relaxed);
                local_count
            }));
        }
        
        // Wait for all tasks to complete
        for handle in handles {
            handle.await.unwrap();
        }
        
        let total_acquired = acquired_count.load(AtomicOrdering::Relaxed);
        let elapsed = start.elapsed();
        
        println!("\nThroughput Test Results:");
        println!("Total tokens acquired by all {} threads: {}", threads, total_acquired);
        println!("Time elapsed: {:.2}s", elapsed.as_secs_f64());
        println!("Tokens per second: {:.2}", total_acquired as f64 / elapsed.as_secs_f64());
        println!("Expected refill rate: 1,000,000,000 tokens/second (100M per 100ms)");
        println!("\nTest completed successfully!");
    }


     #[tokio::test]
    async fn test_throughput_100m_per_100ms_unfair() {
        // Refill: 100 million tokens per 100ms
        // Max buffer: 10 billion tokens
        let quota = FastQuota::new(
            10_000_000_000,              // max_buffer: 10 billion
            100_000_000,                  // refill_amount: 100 million
            Duration::from_millis(100),   // refill_interval: 100ms
        );
        
        let start = Instant::now();
        let acquired_count = Arc::new(AtomicU64::new(0));
        
        let threads = 5;
        println!("Starting throughput test: {} threads acquiring tokens for 10 seconds...", threads);
        
        // Spawn 500 tasks, each acquiring 1 token in a loop
        let mut handles = vec![];
        for _ in 0..threads {
            let quota = quota.clone();
            let count = acquired_count.clone();
            handles.push(tokio::spawn(async move {
                let mut local_count = 0u64;
                while start.elapsed() < Duration::from_secs(10) {
                    quota.acquire(1).await;
                    local_count += 1;
                }
                count.fetch_add(local_count, AtomicOrdering::Relaxed);
                local_count
            }));
        }
        
        // Wait for all tasks to complete
        for handle in handles {
            handle.await.unwrap();
        }
        
        let total_acquired = acquired_count.load(AtomicOrdering::Relaxed);
        let elapsed = start.elapsed();
        
        println!("\nThroughput Test Results:");
        println!("Total tokens acquired by all {} threads: {}", threads, total_acquired);
        println!("Time elapsed: {:.2}s", elapsed.as_secs_f64());
        println!("Tokens per second: {:.2}", total_acquired as f64 / elapsed.as_secs_f64());
        println!("Expected refill rate: 1,000,000,000 tokens/second (100M per 100ms)");
        println!("\nTest completed successfully!");
    }

    #[tokio::test]
    async fn test_mpsc_throughput() {
        use tokio::sync::mpsc as tokio_mpsc;
        
        let num_senders = 500;
        let (sender, mut receiver) = tokio_mpsc::channel(500);
        
        let start = Instant::now();
        let sent_count = Arc::new(AtomicU64::new(0));
        let received_count = Arc::new(AtomicU64::new(0));
        
        println!("Starting mpsc throughput test: {} sender threads, 1 receiver thread, running for 10 seconds...", num_senders);
        
        // Spawn sender tasks
        let mut sender_handles = vec![];
        for _ in 0..num_senders {
            let sender = sender.clone();
            let count = sent_count.clone();
            sender_handles.push(tokio::spawn(async move {
                let mut local_sent = 0u64;
                while start.elapsed() < Duration::from_secs(10) {
                    if sender.send(local_sent).await.is_ok() {
                        local_sent += 1;
                    } else {
                        break; // Channel closed
                    }
                }
                count.fetch_add(local_sent, AtomicOrdering::Relaxed);
                local_sent
            }));
        }
        
        // Spawn receiver task
        let receiver_count = received_count.clone();
        let receiver_handle = tokio::spawn(async move {
            let mut local_received = 0u64;
            while start.elapsed() < Duration::from_secs(10) {
                match receiver.recv().await {
                    Some(_) => {
                        local_received += 1;
                    }
                    None => {
                        break; // Channel closed
                    }
                }
            }
            receiver_count.fetch_add(local_received, AtomicOrdering::Relaxed);
            local_received
        });
        
        // Wait for all senders to complete
        for handle in sender_handles {
            handle.await.unwrap();
        }
        
        // Close the sender to signal receiver to stop
        drop(sender);
        
        // Wait for receiver to complete
        receiver_handle.await.unwrap();
        
        let total_sent = sent_count.load(AtomicOrdering::Relaxed);
        let total_received = received_count.load(AtomicOrdering::Relaxed);
        let elapsed = start.elapsed();
        
        println!("\nMPSC Throughput Test Results:");
        println!("Number of sender threads: {}", num_senders);
        println!("Total messages sent: {}", total_sent);
        println!("Total messages received: {}", total_received);
        println!("Time elapsed: {:.2}s", elapsed.as_secs_f64());
        println!("Sends per second: {:.2}", total_sent as f64 / elapsed.as_secs_f64());
        println!("Receives per second: {:.2}", total_received as f64 / elapsed.as_secs_f64());
        println!("\nTest completed successfully!");
    }
}