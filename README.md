# precise_rate_limiter

A high-performance, precise rate limiter for Rust using tokio channels and atomic operations.

This crate provides four implementations:

**Async implementations (for tokio async contexts):**
- **`FastQuota`**: A high-performance variant with 10x better throughput in low-contention scenarios
- **`Quota`**: A fair, FIFO rate limiter with guaranteed ordering

**Synchronous implementations (for non-async contexts):**
- **`FastQuotaSync`**: Synchronous version with best-effort FIFO ordering and fast-path optimization
- **`QuotaSync`**: Synchronous version with perfect FIFO ordering

Both `Quota::new()` and `FastQuota::new()` (as well as their sync variants) return `Arc<Self>`, so you can easily share them among many threads. You can clone it, send it, no issues.

## Use cases

1. Shared global quota (e.g. built a VPN server but want to limit global bandwidth). You can consume millions of bytes or bits at one go and refresh millions of bits or bytes at one go.
2. API quota across a single server between many threads.
3. Any other high quota refill rate and high acquisitation situation, sometimes acquisition is weighted (depend on actual consumption of tokens, e.g. bytes sent)


Performance:
Since quota is always fair and FIFO, it uses mpsc channel internally. The channel has 500 buffer. When you call quota.acquire, you send a wait request to the queue and wait for wake up.
When queue is full, you are blocked, of course. When queue is not full, you queue request is waiting to be processed and you entered Notify.await state.

The thread determines the head of queue can be waken up will wake up each caller in order.

Based on benchmark, the await operation (assuming not blocked on quota refreshment), can handle roughly 400,000 operations per second. 
Therefore, your quota granularity, at most can go 400,000 per second at 1 acquisition.

If this is not fast enough, you can acquire multiple of X tokens and buffer locally for immediate reuse. For example, if you acquired 5000 tokens at once, then consume one by one locally until counter goes to zero, then you can call Quota.acquire(5000) again. In this case, your ops per second can be 5000*400,000 = 20 billion ops. The global quota of ops per second is still honored.

## FastQuota: High-Performance Variant

`FastQuota` is an optimized variant of `Quota` that provides significantly better performance in low-contention scenarios while maintaining reasonable fairness guarantees.

### Two-Path Design

`FastQuota` uses a dual-path architecture:

- **Fast Path**: Direct token acquisition using atomic operations when there's no contention
  - Achieves approximately **4 million operations per second** (10x faster than `Quota`)
  - No channel overhead, minimal latency
  - Used when no requests are queued and tokens are available

- **Slow Path**: FIFO-queued processing when there is contention
  - Falls back to the same fair queueing mechanism as `Quota`
  - Ensures requests are processed in order when tokens are scarce
  - Uses a write lock to prevent fast path from bypassing queued requests

### When to Use FastQuota

Use `FastQuota` when:
- You have **low contention** (most requests succeed immediately)
- You need **maximum throughput** for high-frequency operations
- You can tolerate **slight fairness trade-offs** in edge cases
- Performance is more critical than perfect FIFO ordering

Use `Quota` when:
- You need **guaranteed perfect fairness** (strict FIFO)
- Contention is expected to be high
- Fairness is more important than peak performance

### Performance Comparison

| Metric | Quota | FastQuota |
|--------|-------|-----------|
| Low contention throughput, mostly no waits | ~400K ops/sec | ~4M ops/sec |
| High contention throughput, everyone waits | ~400K ops/sec | ~400K ops/sec |
| Fairness | Perfect FIFO | Mostly FIFO (with edge cases) |
| Latency (low contention) | Higher (channel overhead) | Lower (direct atomic ops) |

### Fairness Trade-offs

`FastQuota` prioritizes performance over perfect fairness:

- **Queued requests are processed in FIFO order** - Once requests enter the slow path, they are handled fairly
- **Write lock prevents bypassing** - When processing queued requests, the fast path is blocked
- **Edge case**: In rare situations, a fast-path acquire may succeed even when a request is being queued (but not yet processed)

