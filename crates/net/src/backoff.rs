use std::time::Duration;

/// Delay before retrying a transient network or 5xx failure.
pub fn retry_delay(_attempt: u32) -> Duration {
    todo!("calculate exponential backoff with jitter")
}
