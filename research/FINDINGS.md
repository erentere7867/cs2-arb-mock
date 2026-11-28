# Validation findings, 2026-11-28

Produced by the tools in this directory with the shipped `config/default.toml`
(researched fees with cash-out, robust dip baseline, volatility hurdle). Data:
CSFloat daily history for 34 items from 2024-11-28 to 2026-11-28, the latest
40 CSFloat prints per item, and the live Skinport and Waxpeer catalogues.
Rerun the tools to refresh. These are mock results, not forecasts.

## Price risk over the 7-day lock

- Median true daily σ of tradable skins is **1.54%**, so one σ over the lock
  is about **4.1%**. The 5th percentile 7-day move ranges from −5% (Printstreams)
  to −35% (Glock-18 Water Elemental). Robust σ understates these tails.
- CSFloat's round trip costs **5.91%** with cash-out. A strategy needs about
  6% plus one σ (roughly 10%) of expected edge before it is better than a coin flip.
- Daily averages of many skins are dominated by the day's float and sticker
  mix: noise is 3–23% for Redline, Slate, AWP Asiimov, Water Elemental and
  others. Any rule that buys low-average days shows fake mean reversion on
  them. `validate.py` marks such series ✗ and reports clean results separately.

## Spread (Skinport ↔ Waxpeer)

- 27.6% of 16,252 directed pairs look profitable against the other venue's
  lowest **ask**. That is the illusion the engine's spread exit was built on.
- Against Skinport's 7-day median **sale** price, 32 of 761 liquid
  Waxpeer→Skinport pairs clear the hurdle, and 27 still did 30 minutes later.
  None has the 70 weekly sales the simulator's 10%-of-volume cap needs to
  allow even one unit a day. **Tradable capacity today: zero.**

## Dip (robust baseline, volume surge, engine exit)

- 17 signals in two years, concentrated in **3 distinct weeks**. Mean +4.1%,
  but the 95% interval resampled by week is −5.8% to +4.8%. **No evidence of an edge.**
- The best signals came during market stress. The stress proxy independently
  found Valve's Trade Protection launch (16 Jul 2025) and the late-October
  2025 trade-up update. Those are exactly the trades the regime gate blocks:
  dip + regime gate keeps 10 trades at +0.9%. The dip thesis and the regime
  gate contradict each other.
- The Rust engine on the same history (`export_replay.py`) made 2 dip trades
  (+$10.74, both in the October 2025 window). Its $100 position cap rejected
  12 further signals on items above $100.

## Peg bid at the margin ceiling (CSFloat)

- On clean series, under the **engine's fill model** (any print at or below
  the bid fills it): 58 fills / 30 days at **+6.6%** net per fill.
- Under the **conservative model** (fills only when the market itself falls
  to the bid): 2.5 fills / 30 days at **−4.2%**.
- The whole peg result therefore depends on the fill assumption. The engine
  credits the bid with underpriced listings that sniping bots usually take in
  seconds. This is the most important unverified assumption in the engine.

## Update-aware trading (added later on 2026-11-28)

`fetch_updates.py` + `update_study.py`: 202 update days since the CS2 launch
(229 Steam announcements; 29 supply, 29 Major and 8 trade-rule changes, tagged
from the patch notes), against 15 clean case series and the 6 steadiest skins.

- **Only trade-rule updates move prices measurably.** Cases fall 2.9% across
  the release and 3.6% more over the following lock, with no rebound within two
  weeks. Both intervals exclude the every-day baseline. That is repricing, not
  a panic to buy. Supply and Major updates show no reliable average move.
- **No early signal on average:** the four days before an announcement show no
  move in any category, so watching staging and prerelease builds buys nothing
  on average. Individual events differ (cases rose 8–11% before the July 2025
  souvenir and Trade Protection posts).
- **Buying after item updates loses:** cases −8.2% per trade (interval −12.3% to
  −4.8%), skins −1.2%.
- **Dip inside update windows:** cases −10.0% per trade (interval excludes
  zero; the regime gate turns it into +0.6%). Skins +14.2% on 9 trades in 6
  weeks, but the same rule **outside** update windows also made +8.5% on 7
  trades. The update gate is not what makes it work, and 4 of the 9 came from
  one event: the October 2025 trade-up change, when covert skins rose because
  they became trade-up material.
- So the earlier note that the regime gate blocks "the panics dip is designed
  for" holds for skins only. On cases the gate prevented large losses.

The one lead worth following is the dip rule on liquid, clean skins regardless
of updates: 16 winning trades over three years on six skins. That is too few to
trust. A larger universe of clean skins is the next test.

## Engine issues found

1. `BuyOrderFillSimulator` fills a bid on any verified print at or below it.
   Treat peg results as an upper bound until fill rates are measured.
2. Percentage fees round to whole cents, so items under about $1 pay 0¢ fees
   (2% of 9¢ rounds to 0). Real venues charge minimums. Keep the universe at
   $1 and above.
3. The regime gate blocks most skin dip entries in panics, but it saved the dip rule on cases
   (see the update study above): keep it, and test it per item class.
4. `max_position_notional = 10000` ($100) excludes most skins above $100.

## What would change the verdict

- **Peg:** measure real fill rates. Recorded buy-book polls show how fast
  top-of-book depth is consumed at each price. A few days of recording
  bracket where real fills sit between the two models. Only real orders
  settle it.
- **Dip:** more episodes. Two years held three. Without more update panics
  there is no statistical basis to trade it, or to drop the regime gate for it.
- **Spread:** liquid pairs. Re-scan periodically; a pair that clears the
  hurdle at sale prices with at least 70 weekly sales would be the first
  tradable one.
