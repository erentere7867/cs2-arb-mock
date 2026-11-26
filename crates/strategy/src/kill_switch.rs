//! Emergency kill switch that blocks all new mock orders.
//!
//! Three triggers, any of which engages the switch:
//! * the in-process [`AtomicBool`] (shared via `Arc`, e.g. set from a signal handler),
//! * the presence of a sentinel file (conventionally `KILL_SWITCH`),
//! * an environment variable (conventionally `KILL_SWITCH`) set to a truthy value.
//!
//! The switch **latches**: once a file or env trigger is seen, the atomic flag is set and
//! stays set even if the file is removed, until [`KillSwitch::reset`] is called explicitly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Conventional sentinel file name and environment variable name.
pub const DEFAULT_KILL_SWITCH_NAME: &str = "KILL_SWITCH";

#[derive(Debug, Clone, Default)]
pub struct KillSwitch {
    flag: Arc<AtomicBool>,
    file: Option<PathBuf>,
    env_var: Option<String>,
}

impl KillSwitch {
    /// Flag-only switch (no file or env trigger).
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch watching `./KILL_SWITCH` and the `KILL_SWITCH` env var.
    pub fn with_defaults() -> Self {
        Self::new()
            .with_file(DEFAULT_KILL_SWITCH_NAME)
            .with_env_var(DEFAULT_KILL_SWITCH_NAME)
    }

    pub fn with_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.file = Some(path.into());
        self
    }

    pub fn with_env_var(mut self, name: impl Into<String>) -> Self {
        self.env_var = Some(name.into());
        self
    }

    /// Shared flag, e.g. for a Ctrl-C handler. Storing `true` engages the switch.
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Engages the switch immediately for every clone sharing this flag.
    pub fn engage(&self) {
        if !self.flag.swap(true, Ordering::SeqCst) {
            tracing::warn!("kill switch engaged; all new mock orders are blocked");
        }
    }

    /// Explicit operator reset of the latched flag. File/env triggers still apply.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Checks the flag, the sentinel file and the env var. Latches on file/env triggers.
    pub fn is_engaged(&self) -> bool {
        if self.flag.load(Ordering::SeqCst) {
            return true;
        }
        let file_hit = self.file.as_deref().is_some_and(Path::exists);
        let env_hit = self.env_var.as_deref().is_some_and(|name| {
            std::env::var(name)
                .ok()
                .as_deref()
                .is_some_and(env_value_engages)
        });
        if file_hit || env_hit {
            self.engage();
            return true;
        }
        false
    }
}

/// Any non-empty value except `0`, `false`, `no` or `off` (case-insensitive) engages.
pub fn env_value_engages(value: &str) -> bool {
    let v = value.trim();
    !v.is_empty()
        && !["0", "false", "no", "off"]
            .iter()
            .any(|f| v.eq_ignore_ascii_case(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cs2-arb-strategy-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn flag_engages_all_clones_immediately() {
        let ks = KillSwitch::new();
        let clone = ks.clone();
        assert!(!clone.is_engaged());
        ks.flag().store(true, Ordering::SeqCst);
        assert!(clone.is_engaged());
        clone.reset();
        assert!(!ks.is_engaged());
        ks.engage();
        assert!(clone.is_engaged());
    }

    #[test]
    fn sentinel_file_engages_and_latches() {
        let path = temp_path("file");
        let ks = KillSwitch::new().with_file(&path);
        assert!(!ks.is_engaged());
        std::fs::write(&path, b"stop").unwrap();
        assert!(ks.is_engaged());
        std::fs::remove_file(&path).unwrap();
        assert!(ks.is_engaged(), "latched after the file is removed");
        ks.reset();
        assert!(!ks.is_engaged());
    }

    #[test]
    fn env_var_values() {
        for v in ["1", "true", "yes", "STOP"] {
            assert!(env_value_engages(v), "{v}");
        }
        for v in ["", " ", "0", "false", "FALSE", "no", "off"] {
            assert!(!env_value_engages(v), "{v:?}");
        }
        // A variable that is never set does not engage.
        let ks = KillSwitch::new().with_env_var("CS2_ARB_STRATEGY_TEST_UNSET_KILL_SWITCH_7f3a");
        assert!(!ks.is_engaged());
    }
}
