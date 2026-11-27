//! Exact integer-cent fee math.
//!
//! Core's [`FeeSchedule`] stores percentages as `f64` percentage points. The
//! simulator converts each percentage once, at construction, into an integer
//! rate in parts-per-million of the subtotal (`1% == 10_000 ppm`) and rejects
//! schedules that need more than four decimal places of a percentage point.
//! All subsequent arithmetic is integer (`i128` intermediates) with an explicit
//! [`RoundingMode`].
//!
//! Fees are assessed **per unit**: every CS2 item is its own marketplace
//! transaction, so `fee(q units at p) = q * (round(p * rate) + fixed)`. The
//! commission part is capped at the unit price (a 100% fee takes the whole
//! price, never more); the fixed part is added on top, as in core.

use cs2_arb_core::{Error, FeeSchedule, FeeSchedules, Money, Platform, Result};
use serde::{Deserialize, Serialize};

/// Denominator of the integer fee rate: rates are parts-per-million of subtotal.
pub const RATE_SCALE: i64 = 1_000_000;

/// How fractional cents of commission are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundingMode {
    /// Round to nearest; exact halves round up (away from zero). Matches core.
    #[default]
    HalfUp,
    /// Always round down (in the payer's favour).
    Floor,
    /// Always round up (in the marketplace's favour; most conservative).
    Ceil,
}

impl RoundingMode {
    /// `numerator / denominator` for `numerator >= 0`, `denominator > 0`.
    pub fn divide(self, numerator: i128, denominator: i128) -> i128 {
        debug_assert!(numerator >= 0 && denominator > 0);
        let quotient = numerator / denominator;
        let remainder = numerator % denominator;
        match self {
            Self::Floor => quotient,
            Self::Ceil => quotient + i128::from(remainder != 0),
            Self::HalfUp => quotient + i128::from(remainder * 2 >= denominator),
        }
    }
}

/// Convert percentage points (`2.5` = 2.5%) to an exact ppm rate.
pub fn pct_to_ppm(pct: f64) -> Result<i64> {
    if !pct.is_finite() || !(0.0..=100.0).contains(&pct) {
        return Err(Error::InvalidConfig(format!(
            "fee percentage {pct} must be finite and in [0, 100]"
        )));
    }
    let scaled = pct * 10_000.0;
    let rounded = scaled.round();
    if (scaled - rounded).abs() > 1e-6 {
        return Err(Error::InvalidConfig(format!(
            "fee percentage {pct} has more than four decimal places"
        )));
    }
    Ok(rounded as i64)
}

/// One side of a platform's fee schedule in exact form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeRate {
    /// Commission in parts-per-million of the unit price.
    pub rate_ppm: i64,
    /// Fixed fee per filled order, in cents.
    pub fixed: Money,
}

impl FeeRate {
    fn order_fee(self, unit_price: Money, quantity: u32, rounding: RoundingMode) -> Result<Money> {
        let commission = Self {
            fixed: Money(0),
            ..self
        }
        .unit_fee(unit_price, rounding)?;
        cs2_arb_core::fee_for_order(commission, self.fixed, quantity)
    }

    /// Fee for a single unit at `unit_price`.
    pub fn unit_fee(self, unit_price: Money, rounding: RoundingMode) -> Result<Money> {
        if unit_price.0 < 0 {
            return Err(Error::InvalidData("negative unit price".into()));
        }
        let raw = i128::from(unit_price.0) * i128::from(self.rate_ppm);
        let commission = rounding
            .divide(raw, i128::from(RATE_SCALE))
            .min(i128::from(unit_price.0));
        let commission = i64::try_from(commission).map_err(|_| Error::MoneyOverflow)?;
        Money(commission).checked_add(self.fixed)
    }
}

/// Exact version of a core [`FeeSchedule`] plus the rounding rule to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExactFeeSchedule {
    pub buyer: FeeRate,
    pub seller: FeeRate,
    pub crypto_deposit: FeeRate,
    pub crypto_withdrawal: FeeRate,
    pub bank_cashout: FeeRate,
    pub rounding: RoundingMode,
}

/// Result of applying a fee schedule to `quantity` units at `unit_price`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeBreakdown {
    pub quantity: u32,
    pub unit_price: Money,
    /// Whole-cent average fee per unit. Use `fee` for accounting.
    pub unit_fee: Money,
    /// `quantity * unit_price`.
    pub gross: Money,
    /// Total fee for the fill.
    pub fee: Money,
    /// Buy: `gross + fee` (cash paid). Sell: `gross - fee` (cash received).
    pub net: Money,
}

