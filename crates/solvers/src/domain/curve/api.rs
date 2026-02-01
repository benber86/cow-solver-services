//! Curve Router API client for fetching optimal routes.

use {
    crate::domain::eth,
    reqwest::Url,
    serde::Deserialize,
    std::{fmt, time::Duration},
};

/// Curve Router API client.
pub struct Client {
    http: reqwest::Client,
    base_url: Url,
}

/// Route returned by the Curve Router API.
#[derive(Debug, Clone)]
pub struct Route {
    /// The route path: [token, pool, token, pool, ...] (11 addresses)
    pub route: [eth::Address; 11],
    /// Swap parameters per hop: [i, j, swap_type, pool_type, n_coins] for each of 5 hops
    pub swap_params: [[u64; 5]; 5],
    /// Pool addresses for zap swaps (swap_type == 3)
    pub pools: [eth::Address; 5],
    /// Expected output amount
    pub expected_output: eth::U256,
}

/// API response structure.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiResponse {
    data: RouteData,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteData {
    /// Route addresses
    route: Vec<String>,
    /// Swap parameters - nested arrays
    swap_params: Vec<Vec<String>>,
    /// Pool addresses
    pools: Vec<String>,
    /// Expected output amount (as string)
    expected_output: String,
}

impl Client {
    /// Creates a new Curve Router API client.
    pub fn new(base_url: Url) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");

        Self { http, base_url }
    }

    /// Fetches the optimal route for a swap.
    pub async fn get_route(
        &self,
        chain_id: u64,
        token_in: eth::Address,
        token_out: eth::Address,
        amount_in: eth::U256,
    ) -> Result<Route, Error> {
        let url = format!(
            "{}?chainId={}&tokenIn={:?}&tokenOut={:?}&amountIn={}",
            self.base_url, chain_id, token_in, token_out, amount_in
        );

        tracing::debug!(%url, "fetching Curve route");

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

        let api_response: ApiResponse = response
            .json()
            .await
            .map_err(|e| Error::Parse(e.to_string()))?;

        Self::parse_route(api_response.data)
    }

    fn parse_route(data: RouteData) -> Result<Route, Error> {
        // Parse route addresses (pad to 11)
        let mut route = [eth::Address::ZERO; 11];
        for (i, addr_str) in data.route.iter().enumerate() {
            if i >= 11 {
                break;
            }
            route[i] = addr_str
                .parse()
                .map_err(|_| Error::Parse(format!("invalid route address: {}", addr_str)))?;
        }

        // Parse swap params (5x5 array)
        let mut swap_params = [[0u64; 5]; 5];
        for (i, params) in data.swap_params.iter().enumerate() {
            if i >= 5 {
                break;
            }
            for (j, param_str) in params.iter().enumerate() {
                if j >= 5 {
                    break;
                }
                swap_params[i][j] = param_str
                    .parse()
                    .map_err(|_| Error::Parse(format!("invalid swap param: {}", param_str)))?;
            }
        }

        // Parse pool addresses (pad to 5)
        let mut pools = [eth::Address::ZERO; 5];
        for (i, addr_str) in data.pools.iter().enumerate() {
            if i >= 5 {
                break;
            }
            pools[i] = addr_str
                .parse()
                .map_err(|_| Error::Parse(format!("invalid pool address: {}", addr_str)))?;
        }

        // Parse expected output
        let expected_output = data
            .expected_output
            .parse()
            .map_err(|_| Error::Parse(format!("invalid expected output: {}", data.expected_output)))?;

        Ok(Route {
            route,
            swap_params,
            pools,
            expected_output,
        })
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
    fn test_parse_route() {
        let data = RouteData {
            route: vec![
                "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4".to_string(),
                "0xD51a44d3FaE010473ca66a4d2bF7C3F9a0d9D8e4".to_string(),
                "0xdAC17F958D2ee523a2206206994597C13D831ec7".to_string(),
            ],
            swap_params: vec![
                vec!["0".to_string(), "0".to_string(), "6".to_string(), "3".to_string(), "3".to_string()],
            ],
            pools: vec![],
            expected_output: "1000000000000000000".to_string(),
        };

        let route = Client::parse_route(data).unwrap();
        assert_eq!(route.expected_output, eth::U256::from(1000000000000000000u64));
    }
}
