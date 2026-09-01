//! The external reference price: fetch, parse, and the two validations that
//! stand between a bad feed and a real trade (design §6).
//!
//! Everything here is pure except [`fetch`]. The float the feed publishes is
//! converted to an integer **once**, at the edge, by [`micro_usd_from_rate`];
//! `p_micro` is what leaves this module and every comparison downstream of it
//! is integer (design §3).
//!
//! # Why the timestamp is parsed by hand
//!
//! The feed publishes RFC 3339 strings (`"2026-09-01T13:15:33Z"`), not Unix
//! seconds. The design's dependency list (§5) is `reqwest`, `serde_json`,
//! `clap`, `tokio`, `tracing`, `anyhow` — no date library — so
//! [`parse_rfc3339_utc`] does the conversion itself: a strict grammar (a `Z`
//! suffix only, no offsets) plus Howard Hinnant's `days_from_civil`, which is
//! exact integer arithmetic over the whole proleptic Gregorian calendar. A
//! wrong answer here would silently disable the staleness check, so the parser
//! rejects everything it does not fully understand rather than guessing, and
//! its tests pin known epochs on both sides of 1970 and across leap years.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The feed the design names (§8). Published by fedi; `BTC/USD` is its only
/// crypto pair, which is what makes the "1 USDt = 1 USD" assumption (§2)
/// load-bearing rather than incidental.
pub const DEFAULT_ORACLE_URL: &str = "https://price-feed.dev.fedibtc.com/latest";

/// The one pair this bot reads.
pub const BTC_USD: &str = "BTC/USD";

/// Ceiling on the parsed price, in micro-USD per BTC: 10^18, i.e. one trillion
/// USD per BTC.
///
/// Not a plausibility check on the market — it is the bound that keeps §3's
/// cross-multiplication inside `i128`. The largest intermediate downstream is
/// `reserve * p_micro * 10_000`, and with `reserve <= MAX_RESERVE` (`2^58`,
/// ~2.9e17) that is `2.9e17 * 1e18 * 1e4 = 2.9e39` — which would *not* fit, so
/// the real work is done by the fact that a price anywhere near this ceiling
/// is rejected long before it reaches the policy: at any price the feed can
/// plausibly publish (`<= 1e9` USD/BTC, i.e. `p_micro <= 1e15`) the product is
/// `<= 2.9e36`, inside `i128`. The ceiling exists so that a corrupt feed
/// publishing `1e300` is a parse error rather than a saturating cast.
pub const MAX_MICRO_USD_PER_BTC: u128 = 1_000_000_000_000_000_000;

/// The same ceiling as a `f64`, so the one comparison that has to happen in
/// floating point does not need a cast of the `u128` constant. `1e18` is
/// exactly representable.
const MAX_MICRO_USD_PER_BTC_F64: f64 = 1e18;

/// The feed's top-level shape: `{"prices": {"BTC/USD": {...}, ...}}`.
///
/// `BTreeMap` rather than a struct with one field per pair: the feed publishes
/// 67 pairs and adding a 68th must not be a parse error.
#[derive(Debug, Clone, Deserialize)]
pub struct Feed {
    pub prices: BTreeMap<String, FeedQuote>,
}

/// One pair's entry. `rate` is USD per unit of the base currency, so for
/// `BTC/USD` it is USD per BTC.
#[derive(Debug, Clone, Deserialize)]
pub struct FeedQuote {
    pub rate: f64,
    /// RFC 3339, always `Z`-suffixed in every response observed.
    pub timestamp: String,
}

/// A validated reference price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OraclePrice {
    /// `round(rate * 1e6)` micro-USD per BTC. The only number the policy sees.
    pub micro_usd_per_btc: u128,
    /// The feed's own timestamp, Unix seconds.
    pub timestamp_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OracleError {
    #[error("feed is not valid JSON of the expected shape: {0}")]
    Malformed(String),
    #[error("feed has no `{BTC_USD}` pair")]
    MissingPair,
    #[error("rate {0} is not a finite positive number")]
    BadRate(String),
    #[error("rate {0} is above the {MAX_MICRO_USD_PER_BTC} micro-USD/BTC ceiling")]
    RateOutOfRange(String),
    #[error("timestamp {0:?} is not an RFC 3339 UTC instant")]
    BadTimestamp(String),
    #[error("price is {age_secs}s old, limit is {max_age_secs}s")]
    Stale { age_secs: i64, max_age_secs: u64 },
    #[error("rate moved {moved_pct}% since the last accepted tick, limit is {max_pct}%")]
    Jump { moved_pct: u128, max_pct: u64 },
}