impl ExactFeeSchedule {
    pub fn from_schedule(schedule: FeeSchedule, rounding: RoundingMode) -> Result<Self> {
        schedule.validate()?;
        let rate = |charge: cs2_arb_core::FundingCharge| -> Result<FeeRate> {
            Ok(FeeRate {
                rate_ppm: pct_to_ppm(charge.pct)?,
                fixed: charge.fixed,
            })
        };
        Ok(Self {
            buyer: FeeRate {
                rate_ppm: pct_to_ppm(schedule.buyer_fee_pct)?,
                fixed: schedule.buyer_fixed_fee,
            },
            seller: FeeRate {
                rate_ppm: pct_to_ppm(schedule.seller_fee_pct)?,
                fixed: schedule.seller_fixed_fee,
            },
            crypto_deposit: rate(schedule.funding.crypto_deposit)?,
            crypto_withdrawal: rate(schedule.funding.crypto_withdrawal)?,
            bank_cashout: rate(schedule.funding.bank_cashout)?,
            rounding,
        })
    }

    pub fn buyer_unit_fee(&self, unit_price: Money) -> Result<Money> {
        self.buyer
            .unit_fee(unit_price, self.rounding)?
            .checked_add(self.crypto_deposit.unit_fee(unit_price, self.rounding)?)
    }

    pub fn seller_unit_fee(&self, unit_price: Money) -> Result<Money> {
        self.seller
            .unit_fee(unit_price, self.rounding)?
            .checked_add(self.crypto_withdrawal.unit_fee(unit_price, self.rounding)?)?
            .checked_add(self.bank_cashout.unit_fee(unit_price, self.rounding)?)
    }

    /// Cash paid by the buyer for `quantity` units, buyer marketplace fee included.
    pub fn buy(&self, unit_price: Money, quantity: u32) -> Result<FeeBreakdown> {
        self.buy_with_rates(unit_price, quantity, self.buyer, self.crypto_deposit)
    }

    pub(crate) fn buy_fill(
        &self,
        unit_price: Money,
        index: u32,
        order_quantity: u32,
    ) -> Result<(FeeBreakdown, Money, Money)> {
        let n = i64::from(order_quantity);
        let platform_remainder = self.buyer.fixed.0 % n;
        let share = |rate: FeeRate, offset: i64| FeeRate {
            fixed: Money(
                rate.fixed.0 / n + i64::from((i64::from(index) + offset) % n < rate.fixed.0 % n),
            ),
            ..rate
        };
        let buyer = share(self.buyer, 0);
        let deposit = share(self.crypto_deposit, n - platform_remainder);
        let fees = self.buy_with_rates(unit_price, 1, buyer, deposit)?;
        let allocated_deposit = deposit.order_fee(unit_price, 1, self.rounding)?;
        let fixed = self.buyer.fixed.checked_add(self.crypto_deposit.fixed)?;
        let allocated_fixed = buyer.fixed.checked_add(deposit.fixed)?;
        let unallocated_change =
            if index == 0 { fixed } else { Money(0) }.checked_sub(allocated_fixed)?;
        Ok((fees, allocated_deposit, unallocated_change))
    }

    fn buy_with_rates(
        &self,
        unit_price: Money,
        quantity: u32,
        buyer: FeeRate,
        deposit: FeeRate,
    ) -> Result<FeeBreakdown> {
        let gross = unit_price.checked_mul(quantity)?;
        let fee = buyer
            .order_fee(unit_price, quantity, self.rounding)?
            .checked_add(deposit.order_fee(unit_price, quantity, self.rounding)?)?;
        Ok(FeeBreakdown {
            quantity,
            unit_price,
            unit_fee: Money(fee.0.checked_div(i64::from(quantity)).unwrap_or(0)),
            gross,
            fee,
            net: gross.checked_add(fee)?,
        })
    }

