use precise_rate_limiter::QuotaSync;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    println!("=== QuotaSync Example ===\n");

    // Create a quota: 10 tokens max, refill 2 tokens every 100ms
    let quota = QuotaSync::new(10, 2, Duration::from_millis(100));

    println!("1. Basic usage - acquiring tokens immediately:");
    let start = Instant::now();
    quota.acquire(5);
    println!("   Acquired 5 tokens in {:?}", start.elapsed());

    println!("\n2. Rate limiting - waiting for refill:");
    quota.acquire(5); // Use remaining 5 tokens
    println!("   Used all 10 tokens, now waiting for more...");

    let start = Instant::now();
    quota.acquire(2); // Will wait for refill
    println!("   Acquired 2 tokens after waiting {:?}", start.elapsed());

    println!("\n3. Multi-threaded usage with FIFO ordering:");
    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    for i in 0..5 {
        let quota = quota.clone();
        let counter = counter.clone();
        handles.push(thread::spawn(move || {
            let start = Instant::now();
            quota.acquire(1);
            let order = counter.fetch_add(1, Ordering::SeqCst);
            println!(
                "   Thread {} acquired token (order: {}, elapsed: {:?})",
                i,
                order,
                start.elapsed()
            );
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    println!("\n=== Example Complete ===");
}
