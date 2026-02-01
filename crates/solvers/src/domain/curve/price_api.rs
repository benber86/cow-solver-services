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

/// WETH address on Ethereum mainnet.
const WETH_ADDRESS: eth::Address =
    alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// Curve Price API client.
pub struct Client {
    http: reqwest::Client,
    base_url: Url,
    cache: Mutex<HashMap<eth::Address, CachedPrice>>,
}

#[derive(Debug, Deserialize)]
struct PriceResponse {
    data: PriceData,
}

#[derive(Debug, Deserialize)]
struct PriceData {
    usd_price: f64,
}

/// Cached ETH-denominated price with fetch timestamp.
struct CachedPrice {
    price: eth::U256,
    fetched_at: Instant,
}

/// How long to keep a cached price before refreshing.
const CACHE_TTL: Duration = Duration::from_secs(60);

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

    /// Fetches the ETH-denominated price for a token.
    /// Returns price as U256 representing wei needed to buy 10^18 of the token.
    /// This is compatible with `auction::Price`.
    pub async fn get_eth_price(
        &self,
        chain: &str,
        token: eth::Address,
    ) -> Result<eth::U256, Error> {
        if let Some(price) = self.cached_price(token) {
            return Ok(price);
        }

        // Fetch both token and WETH USD prices
        let token_usd = self.get_usd_price_raw(chain, token).await?;
        let weth_usd = self.get_usd_price_raw(chain, WETH_ADDRESS).await?;

        if weth_usd <= 0.0 {
            return Err(Error::Parse("invalid WETH price".to_string()));
        }

        // Convert: eth_price = (token_usd / weth_usd) * 10^18
        // This gives us wei needed to buy 10^18 of the token
        let eth_price = (token_usd / weth_usd) * 1e18;

        if !eth_price.is_finite() || eth_price <= 0.0 {
            return Err(Error::Parse(format!(
                "invalid ETH price calculation: token_usd={}, weth_usd={}",
                token_usd, weth_usd
            )));
        }

        if eth_price >= 2.0_f64.powi(128) {
            return Err(Error::Parse("price overflow".to_string()));
        }

        let as_u256 = eth::U256::from(eth_price as u128);
        self.insert_cache(token, as_u256);
        Ok(as_u256)
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

    fn cached_price(&self, token: eth::Address) -> Option<eth::U256> {
        let cache = self.cache.lock().ok()?;
        let entry = cache.get(&token)?;
        if entry.fetched_at.elapsed() <= CACHE_TTL {
            Some(entry.price)
        } else {
            None
        }
    }

    fn insert_cache(&self, token: eth::Address, price: eth::U256) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                token,
                CachedPrice {
                    price,
                    fetched_at: Instant::now(),
                },
            );
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Network(String),
    Api { status: u16, message: String },
    Parse(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Network(msg) => write!(f, "network error: {}", msg),
            Error::Api { status, message } => {
                write!(f, "API error (status {}): {}", status, message)
            }
            Error::Parse(msg) => write!(f, "parse error: {}", msg),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

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
