//! Environment-only secrets.
//!
//! [`Secret`] values can only be constructed from environment variables (or an
//! injected environment lookup in tests). They are redacted in `Debug` and
//! `Display`, intentionally do not implement `Serialize` or `Clone`, and their
//! heap buffer is zeroized on drop. The process environment itself still holds
//! a copy of the value; that copy is outside this crate's control.

use std::fmt;

use cs2_arb_core::{Error, Result};
use zeroize::Zeroize;

/// Placeholder printed instead of any secret value.
pub const REDACTED: &str = "[REDACTED]";

/// Environment variable names for the marketplace credentials.
pub mod vars {
    pub const SKINPORT_CLIENT_ID: &str = "SKINPORT_CLIENT_ID";
    pub const SKINPORT_CLIENT_SECRET: &str = "SKINPORT_CLIENT_SECRET";
    pub const CSFLOAT_API_KEY: &str = "CSFLOAT_API_KEY";
    pub const DMARKET_PUBLIC_KEY: &str = "DMARKET_PUBLIC_KEY";
    pub const DMARKET_SECRET_KEY: &str = "DMARKET_SECRET_KEY";

    pub const ALL: [&str; 5] = [
        SKINPORT_CLIENT_ID,
        SKINPORT_CLIENT_SECRET,
        CSFLOAT_API_KEY,
        DMARKET_PUBLIC_KEY,
        DMARKET_SECRET_KEY,
    ];
}

/// An environment-only configuration value.
///
/// Never `Serialize`, never `Clone`; `Debug`/`Display` print [`REDACTED`].
pub struct Secret {
    value: String,
}

impl Secret {
    /// Load a required secret from the process environment.
    ///
    /// Errors name the variable but never include its value. Empty or
    /// whitespace-only values are treated as missing.
    pub fn from_env(name: &str) -> Result<Self> {
        Self::from_lookup(name, |key| std::env::var(key).ok())
    }

    /// Load a required secret through an injected environment lookup. Lets tests
    /// avoid mutating the (process-global) environment.
    pub fn from_lookup<F>(name: &str, lookup: F) -> Result<Self>
    where
        F: FnOnce(&str) -> Option<String>,
    {
        match lookup(name) {
            Some(mut value) if !value.trim().is_empty() => {
                let trimmed = value.trim();
                let secret = if trimmed.len() == value.len() {
                    Self {
                        value: std::mem::take(&mut value),
                    }
                } else {
                    let secret = Self {
                        value: trimmed.to_owned(),
                    };
                    value.zeroize();
                    secret
                };
                Ok(secret)
            }
            Some(mut value) => {
                value.zeroize();
                Err(Error::InvalidConfig(format!(
                    "environment variable {name} is empty"
                )))
            }
            None => Err(Error::InvalidConfig(format!(
                "environment variable {name} is not set"
            ))),
        }
    }

    /// Borrow the raw value. Call sites must never log or persist the result.
    pub fn expose_secret(&self) -> &str {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({REDACTED})")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// All marketplace credentials. Each is optional because the mock simulation
/// never needs them; read-only clients that do need one should call the
/// matching `require_*` accessor.
#[derive(Debug, Default)]
pub struct Secrets {
    pub skinport_client_id: Option<Secret>,
    pub skinport_client_secret: Option<Secret>,
    pub csfloat_api_key: Option<Secret>,
    pub dmarket_public_key: Option<Secret>,
    pub dmarket_secret_key: Option<Secret>,
}

impl Secrets {
    /// Load every known credential from the process environment. Missing or
    /// empty variables become `None`.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`Secrets::from_env`] with an injected lookup (for tests).
    pub fn from_lookup<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let load = |name: &str| Secret::from_lookup(name, &lookup).ok();
        Self {
            skinport_client_id: load(vars::SKINPORT_CLIENT_ID),
            skinport_client_secret: load(vars::SKINPORT_CLIENT_SECRET),
            csfloat_api_key: load(vars::CSFLOAT_API_KEY),
            dmarket_public_key: load(vars::DMARKET_PUBLIC_KEY),
            dmarket_secret_key: load(vars::DMARKET_SECRET_KEY),
        }
    }

    /// Skinport client id and secret, both required.
    pub fn require_skinport(&self) -> Result<(&Secret, &Secret)> {
        Ok((
            required(&self.skinport_client_id, vars::SKINPORT_CLIENT_ID)?,
            required(&self.skinport_client_secret, vars::SKINPORT_CLIENT_SECRET)?,
        ))
    }

    pub fn require_csfloat(&self) -> Result<&Secret> {
        required(&self.csfloat_api_key, vars::CSFLOAT_API_KEY)
    }

    /// DMarket public and secret key, both required.
    pub fn require_dmarket(&self) -> Result<(&Secret, &Secret)> {
        Ok((
            required(&self.dmarket_public_key, vars::DMARKET_PUBLIC_KEY)?,
            required(&self.dmarket_secret_key, vars::DMARKET_SECRET_KEY)?,
        ))
    }

    /// Names of the variables that were found, for safe startup logging.
    pub fn present_vars(&self) -> Vec<&'static str> {
        [
            (vars::SKINPORT_CLIENT_ID, self.skinport_client_id.is_some()),
            (
                vars::SKINPORT_CLIENT_SECRET,
                self.skinport_client_secret.is_some(),
            ),
            (vars::CSFLOAT_API_KEY, self.csfloat_api_key.is_some()),
            (vars::DMARKET_PUBLIC_KEY, self.dmarket_public_key.is_some()),
            (vars::DMARKET_SECRET_KEY, self.dmarket_secret_key.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, present)| present.then_some(name))
        .collect()
    }
}