/// GETs `url` and returns the body. The only I/O in this module.
pub async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    Ok(http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// Parses a feed body and applies the staleness check (design §6).
///
/// `now_unix` is a parameter rather than read from the clock here, so every
/// staleness case is a unit test rather than something only reproducible by
/// waiting.
pub fn parse_feed(
    body: &str,
    now_unix: i64,
    max_age_secs: u64,
) -> Result<OraclePrice, OracleError> {
    let feed: Feed =
        serde_json::from_str(body).map_err(|error| OracleError::Malformed(error.to_string()))?;

    let quote = feed.prices.get(BTC_USD).ok_or(OracleError::MissingPair)?;

    let micro_usd_per_btc = micro_usd_from_rate(quote.rate)?;
    let timestamp_unix = parse_rfc3339_utc(&quote.timestamp)
        .ok_or_else(|| OracleError::BadTimestamp(quote.timestamp.clone()))?;

    // Signed on purpose. A feed timestamped far in the *future* is as much a
    // reason to refuse as one far in the past — it would otherwise defeat the
    // staleness check entirely by making `age` permanently negative — so the
    // same limit is applied to both directions and reported as one error.
    let age_secs = now_unix - timestamp_unix;
    let max = i64::try_from(max_age_secs).unwrap_or(i64::MAX);
    if age_secs > max || age_secs < -max {
        return Err(OracleError::Stale {
            age_secs,
            max_age_secs,
        });
    }

    Ok(OraclePrice {
        micro_usd_per_btc,
        timestamp_unix,
    })
}

/// `round(rate * 1e6)`, the one and only float-to-integer conversion (design
/// §3).
///
/// The `as` cast is the sole one in this crate and is not a narrowing cast: it
/// runs only after `micro` has been proven finite and inside
/// `[1, MAX_MICRO_USD_PER_BTC]`, a range every value of which is exactly
/// representable as `u128`.
pub fn micro_usd_from_rate(rate: f64) -> Result<u128, OracleError> {
    if !rate.is_finite() || rate <= 0.0 {
        return Err(OracleError::BadRate(rate.to_string()));
    }

    let micro = (rate * 1e6).round();

    if micro < 1.0 {
        // A positive rate that rounds to zero micro-USD is a price of less
        // than 1e-6 USD per BTC: not a price, a corrupt feed.
        return Err(OracleError::BadRate(rate.to_string()));
    }
    if micro > MAX_MICRO_USD_PER_BTC_F64 {
        return Err(OracleError::RateOutOfRange(rate.to_string()));
    }

    Ok(micro as u128)
}

/// Tick-over-tick sanity limit (design §6): refuse a rate that moved more than
/// `max_pct` against the last accepted one, in either direction.
///
/// Integer throughout: `|new - prev| * 100 > prev * max_pct`. The first tick
/// has no predecessor and is never passed through here — it is accepted on
/// staleness alone.
pub fn check_jump(prev_micro: u128, new_micro: u128, max_pct: u64) -> Result<(), OracleError> {
    let delta = prev_micro.abs_diff(new_micro);
    let moved_bound = prev_micro.saturating_mul(u128::from(max_pct));

    if delta.saturating_mul(100) > moved_bound {
        return Err(OracleError::Jump {
            // Reported, not decided with: the comparison above is exact, this
            // is the same quantity floored to whole percent for the log line.
            // A zero predecessor has no percentage; it is also unreachable,
            // since only an accepted price is ever remembered and every
            // accepted price is non-zero.
            moved_pct: delta
                .saturating_mul(100)
                .checked_div(prev_micro)
                .unwrap_or(u128::MAX),
            max_pct,
        });
    }

    Ok(())
}

/// `"YYYY-MM-DDTHH:MM:SS[.fff][Z]"` -> Unix seconds, or `None`.
///
/// Strict by design (see the module docs): the date and time separators, field
/// widths and ranges must all be exactly right, a fractional part is accepted
/// and discarded, and the only zone accepted is `Z`/`z`. A numeric offset
/// (`+02:00`) is rejected rather than silently read as UTC — the feed has
/// never published one, and reading one wrong would shift staleness by hours.
pub fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    // `YYYY-MM-DDTHH:MM:SS` is 19 bytes, the shortest form accepted.
    if bytes.len() < 19 {
        return None;
    }

    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse::<i64>().ok() };

    // `parse::<i64>` would accept `"+1"` and `" 1"`, which would let
    // `"2026-+9-01..."` through; require plain digits in every fixed field.
    if !bytes[..19].iter().enumerate().all(|(i, b)| match i {
        4 | 7 => *b == b'-',
        10 => *b == b'T' || *b == b't',
        13 | 16 => *b == b':',
        _ => b.is_ascii_digit(),
    }) {
        return None;
    }

    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;

    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    // Second 60 is a leap second; accepted and treated as the next second,
    // which is what every non-leap-aware clock does anyway.
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.len() - frac.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return None;
        }
        // Sub-second precision is discarded: staleness is measured in
        // hundreds of seconds.
        rest = &frac[digits..];
    }
    if !(rest == "Z" || rest == "z") {
        return None;
    }

    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date, by Howard Hinnant's
