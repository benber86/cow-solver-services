//! Curve Price API client for fetching LP token USD prices.

use {
    crate::domain::eth,
    reqwest::Url,
    serde::Deserialize,
    std::{
        collections::HashMap,
        fmt,
        sync::Mutex,
        time::{Duration, Instant},
    },
};

/// Curve Price API client.
pub struct Client {
    http: reqwest::Client,
    base_url: Url,
    cache: Mutex<HashMap<(eth::Address, u8), CacheEntry>>,
}

#[derive(Debug, Deserialize)]
struct PriceResponse {
    data: PriceData,
}

#[derive(Debug, Deserialize)]
struct PriceData {
    usd_price: f64,
}

/// A remembered lookup outcome. Failures are recorded too — see
/// `NEGATIVE_CACHE_TTL`.
enum CacheEntry {
    Price { price: eth::U256, at: Instant },
    Unavailable { at: Instant },
}

/// Wrapped-native wei per 10^18 atoms of a token, from the USD price of one
/// whole token and of one whole wrapped native.
///
/// One whole token is `10^decimals` atoms and costs `token_usd / native_usd`
/// native, i.e. that many times 10^18 wei. Scaling to 10^18 atoms multiplies
/// by `10^(18 - decimals)`: nothing for an 18-decimal token, 10^10 for WBTC,
/// 10^12 for USDC.
fn native_price(token_usd: f64, native_usd: f64, decimals: u8) -> Result<eth::U256, Error> {
    if native_usd <= 0.0 {
        return Err(Error::Parse("invalid wrapped-native price".to_string()));
    }
    let per_whole_token = (token_usd / native_usd) * 1e18;

    if !per_whole_token.is_finite() || per_whole_token <= 0.0 {
        return Err(Error::Parse(format!(
            "invalid native price calculation: token_usd={token_usd}, native_usd={native_usd}"
        )));
    }
    if per_whole_token >= 2.0_f64.powi(128) {
        return Err(Error::Parse("price overflow".to_string()));
    }
    let per_whole_token = eth::U256::from(per_whole_token as u128);

    // The decimal shift is exact in integers; in floating point it would
    // smear the low digits of an already 30-digit number.
    let ten = eth::U256::from(10u8);
    let price = if decimals <= 18 {
        per_whole_token.checked_mul(ten.pow(eth::U256::from(18 - decimals)))
    } else {
        ten.checked_pow(eth::U256::from(decimals - 18))
            .map(|scale| per_whole_token / scale)
    };
    match price {
        Some(price) if !price.is_zero() => Ok(price),
        _ => Err(Error::Parse(format!(
            "native price not representable for a {decimals}-decimal token"
        ))),
    }
}

/// How long to keep a cached price before refreshing.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// How long to remember that a token has no usable price.
///
/// The same unpriced token reappears in every auction, so without this we pay
/// the full lookup timeout for it again and again — on a 2s-block chain that is
/// a fresh timeout every couple of seconds, indefinitely, for a token that is
/// simply not priced. Deliberately much shorter than `CACHE_TTL` so a transient
/// outage sidelines a token for seconds rather than minutes.
const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(15);

impl Client {
    /// Creates a new Curve Price API client.
    pub fn new(base_url: Url) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");

