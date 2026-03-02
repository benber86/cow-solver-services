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

/// API response is an array of route options.
type ApiResponse = Vec<RouteOption>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteOption {
    /// Output amount in wei as string array (e.g., ["1000053518"])
    amount_out: Vec<String>,
    /// The route steps
    route: Vec<RouteStep>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteStep {
    token_in: Vec<String>,
    token_out: Vec<String>,
    args: RouteArgs,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteArgs {
    pool_id: String,
    swap_address: String,
    swap_params: Vec<i64>,
    pool_address: String,
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
    ///
    /// Note: The Curve v1 API expects amounts in wei (raw token units).
    pub async fn get_route(
        &self,
        chain_id: u64,
        token_in: eth::Address,
        token_out: eth::Address,
        amount_in: eth::U256,
        _token_in_decimals: u8,
        _token_out_decimals: u8,
    ) -> Result<Route, Error> {
        let url = format!(
            "{}?chainId={}&tokenIn={:?}&tokenOut={:?}&amountIn={}&router=curve",
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

        Self::parse_route(api_response, token_in, token_out)
    }

    /// Validates that a constructed route matches the requested tokens.
    fn validate_route(
        route: &[eth::Address; 11],
        swap_params: &[[u64; 5]; 5],
        token_in: eth::Address,
        token_out: eth::Address,
    ) -> Result<(), Error> {
        // First token in the route must be the requested sell token
        if route[0] != token_in {
            return Err(Error::InvalidRoute(format!(
                "route starts with {:?} but expected sell token {:?}",
                route[0], token_in,
            )));
        }

        // At least one hop must exist (first swap_params row not all zeros)
        if swap_params[0].iter().all(|&p| p == 0) {
            return Err(Error::InvalidRoute(
                "no hops in route (swap_params[0] is all zeros)".to_string(),
            ));
        }

        // The last non-zero address in the route must be the requested buy token
        let last_token = route
            .iter()
            .rev()
            .find(|addr| **addr != eth::Address::ZERO)
            .copied()
            .unwrap_or(eth::Address::ZERO);
        if last_token != token_out {
            return Err(Error::InvalidRoute(format!(
                "route ends with {:?} but expected buy token {:?}",
                last_token, token_out,
            )));
        }

        Ok(())
    }

    fn parse_route(
        response: ApiResponse,
        token_in: eth::Address,
        token_out: eth::Address,
    ) -> Result<Route, Error> {
        // Take the first route option (best route)
        let route_option = response
            .into_iter()
            .next()
            .ok_or_else(|| Error::Parse("empty route response".to_string()))?;

        // Parse the expected output (wei string in array)
        let amount_out_str = route_option
            .amount_out
            .first()
            .ok_or_else(|| Error::Parse("empty amountOut array".to_string()))?;
        let expected_output: eth::U256 = amount_out_str
            .parse()
            .map_err(|_| Error::Parse(format!("invalid amountOut: {}", amount_out_str)))?;

        // Build the route array for the contract call
        // Format: [token_in, pool, token_out, pool, token_out, ...]
        let mut route = [eth::Address::ZERO; 11];
        let mut swap_params = [[0u64; 5]; 5];
        let mut pools = [eth::Address::ZERO; 5];

        for (i, step) in route_option.route.iter().enumerate() {
            if i >= 5 {
                break;
            }

            // Token in for this step
            if let Some(token_in_str) = step.token_in.first() {
                let addr: eth::Address = token_in_str
                    .parse()
                    .map_err(|_| Error::Parse(format!("invalid token_in: {}", token_in_str)))?;
                route[i * 2] = addr;
            }

            // Pool/swap address
            let swap_addr: eth::Address = step
                .args
                .swap_address
                .parse()
                .map_err(|_| Error::Parse(format!("invalid swap_address: {}", step.args.swap_address)))?;
            route[i * 2 + 1] = swap_addr;

            // Token out for this step
            if let Some(token_out_str) = step.token_out.first() {
                let addr: eth::Address = token_out_str
                    .parse()
                    .map_err(|_| Error::Parse(format!("invalid token_out: {}", token_out_str)))?;
                route[i * 2 + 2] = addr;
            }

            // Swap params [i, j, swap_type, pool_type, n_coins]
            for (j, &param) in step.args.swap_params.iter().enumerate() {
                if j < 5 {
                    swap_params[i][j] = param as u64;
                }
            }

            // Pool address (for zap swaps)
            if !step.args.pool_address.is_empty()
                && step.args.pool_address != "0x0000000000000000000000000000000000000000"
            {
                pools[i] = step
                    .args
                    .pool_address
                    .parse()
                    .map_err(|_| Error::Parse(format!("invalid pool_address: {}", step.args.pool_address)))?;
            }
        }

        // Validate the constructed route
        Self::validate_route(&route, &swap_params, token_in, token_out)?;

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
    InvalidRoute(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Network(msg) => write!(f, "network error: {}", msg),
            Error::Api { status, message } => {
                write!(f, "API error (status {}): {}", status, message)
            }
            Error::Parse(msg) => write!(f, "parse error: {}", msg),
            Error::InvalidRoute(msg) => write!(f, "invalid route: {}", msg),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_route() {
        // Simulate the v1 API response format (amounts in wei)
        let response: ApiResponse = vec![RouteOption {
            amount_out: vec!["1769022968".to_string()],
            route: vec![RouteStep {
                token_in: vec!["0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4".to_string()],
                token_out: vec!["0xdAC17F958D2ee523a2206206994597C13D831ec7".to_string()],
                args: RouteArgs {
                    pool_id: "factory-tricrypto-1".to_string(),
                    swap_address: "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4".to_string(),
                    swap_params: vec![0, 0, 6, 30, 3],
                    pool_address: "0x0000000000000000000000000000000000000000".to_string(),
                },
            }],
        }];

        let token_in: eth::Address = "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4".parse().unwrap();
        let token_out: eth::Address = "0xdAC17F958D2ee523a2206206994597C13D831ec7".parse().unwrap();
        let route = Client::parse_route(response, token_in, token_out).unwrap();
        assert_eq!(route.expected_output, eth::U256::from(1_769_022_968u64));
    }

    #[test]
    fn test_validate_route_rejects_wrong_token_in() {
        let token_in: eth::Address = "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4".parse().unwrap();
        let token_out: eth::Address =
            "0xdAC17F958D2ee523a2206206994597C13D831ec7".parse().unwrap();
        let wrong_token: eth::Address =
            "0x0000000000000000000000000000000000000001".parse().unwrap();

        let mut route = [eth::Address::ZERO; 11];
        route[0] = wrong_token; // wrong first token
        route[1] = "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .parse()
            .unwrap(); // pool
        route[2] = token_out;

        let swap_params = [[1, 0, 6, 30, 3], [0; 5], [0; 5], [0; 5], [0; 5]];

        let result = Client::validate_route(&route, &swap_params, token_in, token_out);
        assert!(result.is_err());
        assert!(
            matches!(&result.unwrap_err(), Error::InvalidRoute(msg) if msg.contains("starts with"))
        );
    }

    #[test]
    fn test_validate_route_rejects_wrong_token_out() {
        let token_in: eth::Address = "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4".parse().unwrap();
        let token_out: eth::Address =
            "0xdAC17F958D2ee523a2206206994597C13D831ec7".parse().unwrap();
        let wrong_token: eth::Address =
            "0x0000000000000000000000000000000000000001".parse().unwrap();

        let mut route = [eth::Address::ZERO; 11];
        route[0] = token_in;
        route[1] = "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .parse()
            .unwrap();
        route[2] = wrong_token; // wrong last token

        let swap_params = [[1, 0, 6, 30, 3], [0; 5], [0; 5], [0; 5], [0; 5]];

        let result = Client::validate_route(&route, &swap_params, token_in, token_out);
        assert!(result.is_err());
        assert!(
            matches!(&result.unwrap_err(), Error::InvalidRoute(msg) if msg.contains("ends with"))
        );
    }

    #[test]
    fn test_validate_route_rejects_no_hops() {
        let token_in: eth::Address = "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4".parse().unwrap();
        let token_out: eth::Address =
            "0xdAC17F958D2ee523a2206206994597C13D831ec7".parse().unwrap();

        let mut route = [eth::Address::ZERO; 11];
        route[0] = token_in;
        route[2] = token_out;

        let swap_params = [[0; 5]; 5]; // all zeros = no hops

        let result = Client::validate_route(&route, &swap_params, token_in, token_out);
        assert!(result.is_err());
        assert!(
            matches!(&result.unwrap_err(), Error::InvalidRoute(msg) if msg.contains("no hops"))
        );
    }

    #[test]
    fn test_validate_route_accepts_valid_route() {
        let token_in: eth::Address = "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4".parse().unwrap();
        let token_out: eth::Address =
            "0xdAC17F958D2ee523a2206206994597C13D831ec7".parse().unwrap();

        let mut route = [eth::Address::ZERO; 11];
        route[0] = token_in;
        route[1] = "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .parse()
            .unwrap();
        route[2] = token_out;

        let swap_params = [[1, 0, 6, 30, 3], [0; 5], [0; 5], [0; 5], [0; 5]];

        let result = Client::validate_route(&route, &swap_params, token_in, token_out);
        assert!(result.is_ok());
    }
}
