use precise_rate_limiter::FastQuotaSync;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    println!("=== FastQuotaSync Example ===\n");

    // Create a fast quota: 100 tokens max, refill 10 tokens every 100ms
    let quota = FastQuotaSync::new(100, 10, Duration::from_millis(100));

    println!("1. Fast path - immediate acquisition with low contention:");
    let start = Instant::now();
    for i in 0..50 {
        quota.acquire(1);
        if i < 5 {
            println!("   Acquired token {} in {:?}", i + 1, start.elapsed());
        }
    }
    println!("   ... acquired 50 tokens total in {:?}", start.elapsed());

    println!("\n2. Remaining tokens, then rate limiting:");
    quota.acquire(50); // Use remaining tokens
    println!("   Used all 100 tokens");

    let start = Instant::now();
    quota.acquire(5); // Will wait for refill
    println!("   Acquired 5 tokens after waiting {:?}", start.elapsed());

    println!("\n3. High-throughput scenario with multiple threads:");
    let counter = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();
    let mut handles = vec![];

    // Spawn threads that will compete for tokens
    for i in 0..10 {
        let quota = quota.clone();
        let counter = counter.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..5 {
                quota.acquire(1);
                counter.fetch_add(1, Ordering::SeqCst);
            }
            println!("   Thread {} completed", i);
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let total = counter.load(Ordering::SeqCst);
    let elapsed = start.elapsed();
    println!(
        "   Acquired {} tokens across threads in {:?}",
        total, elapsed
    );
    println!(
        "   Throughput: {:.2} tokens/sec",
        total as f64 / elapsed.as_secs_f64()
    );

    println!("\n=== Example Complete ===");
    println!("Note: FastQuotaSync uses fast-path optimization for better performance");
    println!("      in low-contention scenarios while maintaining best-effort FIFO.");
}
