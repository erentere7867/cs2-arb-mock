use serde::{Deserialize, Serialize};

use crate::{Error, Platform, Result};

/// Normalized USD cents; signed for P&L. Marketplace adapters normalize source currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Money(pub i64);

impl Money {
    pub fn checked_add(self, rhs: Self) -> Result<Self> {
        self.0
            .checked_add(rhs.0)
            .map(Self)
            .ok_or(Error::MoneyOverflow)
    }

    pub fn checked_sub(self, rhs: Self) -> Result<Self> {
        self.0
            .checked_sub(rhs.0)
            .map(Self)
            .ok_or(Error::MoneyOverflow)
    }

    pub fn checked_mul(self, quantity: u32) -> Result<Self> {
        self.0
            .checked_mul(i64::from(quantity))
            .map(Self)
            .ok_or(Error::MoneyOverflow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FeeSchedule {
    /// Percentage points: 5.0 denotes five percent.
    pub buyer_fee_pct: f64,
    pub seller_fee_pct: f64,
    pub buyer_fixed_fee: Money,
    pub seller_fixed_fee: Money,
}

impl FeeSchedule {
    pub fn validate(self) -> Result<()> {
        if !(0.0..=100.0).contains(&self.buyer_fee_pct)
            || !(0.0..=100.0).contains(&self.seller_fee_pct)
            || self.buyer_fixed_fee.0 < 0
            || self.seller_fixed_fee.0 < 0
        {
            return Err(Error::InvalidConfig(
                "fee percentages must be in [0, 100], and fixed fees must be nonnegative".into(),
            ));
        }
        Ok(())
    }

    pub fn buyer_fee(self, subtotal: Money) -> Result<Money> {
        self.validate()?;
        fee(subtotal, self.buyer_fee_pct, self.buyer_fixed_fee)
    }

    pub fn seller_fee(self, subtotal: Money) -> Result<Money> {
        self.validate()?;
        fee(subtotal, self.seller_fee_pct, self.seller_fixed_fee)
    }

    pub fn buyer_total(self, subtotal: Money) -> Result<Money> {
        subtotal.checked_add(self.buyer_fee(subtotal)?)
    }

    pub fn seller_proceeds(self, subtotal: Money) -> Result<Money> {
        subtotal.checked_sub(self.seller_fee(subtotal)?)
    }
}

fn fee(subtotal: Money, pct: f64, fixed: Money) -> Result<Money> {
    if subtotal.0 < 0 {
        return Err(Error::InvalidData("negative order subtotal".into()));
    }
    let commission = (subtotal.0 as f64 * (pct / 100.0)).round();
    Money((commission as i64).min(subtotal.0)).checked_add(fixed)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FeeSchedules {
    pub skinport: FeeSchedule,
    pub csfloat: FeeSchedule,
    pub dmarket: FeeSchedule,
}

impl FeeSchedules {
    pub fn from_toml(source: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Config {
            fees: FeeSchedules,
        }

        let config: Config =
            toml::from_str(source).map_err(|err| Error::InvalidConfig(err.to_string()))?;
        for platform in [Platform::Skinport, Platform::CsFloat, Platform::DMarket] {
            config.fees.get(platform).validate()?;
        }
        Ok(config.fees)
    }

    pub fn get(&self, platform: Platform) -> FeeSchedule {
        match platform {
            Platform::Skinport => self.skinport,
            Platform::CsFloat => self.csfloat,
            Platform::DMarket => self.dmarket,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_percentage_and_fixed_fees_to_both_sides() {
        let schedule = FeeSchedule {
            buyer_fee_pct: 2.5,
            seller_fee_pct: 5.0,
            buyer_fixed_fee: Money(2),
            seller_fixed_fee: Money(3),
        };
        assert_eq!(schedule.buyer_total(Money(1020)).unwrap(), Money(1048));
        assert_eq!(schedule.seller_proceeds(Money(1020)).unwrap(), Money(966));
        assert!(schedule.buyer_fee(Money(-1)).is_err());
    }

    #[test]
    fn rejects_invalid_config_and_detects_overflow() {
        let schedule = FeeSchedule {
            buyer_fee_pct: 0.0,
            seller_fee_pct: 0.0,
            buyer_fixed_fee: Money(1),
            seller_fixed_fee: Money(0),
        };
        assert!(matches!(
            schedule.buyer_total(Money(i64::MAX)),
            Err(Error::MoneyOverflow)
        ));
        assert!(FeeSchedule {
            seller_fee_pct: f64::NAN,
            ..schedule
        }
        .validate()
        .is_err());
    }

    #[test]
    fn loads_configured_platform_fees() {
        let original = include_str!("../../../config/default.toml");
        let schedules = FeeSchedules::from_toml(original).unwrap();
        let changed = original.replace("seller_fee_pct = 8.0", "seller_fee_pct = 3.5");
        let changed = FeeSchedules::from_toml(&changed).unwrap();
        assert_ne!(
            schedules.get(Platform::Skinport),
            changed.get(Platform::Skinport)
        );
        assert_eq!(changed.get(Platform::Skinport).seller_fee_pct, 3.5);
        assert_eq!(
            schedules.get(Platform::CsFloat),
            changed.get(Platform::CsFloat)
        );
    }
}
