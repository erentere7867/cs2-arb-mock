use cs2_arb_core::{Error, Result};

/// An environment-only configuration value. Never implement Debug or Serialize for secrets.
pub struct Secret {
    _private: (),
}

impl Secret {
    pub fn from_env(_name: &str) -> Result<Self> {
        Err(Error::NotImplemented("environment-only secret loading"))
    }
}
