use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use cs2_arb_core::{Error, FeeSchedules, Money, Platform, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueConfig {
    pub starting_cash: Money,
    pub sale_balance_hold_seconds: i64,
    pub trade_hold_seconds: i64,
    pub transfer_in_seconds: i64,
    pub transfer_out_seconds: i64,
}

#[cfg(test)]
impl VenueConfig {
    pub(crate) fn with_cash(cents: i64) -> Self {
        Self {
            starting_cash: Money(cents),
            sale_balance_hold_seconds: 0,
            trade_hold_seconds: 7 * 86400,
            transfer_in_seconds: 0,
            transfer_out_seconds: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transfer {
    pub at: DateTime<Utc>,
    pub from: Platform,
    pub to: Platform,
    pub amount: Money,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VenueAccountsConfig {
    pub venues: HashMap<Platform, VenueConfig>,
    pub transfers: Vec<Transfer>,
}

impl VenueAccountsConfig {
    pub fn validate(&self, venues: &[Platform]) -> Result<()> {
        let bad = |message: &str| Error::InvalidConfig(format!("venue_accounts: {message}"));
        if self.venues.len() != venues.len()
            || venues.iter().any(|venue| !self.venues.contains_key(venue))
        {
            return Err(bad("provide one account for every enabled venue"));
        }
        for venue in self.venues.values() {
            if venue.starting_cash.0 < 0
                || [
                    venue.sale_balance_hold_seconds,
                    venue.trade_hold_seconds,
                    venue.transfer_in_seconds,
                    venue.transfer_out_seconds,
                ]
                .iter()
                .any(|&seconds| seconds < 0 || TimeDelta::try_seconds(seconds).is_none())
            {
                return Err(bad("cash and delays must be nonnegative and representable"));
            }
        }
        for transfer in &self.transfers {
            if transfer.amount.0 <= 0
                || transfer.from == transfer.to
                || !self.venues.contains_key(&transfer.from)
                || !self.venues.contains_key(&transfer.to)
            {
                return Err(bad(
                    "transfers must be positive and connect different enabled venues",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueBalance {
    pub platform: Platform,
    pub available: Money,
    pub pending: Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueTradeHold {
    pub platform: Platform,
    pub seconds: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VenueAccounts {
    rules: VenueAccountsConfig,
    fees: FeeSchedules,
    available: HashMap<Platform, Money>,
    pending: Vec<(DateTime<Utc>, Platform, Money)>,
    next_transfer: usize,
    transfer_fees_paid: Money,
}

impl VenueAccounts {
    pub fn new(mut rules: VenueAccountsConfig, fees: FeeSchedules) -> Self {
        rules.transfers.sort_by_key(|transfer| transfer.at);
        Self {
            available: rules
                .venues
                .iter()
                .map(|(&platform, venue)| (platform, venue.starting_cash))
                .collect(),
            pending: Vec::new(),
            rules,
            fees,
            next_transfer: 0,
            transfer_fees_paid: Money(0),
        }
    }

    pub fn current(&self, platform: Platform) -> Money {
        self.available.get(&platform).copied().unwrap_or_default()
    }

    pub fn transfer_fees_paid(&self) -> Money {
        self.transfer_fees_paid
    }

    pub fn sale_balance_hold(&self, platform: Platform) -> Option<i64> {
        self.rules
            .venues
            .get(&platform)
            .map(|venue| venue.sale_balance_hold_seconds)
    }

    pub fn trade_unlock(&self, platform: Platform, at: DateTime<Utc>) -> Result<DateTime<Utc>> {
        after(at, self.venue(platform)?.trade_hold_seconds)
    }

    pub fn trade_holds_for(&self, platforms: &[Platform]) -> Vec<VenueTradeHold> {
        Platform::ALL
            .into_iter()
            .filter(|platform| platforms.contains(platform))
            .filter_map(|platform| {
                self.rules
                    .venues
                    .get(&platform)
                    .map(|venue| VenueTradeHold {
                        platform,
                        seconds: venue.trade_hold_seconds,
                    })
            })
            .collect()
    }

    pub fn debit(&mut self, platform: Platform, amount: Money) -> Result<()> {
        self.venue(platform)?;
        let cash = self.current(platform);
        if cash < amount {
            return Err(Error::InvalidData(format!(
                "insufficient mock funds on {platform:?}"
            )));
        }
        self.available.insert(platform, cash.checked_sub(amount)?);
        Ok(())
    }

    pub fn credit_sale(
        &mut self,
        platform: Platform,
        amount: Money,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let release = after(at, self.venue(platform)?.sale_balance_hold_seconds)?;
        self.pending.push((release, platform, amount));
        Ok(())
    }

    fn venue(&self, platform: Platform) -> Result<&VenueConfig> {
        self.rules
            .venues
            .get(&platform)
            .ok_or_else(|| Error::InvalidConfig(format!("no mock account for {platform:?}")))
    }

    pub fn advance(&mut self, now: DateTime<Utc>) -> Result<()> {
        while let Some(transfer) = self
            .rules
            .transfers
            .get(self.next_transfer)
            .cloned()
            .filter(|t| t.at <= now)
        {
            self.release(transfer.at)?;
            let from = self.venue(transfer.from)?;
            let to = self.venue(transfer.to)?;
            let at = after(
                after(transfer.at, from.transfer_out_seconds)?,
                to.transfer_in_seconds,
            )?;
            let withdrawal = self
                .fees
                .get(transfer.from)
                .funding
                .crypto_withdrawal
                .unit_fee(transfer.amount)?;
            let withdrawn = transfer.amount.checked_sub(withdrawal)?;
            let deposit = self
                .fees
                .get(transfer.to)
                .funding
                .crypto_deposit
                .unit_fee(withdrawn)?;
            let credited = withdrawn.checked_sub(deposit)?;
            if credited.0 <= 0 {
                return Err(Error::InvalidData(
                    "mock transfer fees consume the transfer amount".into(),
                ));
            }
            let transfer_fees_paid = self
                .transfer_fees_paid
                .checked_add(transfer.amount.checked_sub(credited)?)?;
            self.debit(transfer.from, transfer.amount)?;
            self.transfer_fees_paid = transfer_fees_paid;
            self.pending.push((at, transfer.to, credited));
            self.next_transfer += 1;
        }
        self.release(now)
    }

    fn release(&mut self, now: DateTime<Utc>) -> Result<()> {
        let mut index = 0;
        while index < self.pending.len() {
            let (at, platform, amount) = self.pending[index];
            if at <= now {
                self.available
                    .insert(platform, self.current(platform).checked_add(amount)?);
                self.pending.swap_remove(index);
            } else {
                index += 1;
            }
        }
        Ok(())
    }

    pub fn balances(&self) -> Result<Vec<VenueBalance>> {
        Platform::ALL
            .into_iter()
            .filter(|platform| self.available.contains_key(platform))
            .map(|platform| {
                Ok(VenueBalance {
                    platform,
                    available: self.current(platform),
                    pending: self
                        .pending
                        .iter()
                        .filter(|(_, p, _)| *p == platform)
                        .try_fold(Money(0), |acc, (_, _, amount)| acc.checked_add(*amount))?,
                })
            })
            .collect()
    }
}

fn after(at: DateTime<Utc>, seconds: i64) -> Result<DateTime<Utc>> {
    TimeDelta::try_seconds(seconds)
        .and_then(|delta| at.checked_add_signed(delta))
        .ok_or_else(|| Error::InvalidData("mock settlement time is out of range".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfers_charge_withdrawal_and_deposit_before_delayed_credit() {
        let start = DateTime::<Utc>::UNIX_EPOCH;
        let from = Platform::Waxpeer;
        let to = Platform::DMarket;
        let rules = VenueAccountsConfig {
            venues: HashMap::from([
                (
                    from,
                    VenueConfig {
                        transfer_out_seconds: 60,
                        ..VenueConfig::with_cash(5000)
                    },
                ),
                (
                    to,
                    VenueConfig {
                        transfer_in_seconds: 120,
                        sale_balance_hold_seconds: 300,
                        ..VenueConfig::with_cash(0)
                    },
                ),
            ]),
            transfers: vec![Transfer {
                at: start,
                from,
                to,
                amount: Money(1000),
            }],
        };
        let mut fees =
            FeeSchedules::from_toml(include_str!("../../../config/default.toml")).unwrap();
        fees.waxpeer.funding.crypto_withdrawal.fixed = Money(10);
        fees.dmarket.funding.crypto_deposit.fixed = Money(5);
        rules.validate(&[from, to]).unwrap();
        let mut accounts = VenueAccounts::new(rules, fees);
        accounts.advance(start).unwrap();
        assert_eq!(accounts.transfer_fees_paid(), Money(30));
        let balances = accounts.balances().unwrap();
        assert_eq!(
            balances
                .iter()
                .map(|b| b.available.0 + b.pending.0)
                .sum::<i64>()
                + accounts.transfer_fees_paid().0,
            5000
        );
        assert_eq!(accounts.current(from), Money(4000));
        assert_eq!(accounts.current(to), Money(0));
        accounts.credit_sale(to, Money(500), start).unwrap();
        accounts.advance(start + TimeDelta::seconds(179)).unwrap();
        assert_eq!(accounts.current(to), Money(0));
        accounts.advance(start + TimeDelta::seconds(180)).unwrap();
        assert_eq!(accounts.current(to), Money(970));
        accounts.advance(start + TimeDelta::seconds(300)).unwrap();
        assert_eq!(accounts.current(to), Money(1470));
    }
}