    /// Cash received by the seller for `quantity` units, seller fee deducted.
    pub fn sell(&self, unit_price: Money, quantity: u32) -> Result<FeeBreakdown> {
        let gross = unit_price.checked_mul(quantity)?;
        let fee = self
            .seller
            .order_fee(unit_price, quantity, self.rounding)?
            .checked_add(
                self.crypto_withdrawal
                    .order_fee(unit_price, quantity, self.rounding)?,
            )?
            .checked_add(
                self.bank_cashout
                    .order_fee(unit_price, quantity, self.rounding)?,
            )?;
        Ok(FeeBreakdown {
            quantity,
            unit_price,
            unit_fee: Money(fee.0.checked_div(i64::from(quantity)).unwrap_or(0)),
            gross,
            fee,
            net: gross.checked_sub(fee)?,
        })
    }
}

/// Exact fee schedules for every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformFees {
    pub skinport: ExactFeeSchedule,
    pub csfloat: ExactFeeSchedule,
    pub dmarket: ExactFeeSchedule,
    pub waxpeer: ExactFeeSchedule,
}

impl PlatformFees {
    pub fn from_schedules(schedules: &FeeSchedules, rounding: RoundingMode) -> Result<Self> {
        Ok(Self {
            skinport: ExactFeeSchedule::from_schedule(schedules.skinport, rounding)?,
            csfloat: ExactFeeSchedule::from_schedule(schedules.csfloat, rounding)?,
            dmarket: ExactFeeSchedule::from_schedule(schedules.dmarket, rounding)?,
            waxpeer: ExactFeeSchedule::from_schedule(schedules.waxpeer, rounding)?,
        })
    }

    /// Same schedule on every platform (handy for single-venue tests).
    pub fn uniform(schedule: FeeSchedule, rounding: RoundingMode) -> Result<Self> {
        let exact = ExactFeeSchedule::from_schedule(schedule, rounding)?;
        Ok(Self {
            skinport: exact,
            csfloat: exact,
            dmarket: exact,
            waxpeer: exact,
        })
    }