        Self {
            http,
            base_url,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Fetches the native-token-denominated price for `token`.
    /// Returns price as U256 representing wrapped-native wei needed to buy
    /// 10^18 atoms of the token. This is compatible with `auction::Price`.
    ///
    /// `decimals` is the token's own: the API prices one whole token, and how
    /// many atoms that is differs by ten orders of magnitude between WBTC and
    /// an 18-decimal token.
    ///
    /// `chain` is the Curve Price API chain slug ("ethereum" | "arbitrum" | "xdai").
    /// `wrapped_native` is the pivot token: WETH on Ethereum/Arbitrum, WXDAI on Gnosis.
    /// The `timeout` is applied *inside* this call rather than by the caller.
    ///
    /// That matters: a caller-side `tokio::time::timeout` drops the future
    /// mid-flight, so the client never learns the lookup failed and caches
    /// nothing — leaving the next auction to repeat the identical timeout. By
    /// owning the deadline we always reach a verdict we can record.
    pub async fn get_eth_price(
        &self,
        chain: &str,
        wrapped_native: eth::Address,
        token: eth::Address,
        decimals: u8,
        timeout: Duration,
    ) -> Result<eth::U256, Error> {
        match self.cached(token, decimals) {
            Some(Ok(price)) => return Ok(price),
            Some(Err(())) => return Err(Error::RecentlyUnavailable),
            None => {}
        }

        let result = tokio::time::timeout(
            timeout,
            self.fetch_eth_price(chain, wrapped_native, token, decimals),
        )
        .await
        .unwrap_or_else(|_| {
            Err(Error::Network(format!(
                "price lookup timed out after {}ms",
                timeout.as_millis()
            )))
        });

        match result {
            Ok(price) => {
                self.insert_price(token, decimals, price);
                Ok(price)
            }
            Err(err) => {
                tracing::debug!(?token, %err, "caching unavailable token price");
                self.insert_unavailable(token, decimals);
                Err(err)
            }
        }
    }

    async fn fetch_eth_price(
        &self,
        chain: &str,
        wrapped_native: eth::Address,
        token: eth::Address,
        decimals: u8,
    ) -> Result<eth::U256, Error> {
        // Fetch both token and wrapped-native USD prices in parallel.
        let (token_usd, native_usd) = tokio::join!(
            self.get_usd_price_raw(chain, token),
            self.get_usd_price_raw(chain, wrapped_native),
        );
        native_price(token_usd?, native_usd?, decimals)
    }

    /// Fetches raw USD price for a token as f64.
    async fn get_usd_price_raw(&self, chain: &str, token: eth::Address) -> Result<f64, Error> {
        let url = format!("{}v1/usd_price/{}/{:?}", self.base_url, chain, token);

        tracing::debug!(%url, "fetching Curve token price");

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                message: body,
            });
        }

        let price_response: PriceResponse = response
            .json()
            .await
            .map_err(|e| Error::Parse(e.to_string()))?;

        let usd_price = price_response.data.usd_price;
        if !usd_price.is_finite() || usd_price <= 0.0 {
            return Err(Error::Parse(format!("invalid price: {}", usd_price)));
        }

        Ok(usd_price)
    }

    /// `Some(Ok(price))` = fresh price, `Some(Err(()))` = known-unavailable and
    /// not worth another request yet, `None` = nothing usable, go fetch.
    fn cached(&self, token: eth::Address, decimals: u8) -> Option<Result<eth::U256, ()>> {
        let cache = self.cache.lock().ok()?;
        match cache.get(&(token, decimals))? {
            CacheEntry::Price { price, at } if at.elapsed() <= CACHE_TTL => Some(Ok(*price)),
            CacheEntry::Unavailable { at } if at.elapsed() <= NEGATIVE_CACHE_TTL => Some(Err(())),
            _ => None,
        }
    }

    fn insert_price(&self, token: eth::Address, decimals: u8, price: eth::U256) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                (token, decimals),
                CacheEntry::Price {
                    price,
                    at: Instant::now(),
                },
            );
        }
    }

    fn insert_unavailable(&self, token: eth::Address, decimals: u8) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert((token, decimals), CacheEntry::Unavailable { at: Instant::now() });
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Network(String),
    Api {
        status: u16,
        message: String,
    },
    Parse(String),
    /// A recent lookup for this token failed and the negative cache entry has
    /// not expired, so no request was made.
    RecentlyUnavailable,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Network(msg) => write!(f, "network error: {}", msg),
            Error::Api { status, message } => {
                write!(f, "API error (status {}): {}", status, message)
            }
            Error::Parse(msg) => write!(f, "parse error: {}", msg),
            Error::RecentlyUnavailable => {
                write!(f, "price recently unavailable; not retried yet")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn unreachable_client() -> Client {
        // Port 1 refuses immediately, so any test that actually issues a
        // request fails fast rather than hanging.
        Client::new("http://localhost:1/".parse().unwrap())
    }

    #[test]
    fn fresh_price_is_served_from_cache() {
        let client = unreachable_client();
        let token = eth::Address::repeat_byte(0x11);
        assert!(client.cached(token, 18).is_none());

        client.insert_price(token, 18, eth::U256::from(1_234u64));
        assert!(client.cached(token, 8).is_none());
        assert_eq!(client.cached(token, 18), Some(Ok(eth::U256::from(1_234u64))));
    }

    #[test]
    fn failed_lookups_are_remembered() {
        let client = unreachable_client();
        let token = eth::Address::repeat_byte(0x22);

        client.insert_unavailable(token, 18);
        assert_eq!(
            client.cached(token, 18),
            Some(Err(())),
            "an unavailable token must be distinguishable from an unknown one"
        );
    }

    #[tokio::test]
    async fn known_unavailable_token_short_circuits_without_a_request() {
        // The regression this guards: the caller used to wrap the lookup in its
        // own timeout, which dropped the future mid-flight so nothing was ever
        // recorded — and the next auction, seconds later, paid the identical
        // timeout again, forever.
        let client = unreachable_client();
        let token = eth::Address::repeat_byte(0x33);
        client.insert_unavailable(token, 18);

        let started = Instant::now();
        let err = client
            .get_eth_price(
                "base",
                eth::Address::repeat_byte(0x44),
                token,
                18,
                Duration::from_secs(5),
            )
            .await
            .expect_err("must not fetch");

        assert!(matches!(err, Error::RecentlyUnavailable));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "should have returned from cache, not attempted a request"
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_populates_the_negative_cache() {
        let client = unreachable_client();
        let token = eth::Address::repeat_byte(0x55);

        // First call actually tries (and fails against a dead port).
        let _ = client
            .get_eth_price(
                "base",
                eth::Address::repeat_byte(0x66),
                token,
                18,
                Duration::from_secs(2),
            )
            .await
            .expect_err("dead port must fail");

        assert_eq!(
            client.cached(token, 18),
            Some(Err(())),
            "the failure must be recorded so the next auction skips the lookup"
        );
    }

    #[test]
    fn native_price_of_an_18_decimal_token_is_the_usd_ratio_in_wei() {
        // 3000 USD token against 2000 USD native: 1.5 native per whole token,
        // and a whole token is already 10^18 atoms.
        assert_eq!(
            native_price(3_000.0, 2_000.0, 18).unwrap(),
            eth::U256::from(1_500_000_000_000_000_000u128)
        );
    }

    #[test]
    fn native_price_scales_with_the_tokens_own_decimals() {
        // One WBTC (10^8 atoms) at 60000 USD against 2000 USD native is 30
        // native, so 10^18 atoms cost 30 * 10^10 native.
        assert_eq!(
            native_price(60_000.0, 2_000.0, 8).unwrap(),
            eth::U256::from(30u128 * 10u128.pow(18) * 10u128.pow(10))
        );
        // One USDC (10^6 atoms) at 1 USD is 0.0005 native, so 10^18 atoms cost
        // 5 * 10^14 * 10^12 wei.
        assert_eq!(
            native_price(1.0, 2_000.0, 6).unwrap(),
            eth::U256::from(5u128 * 10u128.pow(14) * 10u128.pow(12))
        );
    }

    #[test]
    fn gas_fee_in_an_8_decimal_token_is_a_few_atoms_not_thousands_of_coins() {
        // The bug this guards: pricing WBTC as if it had 18 decimals made the
        // fee for a small trade come out above 10,000 WBTC.
        let price = native_price(60_000.0, 2_000.0, 8).unwrap();
        // 300k gas at 1 gwei = 3 * 10^14 wei of native
        let gas_cost = eth::U256::from(300_000u128 * 1_000_000_000u128);
        let fee_atoms = gas_cost * eth::U256::from(10u128.pow(18)) / price;
        // 0.0003 native at 30 native per WBTC = 0.00001 WBTC = 1000 atoms
        assert_eq!(fee_atoms, eth::U256::from(1_000u64));
    }

    #[test]
    fn native_price_rejects_unusable_inputs() {
        assert!(native_price(1.0, 0.0, 18).is_err());
        assert!(native_price(1.0, 1.0, u8::MAX).is_err());
        assert!(native_price(f64::NAN, 2_000.0, 18).is_err());
        assert!(native_price(1e30, 1e-30, 0).is_err(), "overflow");
        assert!(native_price(1e-9, 2_000.0, 40).is_err(), "rounds to nothing");
    }

    /// Live check against the Curve price API:
    /// `cargo test -p solvers --lib live_fallback_price -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_fallback_price_gives_a_sane_gas_fee_for_wbtc_and_usdc() {
        let client = Client::new("https://prices.curve.finance/".parse().unwrap());
        let weth = alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let gas_cost = eth::U256::from(300_000u128 * 1_000_000_000u128); // 300k gas at 1 gwei
        for (name, token, decimals) in [
            ("WBTC", alloy::primitives::address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"), 8u8),
            ("USDC", alloy::primitives::address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), 6u8),
        ] {
            let price = client
                .get_eth_price("ethereum", weth, token, decimals, Duration::from_secs(10))
                .await
                .expect("price");
            let fee_atoms = gas_cost * eth::U256::from(10u128.pow(18)) / price;
            let fee_whole = fee_atoms.to::<u128>() as f64 / 10f64.powi(i32::from(decimals));
            println!("{name}: price {price}, fee for 0.0003 ETH of gas = {fee_whole} {name}");
            // 0.0003 ETH is well under a dollar: a few cents of USDC, a few
            // hundred-thousandths of a WBTC
            let fee_usd = if name == "USDC" { fee_whole } else { fee_whole * 60_000.0 };
            assert!((0.05..5.0).contains(&fee_usd), "{name} fee of ~${fee_usd} is not sane");
        }
    }
}