For most practical use cases, `FastQuota` provides excellent performance with reasonable fairness guarantees.

### FastQuota Example

```rust
use precise_rate_limiter::FastQuota;
use std::time::Duration;

#[tokio::main]
async fn main() {
    // Create a fast quota with the same parameters as Quota
    let quota = FastQuota::new(
        1000,                        // max_buffer: 1000 tokens
        100,                         // refill_amount: 100 tokens
        Duration::from_millis(100),  // refill_interval: every 100ms
    );
    
    // Fast path: acquires immediately if tokens available
    quota.acquire(50).await;
    
    // Automatically falls back to slow path if needed
    quota.acquire(200).await;
}
```

### Implementation Details

The fast path works by:
1. Attempting to acquire a read lock (non-blocking)
2. If successful, atomically checking and subtracting tokens
3. Returning immediately if tokens are available

The slow path:
1. Sends request to a channel (same as `Quota`)
2. Background task acquires write lock (blocks fast path)
3. Processes requests in FIFO order
4. Releases write lock when queue is empty

This design ensures that when there's no contention, operations are extremely fast, while still maintaining fairness when contention occurs.

### Notes about atomicity
The token refills and deduction are using atomic usize and should be accurate, atomic.
the refill is using time ticker and missed ticker events (unlikely) are bursted up.

the refill is very reliable. 

## Notes about initial token bucket.
The bucket is initially full. The initial number of token is always the max token.
If you consume quota too slowly, you will always operate at full speed.

If you consume quota too fast, you will burn out the token bucket soon, and you will be
subjected to the quota refill rate.

## Warning
The quota refill will stop once the buffered token reaches the max_token.

If you try to acquire max_token + N where N is positive, your code will panic.


## Features

- **Event-driven**: Uses tokio channels and notifications for efficient waiting
- **Precise rate limiting**: Automatic token refilling at fixed intervals
- **Fair queueing**: Completely fair FIFO queueing mechanism ensures requests are processed in order
- **High performance**: Single-threaded processing with atomic operations
- **Async/await**: Built on tokio for async Rust applications

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
precise_rate_limiter = "0.1.0"
tokio = { version = "1", features = ["full"] }
```

## Synchronous Implementations

For non-async contexts (e.g., standard threads, libraries without async runtime), this crate provides synchronous versions that use blocking operations instead of async/await.

### QuotaSync: Blocking with Perfect FIFO

`QuotaSync` provides the same functionality as `Quota` but uses `std::sync` primitives and blocks the calling thread instead of using async/await.

**Features:**
- **Perfect FIFO ordering**: Guarantees strict first-in-first-out processing
- **Blocking operations**: Uses `std::sync` instead of tokio async
- **Same API**: Drop-in replacement for `Quota` in non-async contexts
- **Background threads**: Uses standard threads for refilling and processing

**Example:**

```rust
use precise_rate_limiter::QuotaSync;
use std::time::Duration;
use std::thread;

fn main() {
    let quota = QuotaSync::new(100, 10, Duration::from_millis(100));
    
    // Blocks until tokens are available
    quota.acquire(50);
    println!("Acquired 50 tokens!");
    
    // Can be shared across threads
    let quota_clone = quota.clone();
    thread::spawn(move || {
        quota_clone.acquire(30);
        println!("Thread acquired 30 tokens!");
    }).join().unwrap();
}
```

### FastQuotaSync: Blocking with Best-Effort FIFO

`FastQuotaSync` provides the same functionality as `FastQuota` but uses blocking operations.

**Features:**
- **Fast-path optimization**: Uses atomic operations for low-contention scenarios
- **Best-effort FIFO**: Maintains fairness once requests are queued
- **High performance**: Similar throughput improvements as `FastQuota`
- **No async runtime needed**: Uses standard threads and blocking

**Example:**

```rust
use precise_rate_limiter::FastQuotaSync;
use std::time::Duration;

