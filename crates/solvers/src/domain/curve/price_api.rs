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
    cache: Mutex<HashMap<eth::Address, CacheEntry>>,
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
    /// 10^18 of the token. This is compatible with `auction::Price`.
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
        timeout: Duration,
    ) -> Result<eth::U256, Error> {
        match self.cached(token) {
            Some(Ok(price)) => return Ok(price),
            Some(Err(())) => return Err(Error::RecentlyUnavailable),
            None => {}
        }

        let result =
            tokio::time::timeout(timeout, self.fetch_eth_price(chain, wrapped_native, token))
                .await
                .unwrap_or_else(|_| {
                    Err(Error::Network(format!(
                        "price lookup timed out after {}ms",
                        timeout.as_millis()
                    )))
                });

        match result {
            Ok(price) => {
                self.insert_price(token, price);
                Ok(price)
            }
            Err(err) => {
                tracing::debug!(?token, %err, "caching unavailable token price");
                self.insert_unavailable(token);
                Err(err)
            }
        }
    }

    async fn fetch_eth_price(
        &self,
        chain: &str,
        wrapped_native: eth::Address,
        token: eth::Address,
    ) -> Result<eth::U256, Error> {
        // Fetch both token and wrapped-native USD prices in parallel.
        let (token_usd, native_usd) = tokio::join!(
            self.get_usd_price_raw(chain, token),
            self.get_usd_price_raw(chain, wrapped_native),
        );
        let token_usd = token_usd?;
        let native_usd = native_usd?;

        if native_usd <= 0.0 {
            return Err(Error::Parse("invalid wrapped-native price".to_string()));
        }

        // Convert: native_price = (token_usd / native_usd) * 10^18
        // This gives us wei of the wrapped native needed to buy 10^18 of the token.
        let native_price = (token_usd / native_usd) * 1e18;

        if !native_price.is_finite() || native_price <= 0.0 {
            return Err(Error::Parse(format!(
                "invalid native price calculation: token_usd={}, native_usd={}",
                token_usd, native_usd
            )));
        }

        if native_price >= 2.0_f64.powi(128) {
            return Err(Error::Parse("price overflow".to_string()));
        }

        Ok(eth::U256::from(native_price as u128))
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
    fn cached(&self, token: eth::Address) -> Option<Result<eth::U256, ()>> {
        let cache = self.cache.lock().ok()?;
        match cache.get(&token)? {
            CacheEntry::Price { price, at } if at.elapsed() <= CACHE_TTL => Some(Ok(*price)),
            CacheEntry::Unavailable { at } if at.elapsed() <= NEGATIVE_CACHE_TTL => Some(Err(())),
            _ => None,
        }
    }

    fn insert_price(&self, token: eth::Address, price: eth::U256) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                token,
                CacheEntry::Price {
                    price,
                    at: Instant::now(),
                },
            );
        }
    }

    fn insert_unavailable(&self, token: eth::Address) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(token, CacheEntry::Unavailable { at: Instant::now() });
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
        assert!(client.cached(token).is_none());

        client.insert_price(token, eth::U256::from(1_234u64));
        assert_eq!(client.cached(token), Some(Ok(eth::U256::from(1_234u64))));
    }

    #[test]
    fn failed_lookups_are_remembered() {
        let client = unreachable_client();
        let token = eth::Address::repeat_byte(0x22);

        client.insert_unavailable(token);
        assert_eq!(
            client.cached(token),
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
        client.insert_unavailable(token);

        let started = Instant::now();
        let err = client
            .get_eth_price(
                "base",
                eth::Address::repeat_byte(0x44),
                token,
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
                Duration::from_secs(2),
            )
            .await
            .expect_err("dead port must fail");

        assert_eq!(
            client.cached(token),
            Some(Err(())),
            "the failure must be recorded so the next auction skips the lookup"
        );
    }

    #[test]
    fn test_eth_price_conversion() {
        // Test ETH price calculation: token_usd=3000, weth_usd=2000
        // eth_price = (3000 / 2000) * 10^18 = 1.5 * 10^18
        let token_usd = 3000.0_f64;
        let weth_usd = 2000.0_f64;
        let eth_price = (token_usd / weth_usd) * 1e18;
        let result = eth::U256::from(eth_price as u128);
        assert_eq!(result, eth::U256::from(1_500_000_000_000_000_000u128));
    }
}
