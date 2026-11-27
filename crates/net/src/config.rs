//! Loading `[rate_limits]` and `[retry]` from the workspace TOML config.
//!
//! ```toml
//! [rate_limits.skinport.items]
//! path = "/v1/items"          # optional; registered as an alias of the name
//! max_requests = 8
//! window_seconds = 300
//! safety_margin_seconds = 1   # optional, default 0
//! quota_group = "skinport"    # optional; entries with the same group share a bucket
//!
//! [retry]
//! base_seconds = 1
//! cap_seconds = 300
//! max_attempts = 5            # optional, default 5
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use cs2_arb_core::{Error, Platform, Result};
use serde::Deserialize;

use crate::backoff::{RetryPolicy, DEFAULT_MAX_ATTEMPTS};
use crate::clock::Clock;
use crate::limiter::{RateLimit, RateLimiter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointLimitConfig {
    pub platform: Platform,
    pub name: String,
    pub path: Option<String>,
    pub limit: RateLimit,
    pub quota_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetConfig {
    pub endpoints: Vec<EndpointLimitConfig>,
    pub retry: RetryPolicy,
}

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    rate_limits: BTreeMap<String, BTreeMap<String, RawLimit>>,
    retry: Option<RawRetry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimit {
    path: Option<String>,
    max_requests: u32,
    window_seconds: u64,
    #[serde(default)]
    safety_margin_seconds: u64,
    quota_group: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRetry {
    base_seconds: u64,
    cap_seconds: u64,
    max_attempts: Option<u32>,
}

fn parse_platform(name: &str) -> Result<Platform> {
    serde_json::from_value(serde_json::Value::String(name.to_owned()))
        .map_err(|_| Error::InvalidConfig(format!("unknown platform {name:?} in [rate_limits]")))
}

impl NetConfig {
    /// Parse the network sections of the config; other sections are ignored.
    pub fn from_toml(source: &str) -> Result<Self> {
        let raw: RawConfig =
            toml::from_str(source).map_err(|err| Error::InvalidConfig(err.to_string()))?;
        let mut endpoints = Vec::new();
        for (platform_name, entries) in raw.rate_limits {
            let platform = parse_platform(&platform_name)?;
            for (name, entry) in entries {
                let limit = RateLimit::new(
                    entry.max_requests,
                    Duration::from_secs(entry.window_seconds),
                )
                .map_err(|err| {
                    Error::InvalidConfig(format!("rate_limits.{platform_name}.{name}: {err}"))
                })?
                .with_safety_margin(Duration::from_secs(entry.safety_margin_seconds));
                endpoints.push(EndpointLimitConfig {
                    platform,
                    name,
                    path: entry.path,
                    limit,
                    quota_group: entry.quota_group,
                });
            }
        }
        let retry = match raw.retry {
            None => RetryPolicy::default(),
            Some(r) => RetryPolicy::new(
                Duration::from_secs(r.base_seconds),
                Duration::from_secs(r.cap_seconds),
                r.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS),
            )?,
        };
        Ok(Self { endpoints, retry })
    }

    /// Build a registry where every entry is reachable by its name and (if
    /// set) its path. Entries sharing a `quota_group` share one bucket and must
    /// declare identical limits.
    pub fn build_limiter(&self, clock: Arc<dyn Clock>) -> Result<RateLimiter> {
        let mut groups: BTreeMap<(String, String), (Platform, RateLimit, Vec<String>)> =
            BTreeMap::new();
        for entry in &self.endpoints {
            let group = entry
                .quota_group
                .clone()
                .unwrap_or_else(|| format!("endpoint:{}", entry.name));
            let slot = groups
                .entry((format!("{:?}", entry.platform), group.clone()))
                .or_insert_with(|| (entry.platform, entry.limit, Vec::new()));
            if slot.1 != entry.limit {
                return Err(Error::InvalidConfig(format!(
                    "quota group {group:?} for {:?} has conflicting limits",
                    entry.platform
                )));
            }
            slot.2.push(entry.name.clone());
            if let Some(path) = &entry.path {
                if path != &entry.name {
                    slot.2.push(path.clone());
                }
            }
        }
        let mut limiter = RateLimiter::new(clock);
        for (platform, limit, keys) in groups.into_values() {
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            limiter.register_shared(platform, &keys, limit)?;
        }
        Ok(limiter)
    }

    /// Paths configured per platform (useful for routing / diagnostics).
    pub fn paths(&self) -> HashMap<Platform, Vec<String>> {
        let mut out: HashMap<Platform, Vec<String>> = HashMap::new();
        for entry in &self.endpoints {
            if let Some(path) = &entry.path {
                out.entry(entry.platform).or_default().push(path.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TokioClock;

    const WORKSPACE_CONFIG: &str = include_str!("../../../config/default.toml");

    #[test]
    fn loads_workspace_default_config() {
        let config = NetConfig::from_toml(WORKSPACE_CONFIG).unwrap();
        assert_eq!(config.retry.base(), Duration::from_secs(1));
        assert_eq!(config.retry.cap(), Duration::from_secs(300));
        assert_eq!(config.retry.max_attempts(), DEFAULT_MAX_ATTEMPTS);
        let items = config
            .endpoints
            .iter()
            .find(|e| e.platform == Platform::Skinport && e.name == "items")
            .unwrap();
        assert_eq!(items.path.as_deref(), Some("/v1/items"));
        assert_eq!(
            items.limit,
            RateLimit::new(8, Duration::from_secs(300)).unwrap()
        );

        let limiter = config.build_limiter(Arc::new(TokioClock)).unwrap();
        let by_path = limiter.resolve(Platform::Skinport, "/v1/items").unwrap();
        let by_name = limiter.resolve(Platform::Skinport, "items").unwrap();
        assert!(Arc::ptr_eq(&by_path, &by_name));
        let history = limiter
            .resolve(Platform::Skinport, "/v1/sales/history")
            .unwrap();
        assert!(Arc::ptr_eq(&by_path, &history));
        assert_eq!(
            limiter
                .resolve(Platform::DMarket, "/exchange/v1/market/items")
                .unwrap()
                .limit()
                .capacity(),
            1
        );
        assert!(limiter.resolve(Platform::Skinport, "/v1/other").is_err());
    }

    #[test]
    fn quota_groups_share_one_bucket() {
        let source = r#"
            [rate_limits.skinport.items]
            path = "/v1/items"
            max_requests = 8
            window_seconds = 300
            quota_group = "skinport"

            [rate_limits.skinport.sales_history]
            path = "/v1/sales/history"
            max_requests = 8
            window_seconds = 300
            quota_group = "skinport"
        "#;
        let limiter = NetConfig::from_toml(source)
            .unwrap()
            .build_limiter(Arc::new(TokioClock))
            .unwrap();
        assert!(Arc::ptr_eq(
            &limiter.resolve(Platform::Skinport, "/v1/items").unwrap(),
            &limiter
                .resolve(Platform::Skinport, "/v1/sales/history")
                .unwrap()
        ));
        let conflicting = source.replacen("max_requests = 8", "max_requests = 4", 1);
        assert!(NetConfig::from_toml(&conflicting)
            .unwrap()
            .build_limiter(Arc::new(TokioClock))
            .is_err());
    }

    #[test]
    fn rejects_bad_config() {
        let bad = [
            "[rate_limits.steam.default]\nmax_requests = 1\nwindow_seconds = 1",
            "[rate_limits.csfloat.default]\nmax_requests = 0\nwindow_seconds = 1",
            "[rate_limits.csfloat.default]\nmax_requests = 1\nwindow_seconds = 0",
            "[rate_limits.csfloat.default]\nmax_requests = 1\nwindow_secs = 1",
            "[retry]\nbase_seconds = 10\ncap_seconds = 1",
            "[retry]\nbase_seconds = 1\ncap_seconds = 10\nmax_attempts = 0",
        ];
        for source in bad {
            assert!(NetConfig::from_toml(source).is_err(), "{source}");
        }
        let defaults = NetConfig::from_toml("").unwrap();
        assert_eq!(defaults.retry, RetryPolicy::default());
        assert!(defaults.endpoints.is_empty());
    }
}
