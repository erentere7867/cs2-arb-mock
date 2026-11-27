use cs2_arb_core::Platform;

pub fn unverified(platforms: &[Platform]) -> Vec<String> {
    let mut notes = vec!["UNVERIFIED: runtime fee tier, funding rail costs, account eligibility, settlement delays, and simulation parameters are scenario inputs, not live quotes.".into()];
    for platform in Platform::ALL.into_iter().filter(|p| platforms.contains(p)) {
        notes.push(match platform {
            Platform::Skinport => "UNVERIFIED — Skinport: current per-item fee, payout timing for this account/game, and exact price-level depth or individual sales (public endpoints publish aggregates only).",
            Platform::CsFloat => "UNVERIFIED — CSFloat: public API selectors/schema, buy-order route and item eligibility, available instant-sale depth, anonymous access, rate quota, trade locks, settlement timing, and effective fees.",
            Platform::DMarket => "UNVERIFIED — DMarket: account-specific fees, quotas and escrow, signed production responses, target funding/executability, and distinct IDs for identical sales.",
            Platform::Waxpeer => "UNVERIFIED — Waxpeer: current rate quota, account/network costs, settlement policy by game, and actual executable bids including shared owner funding. Public max bid is indicative; detailed bids model one unit per distinct eligible order.",
        }.into());
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_every_selected_venue_but_does_not_claim_unselected_venues() {
        let notes = unverified(&[Platform::Skinport, Platform::Waxpeer, Platform::Waxpeer]);
        assert_eq!(notes.len(), 3);
        assert!(notes.iter().all(|note| note.starts_with("UNVERIFIED")));
        assert!(notes[1].contains("Skinport"));
        assert!(notes[2].contains("Waxpeer"));
    }
}