    pub fn get(&self, platform: Platform) -> &ExactFeeSchedule {
        match platform {
            Platform::Skinport => &self.skinport,
            Platform::CsFloat => &self.csfloat,
            Platform::DMarket => &self.dmarket,
            Platform::Waxpeer => &self.waxpeer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(
        buyer_pct: f64,
        seller_pct: f64,
        buyer_fixed: i64,
        seller_fixed: i64,
    ) -> FeeSchedule {
        FeeSchedule {
            buyer_fee_pct: buyer_pct,
            seller_fee_pct: seller_pct,
            buyer_fixed_fee: Money(buyer_fixed),
            seller_fixed_fee: Money(seller_fixed),
            ..FeeSchedule::default()
        }
    }

    fn exact(s: FeeSchedule, rounding: RoundingMode) -> ExactFeeSchedule {
        ExactFeeSchedule::from_schedule(s, rounding).unwrap()
    }

    #[test]
    fn rounding_modes_on_exact_halves_and_fractions() {
        // 255 / 10 = 25.5
        assert_eq!(RoundingMode::HalfUp.divide(255, 10), 26);
        assert_eq!(RoundingMode::Floor.divide(255, 10), 25);
        assert_eq!(RoundingMode::Ceil.divide(255, 10), 26);
        // 2525 / 100 = 25.25
        assert_eq!(RoundingMode::HalfUp.divide(2525, 100), 25);
        assert_eq!(RoundingMode::Floor.divide(2525, 100), 25);
        assert_eq!(RoundingMode::Ceil.divide(2525, 100), 26);
        // exact division is unaffected
        for mode in [
            RoundingMode::HalfUp,
            RoundingMode::Floor,
            RoundingMode::Ceil,
        ] {
            assert_eq!(mode.divide(300, 10), 30);
            assert_eq!(mode.divide(0, 10), 0);
        }
    }

    #[test]
    fn percentage_conversion_is_exact_or_rejected() {
        assert_eq!(pct_to_ppm(0.0).unwrap(), 0);
        assert_eq!(pct_to_ppm(2.5).unwrap(), 25_000);
        assert_eq!(pct_to_ppm(8.0).unwrap(), 80_000);
        assert_eq!(pct_to_ppm(0.0001).unwrap(), 1);
        assert_eq!(pct_to_ppm(100.0).unwrap(), 1_000_000);
        // 0.1 + 0.2 is not exactly 0.3 in f64, but is within tolerance.
        assert_eq!(pct_to_ppm(0.1 + 0.2).unwrap(), 3_000);
        assert!(pct_to_ppm(0.00001).is_err());
        assert!(pct_to_ppm(-1.0).is_err());
        assert!(pct_to_ppm(100.5).is_err());
        assert!(pct_to_ppm(f64::NAN).is_err());
        assert!(pct_to_ppm(f64::INFINITY).is_err());
    }

    /// Worked example: Skinport-style 8% seller fee, $10.00 sale.
    /// 1000 * 8% = 80.00 -> fee 80c, proceeds 920c.
    #[test]
    fn worked_example_eight_percent_seller_fee() {
        let s = exact(schedule(0.0, 8.0, 0, 0), RoundingMode::HalfUp);
        let sell = s.sell(Money(1000), 1).unwrap();
        assert_eq!(sell.unit_fee, Money(80));
        assert_eq!(sell.gross, Money(1000));
        assert_eq!(sell.net, Money(920));
        // No buyer fee on this schedule.
        let buy = s.buy(Money(1000), 1).unwrap();
        assert_eq!(buy.fee, Money(0));
        assert_eq!(buy.net, Money(1000));
    }

    /// Worked example matching core's unit test: 2.5% + 2c buyer, 5% + 3c seller on $10.20.
    /// Buyer:  1020 * 2.5% = 25.5 -> 26 (half up) + 2 = 28 -> pays 1048.
    /// Seller: 1020 * 5%   = 51.0 -> 51         + 3 = 54 -> receives 966.
    #[test]
    fn worked_example_matches_core_fee_schedule() {
        let core = schedule(2.5, 5.0, 2, 3);
        let s = exact(core, RoundingMode::HalfUp);
        assert_eq!(s.buy(Money(1020), 1).unwrap().net, Money(1048));
        assert_eq!(s.sell(Money(1020), 1).unwrap().net, Money(966));
        assert_eq!(core.buyer_total(Money(1020)).unwrap(), Money(1048));
        assert_eq!(core.seller_proceeds(Money(1020)).unwrap(), Money(966));
    }

    /// Worked example of each rounding mode: 2.5% of $10.20 = 25.5c.
    #[test]
    fn worked_example_rounding_mode_choice() {
        let base = schedule(2.5, 0.0, 0, 0);
        assert_eq!(
            exact(base, RoundingMode::HalfUp)
                .buyer_unit_fee(Money(1020))
                .unwrap(),
            Money(26)
        );
        assert_eq!(
            exact(base, RoundingMode::Floor)
                .buyer_unit_fee(Money(1020))
                .unwrap(),
            Money(25)
        );
        assert_eq!(
            exact(base, RoundingMode::Ceil)
                .buyer_unit_fee(Money(1020))
                .unwrap(),
            Money(26)
        );
    }

    /// Worked example: 7% of $19.99 = 139.93c -> 140c; proceeds 1859c.
    /// For three units the fee is 3 * 140 = 420c, proceeds 5577c.
    #[test]
    fn worked_example_seven_percent_multi_unit() {
        let s = exact(schedule(0.0, 7.0, 0, 0), RoundingMode::HalfUp);
        let one = s.sell(Money(1999), 1).unwrap();
        assert_eq!(one.unit_fee, Money(140));
        assert_eq!(one.net, Money(1859));
        let three = s.sell(Money(1999), 3).unwrap();
        assert_eq!(three.gross, Money(5997));
        assert_eq!(three.fee, Money(420));
        assert_eq!(three.net, Money(5577));
    }

    /// Per-unit rounding differs from whole-line rounding, deliberately:
    /// 2.5% of $10.10 = 25.25c -> 25c per unit -> 4 units = 100c,
    /// whereas 2.5% of the $40.40 line would be 101c.
    #[test]
    fn worked_example_fees_round_per_unit() {
        let s = exact(schedule(2.5, 0.0, 0, 0), RoundingMode::HalfUp);
        let buy = s.buy(Money(1010), 4).unwrap();
        assert_eq!(buy.unit_fee, Money(25));
        assert_eq!(buy.fee, Money(100));
        assert_eq!(buy.net, Money(4140));
    }

    /// Tiny prices: 2% of 1c = 0.02c -> 0c (half up), 1c (ceil).
    /// A fixed fee larger than the price makes proceeds negative (the executor rejects that).
    #[test]
    fn worked_example_tiny_prices_and_fixed_fees() {
        let half_up = exact(schedule(0.0, 2.0, 0, 0), RoundingMode::HalfUp);
        assert_eq!(half_up.sell(Money(1), 1).unwrap().net, Money(1));
        let ceil = exact(schedule(0.0, 2.0, 0, 0), RoundingMode::Ceil);
        assert_eq!(ceil.sell(Money(1), 1).unwrap().net, Money(0));
        let fixed = exact(schedule(0.0, 0.0, 0, 5), RoundingMode::HalfUp);
        assert_eq!(fixed.sell(Money(3), 1).unwrap().net, Money(-2));
    }

    #[test]
    fn commission_is_capped_at_price_and_fixed_fee_added() {
        let s = exact(schedule(100.0, 100.0, 7, 0), RoundingMode::Ceil);
        assert_eq!(s.buyer_unit_fee(Money(500)).unwrap(), Money(507));
        assert_eq!(s.sell(Money(500), 2).unwrap().net, Money(0));
    }

    #[test]
    fn rejects_negative_prices_overflow_and_bad_schedules() {
        let s = exact(schedule(1.0, 1.0, 0, 0), RoundingMode::HalfUp);
        assert!(matches!(s.buy(Money(-1), 1), Err(Error::InvalidData(_))));
        assert!(matches!(
            s.buy(Money(i64::MAX), 2),
            Err(Error::MoneyOverflow)
        ));
        let fixed = exact(schedule(0.0, 0.0, 1, 0), RoundingMode::HalfUp);
        assert!(matches!(
            fixed.buy(Money(i64::MAX), 1),
            Err(Error::MoneyOverflow)
        ));
        assert!(
            ExactFeeSchedule::from_schedule(schedule(0.0, 0.0, -1, 0), RoundingMode::HalfUp)
                .is_err()
        );
        assert!(ExactFeeSchedule::from_schedule(
            schedule(f64::NAN, 0.0, 0, 0),
            RoundingMode::HalfUp
        )
        .is_err());
        assert!(ExactFeeSchedule::from_schedule(
            schedule(1.23456, 0.0, 0, 0),
            RoundingMode::HalfUp
        )
        .is_err());
    }

    #[test]
    fn loads_platform_fees_from_default_config() {
        let schedules =
            FeeSchedules::from_toml(include_str!("../../../config/default.toml")).unwrap();
        let fees = PlatformFees::from_schedules(&schedules, RoundingMode::HalfUp).unwrap();
        // Illustrative config: Skinport 8%, CSFloat 2%, DMarket 7% seller fees.
        assert_eq!(fees.get(Platform::Skinport).seller.rate_ppm, 80_000);
        assert_eq!(fees.get(Platform::CsFloat).seller.rate_ppm, 20_000);
        assert_eq!(fees.get(Platform::DMarket).seller.rate_ppm, 70_000);
        for platform in Platform::ALL {
            let core = schedules.get(platform);
            let ours = fees.get(platform);
            for price in [1, 99, 1020, 1999, 12_345, 250_000] {
                assert_eq!(
                    ours.sell(Money(price), 1).unwrap().net,
                    core.seller_proceeds(Money(price)).unwrap(),
                    "{platform:?} @ {price}"
                );
                assert_eq!(
                    ours.buy(Money(price), 1).unwrap().net,
                    core.buyer_total(Money(price)).unwrap()
                );
            }
        }
    }

    #[test]
    fn waxpeer_charges_funding_per_unit_and_separates_fee_components() {
        use cs2_arb_core::TradeFees;

        let schedules =
            FeeSchedules::from_toml(include_str!("../../../config/default.toml")).unwrap();
        let schedule = schedules.waxpeer;
        let exact = exact(schedule, RoundingMode::HalfUp);
        let buy = exact.buy(Money(1000), 2).unwrap();
        let sell = exact.sell(Money(1200), 2).unwrap();
        assert_eq!(buy.net, Money(2020));
        assert_eq!(sell.net, Money(2220));
        let fees = TradeFees::from_trade(
            schedule,
            schedule,
            buy.fee,
            sell.fee,
            buy.unit_price,
            sell.unit_price,
            2,
        )
        .unwrap()
        .unwrap();
        assert_eq!(fees.platform_buy, Money(0));
        assert_eq!(fees.platform_sell, Money(144));
        assert_eq!(fees.crypto_deposit, Money(20));
        assert_eq!(fees.crypto_withdrawal, Money(36));
        assert_eq!(fees.bank_cashout, Money(0));
    }
}