fn required<'a>(secret: &'a Option<Secret>, name: &str) -> Result<&'a Secret> {
    secret
        .as_ref()
        .ok_or_else(|| Error::InvalidConfig(format!("environment variable {name} is not set")))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    const VALUES: [(&str, &str); 5] = [
        (vars::SKINPORT_CLIENT_ID, "sp-id-SENTINEL-1111"),
        (vars::SKINPORT_CLIENT_SECRET, "sp-secret-SENTINEL-2222"),
        (vars::CSFLOAT_API_KEY, "csf-key-SENTINEL-3333"),
        (vars::DMARKET_PUBLIC_KEY, "dm-pub-SENTINEL-4444"),
        (vars::DMARKET_SECRET_KEY, "dm-sec-SENTINEL-5555"),
    ];

    fn fake_env() -> HashMap<String, String> {
        VALUES
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn assert_redacted(output: &str) {
        for (_, value) in VALUES {
            assert!(!output.contains(value), "secret leaked into: {output}");
            assert!(!output.contains("SENTINEL"), "secret leaked into: {output}");
        }
    }

    #[test]
    fn loads_all_secrets_from_lookup() {
        let env = fake_env();
        let secrets = Secrets::from_lookup(|k| env.get(k).cloned());
        assert_eq!(secrets.present_vars(), vars::ALL.to_vec());
        let (id, secret) = secrets.require_skinport().unwrap();
        assert_eq!(id.expose_secret(), "sp-id-SENTINEL-1111");
        assert_eq!(secret.expose_secret(), "sp-secret-SENTINEL-2222");
        assert_eq!(
            secrets.require_csfloat().unwrap().expose_secret(),
            "csf-key-SENTINEL-3333"
        );
        let (public, private) = secrets.require_dmarket().unwrap();
        assert_eq!(public.expose_secret(), "dm-pub-SENTINEL-4444");
        assert_eq!(private.expose_secret(), "dm-sec-SENTINEL-5555");
    }

    #[test]
    fn missing_and_empty_values_are_absent_and_errors_do_not_leak() {
        let secrets = Secrets::from_lookup(|k| match k {
            vars::CSFLOAT_API_KEY => Some("   ".into()),
            vars::DMARKET_PUBLIC_KEY => Some("dm-pub-SENTINEL-4444".into()),
            _ => None,
        });
        assert_eq!(secrets.present_vars(), vec![vars::DMARKET_PUBLIC_KEY]);
        let err = secrets.require_dmarket().unwrap_err().to_string();
        assert!(err.contains(vars::DMARKET_SECRET_KEY));
        assert_redacted(&err);
        assert!(secrets.require_csfloat().is_err());
        let err = Secret::from_lookup("X", |_| Some(String::new()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty"));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let secret = Secret::from_lookup("X", |_| Some("  abc\n".into())).unwrap();
        assert_eq!(secret.expose_secret(), "abc");
    }

    #[test]
    fn from_env_reads_process_environment_without_mutating_it() {
        // Read an always-present variable rather than calling set_var, which races
        // with concurrent env reads in the multi-threaded test harness.
        let expected = std::env::var("PATH").expect("PATH is set for cargo test");
        let secret = Secret::from_env("PATH").unwrap();
        assert_eq!(secret.expose_secret(), expected.trim());
        assert_eq!(format!("{secret:?}"), "Secret([REDACTED])");
        assert!(Secret::from_env("CS2_ARB_BACKTEST_DEFINITELY_UNSET_VAR").is_err());
    }

    #[test]
    fn debug_and_display_are_redacted() {
        let env = fake_env();
        let secrets = Secrets::from_lookup(|k| env.get(k).cloned());
        let debug = format!("{secrets:?}");
        let pretty = format!("{secrets:#?}");
        let display = format!("{}", secrets.require_csfloat().unwrap());
        assert_redacted(&debug);
        assert_redacted(&pretty);
        assert_redacted(&display);
        assert!(debug.contains("Secret([REDACTED])"));
        assert_eq!(display, REDACTED);
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn tracing_output_is_redacted() {
        let env = fake_env();
        let secrets = Secrets::from_lookup(|k| env.get(k).cloned());
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let key = secrets.require_csfloat().unwrap();
            tracing::info!(?secrets, "loaded secrets");
            tracing::warn!(api_key = %key, debug_key = ?key, "using key");
            tracing::error!("inline {key} {key:?}");
            let span = tracing::info_span!("client", secret = ?secrets.dmarket_secret_key);
            let _guard = span.enter();
            tracing::debug!(present = ?secrets.present_vars(), "inside span");
        });
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("loaded secrets"));
        assert!(output.contains("inside span"));
        assert!(output.contains(REDACTED));
        assert_redacted(&output);
    }
}
