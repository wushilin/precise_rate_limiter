use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};
use tokio::time::{interval, timeout, MissedTickBehavior};

struct AcquireRequest {
    required: usize,
    notify: Arc<Notify>,
}

pub struct Quota {
    tokens: Arc<AtomicUsize>,
    max_buffer: usize,
    acquire_sender: mpsc::Sender<AcquireRequest>,
}

impl Quota {
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

    pub async fn acquire(self: &Arc<Self>, n: usize) {
        let notify = Arc::new(Notify::new());
        let request = AcquireRequest {
            required: n,
            notify: notify.clone(),
        };
        
        // All acquire requests go through the mpsc channel
        self.acquire_sender.send(request).await.expect("acquire channel closed");
        
        // Wait for notification that tokens are available
        notify.notified().await;
    }

}

// Background task 1: Keep adding tokens using atomic usize up to max limit, then stop adding
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
        
        let current = tokens.load(Ordering::Acquire);
        if current >= capacity {
            // Already at max limit, skip this refill
            continue;
        }
        
        // Calculate how much we can safely add (won't exceed capacity)
        let to_add = (capacity - current).min(refill_amount);
        if to_add > 0 {
            // Since there's only one refiller, fetch_add is safe
            tokens.fetch_add(to_add, Ordering::AcqRel);
            // Notify acquire task that tokens might be available now
            tokens_available.notify_one();
        }
    }
}

// Background task 2: Receive from mpsc channel and wait until it can be fulfilled and notify the waiter
async fn acquire_task(
    tokens: Arc<AtomicUsize>,
    mut receiver: mpsc::Receiver<AcquireRequest>,
    tokens_available: Arc<Notify>,
) {
    while let Some(request) = receiver.recv().await {
        // Wait until we can fulfill this request
        loop {
            let current = tokens.load(Ordering::Acquire);
            if current >= request.required {
                // Since there's only one acquire task, fetch_sub is safe
                let old = tokens.fetch_sub(request.required, Ordering::AcqRel);
                // Verify we had enough (should always be true since we checked, but be safe)
                if old >= request.required {
                    // Successfully acquired, notify the waiter
                    request.notify.notify_one();
                    break;
                }
                println!("The impossible happened: we didn't have enough tokens");
                // If we didn't have enough (shouldn't happen, but handle it), add it back and retry
                tokens.fetch_add(request.required, Ordering::AcqRel);
            } else {
                // Not enough tokens, wait for refill task to notify us
                // Use timeout as fail-safe in case notifications are missed
                let _ = timeout(Duration::from_millis(10), tokens_available.notified()).await;
                // Continue loop to check tokens again (either notified or timed out)
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
    async fn test_large_acquire_request() {
        let quota = Quota::new(10, 1, Duration::from_millis(1));
        
        // Try to acquire more than capacity
        let start = Instant::now();
        let result = tokio::time::timeout(Duration::from_millis(1000), quota.acquire(15)).await;
        assert!(result.is_err());
        let elapsed = start.elapsed();
        
        // Should wait until enough tokens accumulate
        // 15 tokens - 10 initial = 5 more needed
        // At 1 per 100ms, that's ~500ms
        assert!(elapsed >= Duration::from_millis(1000));
    }
}