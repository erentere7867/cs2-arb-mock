use serde::{Deserialize, Serialize};

use crate::{Error, Platform, Result};

/// Normalized USD cents; signed for P&L. Marketplace adapters normalize source currency.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FundingCharge {
    pub pct: f64,
    pub fixed: Money,
}

impl FundingCharge {
    pub fn unit_fee(self, price: Money) -> Result<Money> {
        fee(price, self.pct, self.fixed)
    }

    pub fn order_fee(self, unit_price: Money, quantity: u32) -> Result<Money> {
        fee_for_order(fee(unit_price, self.pct, Money(0))?, self.fixed, quantity)
    }

    fn valid(self) -> bool {
        (0.0..=100.0).contains(&self.pct) && self.fixed.0 >= 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FundingFees {
    pub crypto_deposit: FundingCharge,
    pub crypto_withdrawal: FundingCharge,
    pub bank_cashout: FundingCharge,
}

impl FundingFees {
    pub const NONE: Self = Self {
        crypto_deposit: FundingCharge {
            pct: 0.0,
            fixed: Money(0),
        },
        crypto_withdrawal: FundingCharge {
            pct: 0.0,
            fixed: Money(0),
        },
        bank_cashout: FundingCharge {
            pct: 0.0,
            fixed: Money(0),
        },
    };

    pub fn enabled(self) -> bool {
        self != Self::default()
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TradeFees {
    pub platform_buy: Money,
    pub platform_sell: Money,
    pub crypto_deposit: Money,
    pub crypto_withdrawal: Money,
    pub bank_cashout: Money,
}

impl TradeFees {
    pub fn from_trade(
        entry: FeeSchedule,
        exit: FeeSchedule,
        entry_fee: Money,
        exit_fee: Money,
        entry_unit: Money,
        exit_unit: Money,
        quantity: u32,
    ) -> Result<Option<Self>> {
        let deposit = entry
            .funding
            .crypto_deposit
            .order_fee(entry_unit, quantity)?;
        Self::from_allocated_trade(
            entry, exit, entry_fee, exit_fee, deposit, exit_unit, quantity,
        )
    }

    pub fn from_allocated_trade(
        entry: FeeSchedule,
        exit: FeeSchedule,
        entry_fee: Money,
        exit_fee: Money,
        crypto_deposit: Money,
        exit_unit: Money,
        quantity: u32,
    ) -> Result<Option<Self>> {
        if !entry.funding.enabled() && !exit.funding.enabled() {
            return Ok(None);
        }
        let crypto_withdrawal = exit
            .funding
            .crypto_withdrawal
            .order_fee(exit_unit, quantity)?;
        let bank_cashout = exit.funding.bank_cashout.order_fee(exit_unit, quantity)?;
        Ok(Some(Self {
            platform_buy: entry_fee.checked_sub(crypto_deposit)?,
            platform_sell: exit_fee
                .checked_sub(crypto_withdrawal)?
                .checked_sub(bank_cashout)?,
            crypto_deposit,
            crypto_withdrawal,
            bank_cashout,
        }))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct FeeSchedule {
    /// Percentage points: 5.0 denotes five percent.
    pub buyer_fee_pct: f64,
    pub seller_fee_pct: f64,
    pub buyer_fixed_fee: Money,
    pub seller_fixed_fee: Money,
    #[serde(default, skip_serializing_if = "funding_disabled")]
    pub funding: FundingFees,
}

fn funding_disabled(funding: &FundingFees) -> bool {
    !funding.enabled()
}

impl FeeSchedule {
    pub fn validate(self) -> Result<()> {
        if !(0.0..=100.0).contains(&self.buyer_fee_pct)
            || !(0.0..=100.0).contains(&self.seller_fee_pct)
            || self.buyer_fixed_fee.0 < 0
            || self.seller_fixed_fee.0 < 0
            || !self.funding.crypto_deposit.valid()
            || !self.funding.crypto_withdrawal.valid()
            || !self.funding.bank_cashout.valid()
        {
            return Err(Error::InvalidConfig(
                "fee percentages must be in [0, 100], and fixed fees must be nonnegative".into(),
            ));
        }
        Ok(())
    }

    pub fn buyer_fee(self, subtotal: Money) -> Result<Money> {
        self.validate()?;
        fee(subtotal, self.buyer_fee_pct, self.buyer_fixed_fee)?
            .checked_add(self.funding.crypto_deposit.unit_fee(subtotal)?)
    }

    pub fn seller_fee(self, subtotal: Money) -> Result<Money> {
        self.validate()?;
        fee(subtotal, self.seller_fee_pct, self.seller_fixed_fee)?
            .checked_add(self.funding.crypto_withdrawal.unit_fee(subtotal)?)?
            .checked_add(self.funding.bank_cashout.unit_fee(subtotal)?)
    }

    pub fn buyer_order_fee(self, unit_price: Money, quantity: u32) -> Result<Money> {
        self.validate()?;
        fee_for_order(
            fee(unit_price, self.buyer_fee_pct, Money(0))?,
            self.buyer_fixed_fee,
            quantity,
        )?
        .checked_add(
            self.funding
                .crypto_deposit
                .order_fee(unit_price, quantity)?,
        )
    }

    pub fn seller_order_fee(self, unit_price: Money, quantity: u32) -> Result<Money> {
        self.validate()?;
        fee_for_order(
            fee(unit_price, self.seller_fee_pct, Money(0))?,
            self.seller_fixed_fee,
            quantity,
        )?
        .checked_add(
            self.funding
                .crypto_withdrawal
                .order_fee(unit_price, quantity)?,
        )?
        .checked_add(self.funding.bank_cashout.order_fee(unit_price, quantity)?)
    }

    pub fn buyer_order_total(self, unit_price: Money, quantity: u32) -> Result<Money> {
        unit_price
            .checked_mul(quantity)?
            .checked_add(self.buyer_order_fee(unit_price, quantity)?)
    }

    pub fn seller_order_proceeds(self, unit_price: Money, quantity: u32) -> Result<Money> {
        unit_price
            .checked_mul(quantity)?
            .checked_sub(self.seller_order_fee(unit_price, quantity)?)
    }

    pub fn buyer_total(self, subtotal: Money) -> Result<Money> {
        subtotal.checked_add(self.buyer_fee(subtotal)?)
    }

    pub fn seller_proceeds(self, subtotal: Money) -> Result<Money> {
        subtotal.checked_sub(self.seller_fee(subtotal)?)
    }
}

pub fn fee_for_order(unit_commission: Money, fixed: Money, quantity: u32) -> Result<Money> {
    if quantity == 0 {
        return Ok(Money(0));
    }
    unit_commission.checked_mul(quantity)?.checked_add(fixed)
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
    #[serde(default = "default_waxpeer_fees")]
    pub waxpeer: FeeSchedule,
}

fn default_waxpeer_fees() -> FeeSchedule {
    FeeSchedule {
        seller_fee_pct: 6.0,
        ..FeeSchedule::default()
    }
}

impl FeeSchedules {
    pub fn from_toml(source: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Config {
            fees: FeeSchedules,
        }

        let config: Config =
            toml::from_str(source).map_err(|err| Error::InvalidConfig(err.to_string()))?;
        for platform in Platform::ALL {
            config.fees.get(platform).validate()?;
        }
        Ok(config.fees)
    }

    pub fn get(&self, platform: Platform) -> FeeSchedule {
        match platform {
            Platform::Skinport => self.skinport,
            Platform::CsFloat => self.csfloat,
            Platform::DMarket => self.dmarket,
            Platform::Waxpeer => self.waxpeer,
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
            ..FeeSchedule::default()
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
            ..FeeSchedule::default()
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
