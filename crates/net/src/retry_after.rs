//! `Retry-After` header parsing (RFC 9110 §10.2.3): either delay-seconds or
//! an HTTP-date.

use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, RETRY_AFTER};

/// Parse a `Retry-After` value relative to `now`. Dates in the past yield
/// `Duration::ZERO`; unparseable values yield `None` (callers then fall back
/// to the endpoint's full window, which is the safe choice).
pub fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let date = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    Some((date - now).to_std().unwrap_or(Duration::ZERO))
}

/// Extract and parse `Retry-After` from response headers.
pub fn retry_after_from_headers(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value, now)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use reqwest::header::HeaderValue;

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2015, 10, 21, 7, 20, 0).unwrap()
    }

    #[test]
    fn parses_seconds_and_http_dates() {
        assert_eq!(
            parse_retry_after("120", now()),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after(" 0 ", now()), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", now()),
            Some(Duration::from_secs(480))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now()),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("-5", now()), None);
        assert_eq!(parse_retry_after("soon", now()), None);
    }

    #[test]
    fn reads_header_map() {
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after_from_headers(&headers, now()), None);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("600"));
        assert_eq!(
            retry_after_from_headers(&headers, now()),
            Some(Duration::from_secs(600))
        );
    }
}
