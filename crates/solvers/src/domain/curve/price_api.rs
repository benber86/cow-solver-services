//! Curve Price API client for fetching LP token USD prices.

use {
    crate::domain::eth,
    reqwest::Url,
    serde::Deserialize,
    std::{fmt, time::Duration},
};

/// Curve Price API client.
pub struct Client {
    http: reqwest::Client,
    base_url: Url,
}

#[derive(Debug, Deserialize)]
struct PriceResponse {
    data: PriceData,
}

#[derive(Debug, Deserialize)]
struct PriceData {
    usd_price: f64,
}

impl Client {
    /// Creates a new Curve Price API client.
    pub fn new(base_url: Url) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");

        Self { http, base_url }
    }

    /// Fetches USD price for a token.
    /// Returns price as U256 with 18 decimals (like ETH price format).
    pub async fn get_usd_price(
        &self,
        chain: &str,
        token: eth::Address,
    ) -> Result<eth::U256, Error> {
        let url = format!(
            "{}v1/usd_price/{}/{:?}",
            self.base_url, chain, token
        );

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

        // Convert f64 USD price to U256 with 18 decimals
        let usd_price = price_response.data.usd_price;
        if !usd_price.is_finite() || usd_price <= 0.0 {
            return Err(Error::Parse(format!("invalid price: {}", usd_price)));
        }

        // Convert to 18 decimal representation
        let price_with_decimals = usd_price * 1e18;
        if price_with_decimals >= 2.0_f64.powi(256) {
            return Err(Error::Parse("price overflow".to_string()));
        }

        Ok(eth::U256::from(price_with_decimals as u128))
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
    fn test_price_conversion() {
        // Test that price conversion works correctly
        let usd_price = 1.5_f64;
        let price_with_decimals = usd_price * 1e18;
        let result = eth::U256::from(price_with_decimals as u128);
        assert_eq!(result, eth::U256::from(1_500_000_000_000_000_000u128));
    }
}