fn main() {
    let quota = FastQuotaSync::new(1000, 100, Duration::from_millis(100));
    
    // Fast path: acquires immediately if tokens available
    quota.acquire(50);
    
    // Automatically falls back to slow path if needed
    quota.acquire(200);
}
```

### When to Use Sync vs Async

| Use Case | Recommended Implementation |
|----------|---------------------------|
| **Tokio async application** | `Quota` or `FastQuota` |
| **Standard threads** | `QuotaSync` or `FastQuotaSync` |
| **Library without async** | `QuotaSync` or `FastQuotaSync` |
| **Mixed async/sync code** | Use both (they're independent) |

### Performance Notes

The synchronous implementations have similar performance characteristics to their async counterparts:
- `QuotaSync` processes ~400K requests/sec (similar to `Quota`)
- `FastQuotaSync` can achieve ~4M requests/sec on the fast path (similar to `FastQuota`)
- Both use background threads for token refilling and request processing

## Example

### Using Quota (Fair FIFO)

```rust
use precise_rate_limiter::Quota;
use std::time::Duration;

#[tokio::main]
async fn main() {
    // Create a rate limiter with:
    // - max_buffer: 10 tokens (capacity)
    // - refill_amount: 1 token per refill
    // - refill_interval: 1 second
    let quota = Quota::new(10, 1, Duration::from_secs(1));
    
    // Acquire tokens (blocks until available)
    quota.acquire(5).await;
    println!("Acquired 5 tokens!");
    
    // Acquire more tokens
    quota.acquire(3).await;
    println!("Acquired 3 more tokens!");
}
```

### Using FastQuota (High Performance)

```rust
use precise_rate_limiter::FastQuota;
use std::time::Duration;

#[tokio::main]
async fn main() {
    // FastQuota provides 10x better performance in low-contention scenarios
    let quota = FastQuota::new(10, 1, Duration::from_secs(1));
    
    // Fast path: acquires immediately if tokens available
    quota.acquire(5).await;
    
    // Automatically falls back to slow path if needed
    quota.acquire(3).await;
}
```

## Key Advantages

### Flexible Acquire and Refill Amounts

Unlike most rate limiter implementations, `precise_rate_limiter` supports **flexible acquire and refill amounts** up to the maximum buffer size:

- **Large acquire requests**: You can request up to `max_buffer` tokens in a single call. The rate limiter will wait until enough tokens accumulate to fulfill the request.

```rust
// Request up to max_buffer tokens (10 in this case)
let quota = Quota::new(10, 1, Duration::from_secs(1));
quota.acquire(10).await; // Acquires all available tokens
```

- **Large refill amounts**: You can configure the refill to add many tokens at once (up to `max_buffer`), allowing for burst capacity or faster recovery. Refill amounts exceeding the buffer are automatically clamped.

```rust
// Refill up to 100 tokens per interval (clamped to max_buffer)
let quota = Quota::new(100, 100, Duration::from_secs(1));
```

This flexibility is particularly useful for:
- **Burst handling**: Allowing large requests when tokens have accumulated up to the buffer limit
- **Variable workloads**: Adapting to different request sizes up to the maximum buffer capacity
- **Efficient batching**: Processing large batches when rate limits allow

Most existing implementations restrict acquire amounts to small fixed values, which can be limiting for real-world use cases.

### Fair Queueing

The rate limiter uses a **completely fair FIFO (First-In-First-Out) queueing mechanism**. All acquire requests are processed through a single channel and handled in strict order:

- **FIFO guarantee**: Requests are fulfilled in the exact order they were submitted
- **No starvation**: Every request will eventually be fulfilled, regardless of request size
- **Single processing thread**: One dedicated background task processes all requests sequentially, ensuring fairness

This guarantees that if multiple tasks request tokens simultaneously, they will be served in the order they called `acquire()`, preventing any task from being starved or receiving preferential treatment.


## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