/// `days_from_civil`. Exact for every year `i64` can hold; the inputs here are
/// four-digit years.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // Shift the year so that March is month 1 and the leap day is the last day
    // of the year, which is what removes every special case.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400; // [0, 399]
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1; // [0, 365]
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real response, captured from the live feed on 2026-09-01. Design §7
    /// requires the parse tests to run against the actual wire format rather
    /// than a hand-written approximation of it: 67 pairs, `BTC/USD` among
    /// them, rates spanning 1e-5 to 1e5, and every timestamp RFC 3339.
    const FIXTURE: &str = include_str!("../tests/fixtures/price-feed-latest.json");

    /// `BTC/USD` in the fixture: `{"rate":77785.25,"timestamp":"2026-09-01T13:15:33Z"}`.
    const FIXTURE_TIMESTAMP: i64 = 1_788_268_533;
    const FIXTURE_MICRO: u128 = 77_785_250_000;

    #[test]
    fn the_live_fixture_parses() {
        let price = parse_feed(FIXTURE, FIXTURE_TIMESTAMP + 5, 300).expect("fixture parses");

        assert_eq!(price.micro_usd_per_btc, FIXTURE_MICRO);
        assert_eq!(price.timestamp_unix, FIXTURE_TIMESTAMP);
    }

    /// The fixture must keep being a *whole* feed, not just the one pair — a
    /// future pair with an unexpected field must not break the parse.
    #[test]
    fn the_live_fixture_carries_every_pair() {
        let feed: Feed = serde_json::from_str(FIXTURE).expect("fixture parses");

        assert!(feed.prices.len() > 60, "{} pairs", feed.prices.len());
        assert!(feed.prices.contains_key(BTC_USD));
        assert!(feed.prices.contains_key("EUR/USD"));
    }

    #[test]
    fn a_stale_timestamp_is_refused() {
        let error = parse_feed(FIXTURE, FIXTURE_TIMESTAMP + 301, 300).expect_err("301s > 300s");

        assert_eq!(
            error,
            OracleError::Stale {
                age_secs: 301,
                max_age_secs: 300
            }
        );

        // The boundary itself is accepted: "older than" is strict.
        parse_feed(FIXTURE, FIXTURE_TIMESTAMP + 300, 300).expect("exactly at the limit is fresh");
    }

    /// A timestamp far in the future is refused by the same check, or the
    /// staleness limit could be defeated by publishing one.
    #[test]
    fn a_far_future_timestamp_is_refused() {
        let error =
            parse_feed(FIXTURE, FIXTURE_TIMESTAMP - 301, 300).expect_err("301s in the future");

        assert_eq!(
            error,
            OracleError::Stale {
                age_secs: -301,
                max_age_secs: 300
            }
        );
    }

    #[test]
    fn an_absent_btc_pair_is_refused() {
        let body = r#"{"prices":{"EUR/USD":{"rate":1.15,"timestamp":"2026-09-01T13:15:33Z"}}}"#;

        assert_eq!(
            parse_feed(body, FIXTURE_TIMESTAMP, 300),
            Err(OracleError::MissingPair)
        );
    }

    #[test]
    fn malformed_json_is_refused() {
        for body in ["", "not json", "{}", r#"{"prices":[]}"#] {
            assert!(
                matches!(
                    parse_feed(body, FIXTURE_TIMESTAMP, 300),
                    Err(OracleError::Malformed(_))
                ),
                "{body:?} parsed"
            );
        }
    }

    #[test]
    fn a_nonsense_rate_is_refused() {
        for rate in ["0.0", "-1.0", "1e-9"] {
            let body = format!(
                r#"{{"prices":{{"BTC/USD":{{"rate":{rate},"timestamp":"2026-09-01T13:15:33Z"}}}}}}"#
            );

            assert!(
                matches!(
                    parse_feed(&body, FIXTURE_TIMESTAMP, 300),
                    Err(OracleError::BadRate(_))
                ),
                "rate {rate} was accepted"
            );
        }

        let body = r#"{"prices":{"BTC/USD":{"rate":1e30,"timestamp":"2026-09-01T13:15:33Z"}}}"#;
        assert!(matches!(
            parse_feed(body, FIXTURE_TIMESTAMP, 300),
            Err(OracleError::RateOutOfRange(_))
        ));
    }

    #[test]
    fn a_malformed_timestamp_is_refused() {
        for timestamp in [
            "",
            "2026-09-01",
            "2026-09-01 13:15:33Z",
            "2026-13-01T13:15:33Z",
            "2026-02-30T13:15:33Z",
            "2026-09-01T24:15:33Z",
            "2026-09-01T13:60:33Z",
            "2026-09-01T13:15:33+02:00",
            "2026-09-01T13:15:33",
            "2026-+9-01T13:15:33Z",
        ] {
            let body = format!(
                r#"{{"prices":{{"BTC/USD":{{"rate":77785.25,"timestamp":"{timestamp}"}}}}}}"#
            );

            assert!(
                matches!(
                    parse_feed(&body, FIXTURE_TIMESTAMP, 300),
                    Err(OracleError::BadTimestamp(_))
                ),
                "timestamp {timestamp:?} was accepted"
            );
        }
    }

    /// Every value here was produced by `date -u -d <literal> +%s`, not by
    /// this parser.
    #[test]
    fn rfc3339_matches_known_epochs() {
        for (text, expected) in [
            ("1970-01-01T00:00:00Z", 0),
            ("1969-12-31T23:59:59Z", -1),
            ("1999-12-31T23:59:59Z", 946_684_799),
            ("2000-02-29T12:00:00Z", 951_825_600),
            ("2024-02-29T00:00:00Z", 1_709_164_800),
            ("2026-09-01T13:15:33Z", 1_788_268_533),
            ("2100-03-01T00:00:00Z", 4_107_542_400),
        ] {
            assert_eq!(parse_rfc3339_utc(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn rfc3339_accepts_the_shapes_the_feed_could_publish() {
        let base = parse_rfc3339_utc("2026-09-01T13:15:33Z").expect("base parses");

        assert_eq!(parse_rfc3339_utc("2026-09-01t13:15:33z"), Some(base));
        assert_eq!(parse_rfc3339_utc("2026-09-01T13:15:33.000Z"), Some(base));
        assert_eq!(parse_rfc3339_utc("2026-09-01T13:15:33.123456Z"), Some(base));
        // A leap second lands on the following second rather than failing.
        assert_eq!(
            parse_rfc3339_utc("2016-12-31T23:59:60Z"),
            Some(1_483_228_800)
        );
        // A fractional marker with no digits is not a timestamp.
        assert_eq!(parse_rfc3339_utc("2026-09-01T13:15:33.Z"), None);
    }

    #[test]
    fn a_jump_larger_than_the_limit_is_refused() {
        // +10% exactly is accepted, +10.001% is not.
        assert_eq!(check_jump(100_000, 110_000, 10), Ok(()));
        assert_eq!(check_jump(100_000, 90_000, 10), Ok(()));
        assert_eq!(
            check_jump(100_000, 110_001, 10),
            Err(OracleError::Jump {
                moved_pct: 10,
                max_pct: 10
            })
        );
        assert_eq!(
            check_jump(100_000, 50_000, 10),
            Err(OracleError::Jump {
                moved_pct: 50,
                max_pct: 10
            })
        );
    }

    /// Realistic magnitudes: BTC at $77 785.25 moving to $85 000 is +9.3%,
    /// inside the default band; to $90 000 is +15.7%, outside it.
    #[test]
    fn the_jump_check_is_exact_at_feed_scale() {
        assert_eq!(check_jump(FIXTURE_MICRO, 85_000_000_000, 10), Ok(()));
        assert!(check_jump(FIXTURE_MICRO, 90_000_000_000, 10).is_err());
    }

    #[test]
    fn the_rate_conversion_rounds_rather_than_truncates() {
        assert_eq!(micro_usd_from_rate(77_785.25), Ok(77_785_250_000));
        assert_eq!(micro_usd_from_rate(1.0), Ok(1_000_000));
        // 1.0000005 * 1e6 = 1000000.5, which rounds up.
        assert_eq!(micro_usd_from_rate(1.000_000_5), Ok(1_000_001));
        assert!(micro_usd_from_rate(f64::NAN).is_err());
        assert!(micro_usd_from_rate(f64::INFINITY).is_err());
    }
}
