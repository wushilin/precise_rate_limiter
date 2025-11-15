# precise_rate_limiter

A high-performance, precise rate limiter for Rust using tokio channels and atomic operations.

The Quota::new gives you an Arc<Quota>, so you can easily share this among many threads. You can clone it, send it, no issues.

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

## Example

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
