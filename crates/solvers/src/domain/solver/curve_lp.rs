//! Curve LP Token Solver
//!
//! A solver specialized for Curve LP token orders. It handles LP sell orders
//! by routing through the Curve Router API and contract.

use {
    crate::{
        boundary::curve::{interactions, router},
        domain::{
            auction::{self, Auction},
            curve::{api, price_api},
            eth,
            order::{self, Order},
            solution::{self, Solution},
        },
    },
    alloy::{primitives::U256, providers::Provider, rpc::types::TransactionRequest},
    reqwest::Url,
    std::{collections::HashSet, fmt, sync::Arc},
    tracing::Instrument,
};

/// The amount of time we aim the solver to finish before the deadline.
const DEADLINE_SLACK: chrono::Duration = chrono::Duration::milliseconds(500);

/// Curve LP token solver.
pub struct Solver {
    inner: Arc<Inner>,
}

/// Configuration for the Curve LP solver.
pub struct Config {
    /// Chain ID (1 for mainnet).
    pub chain_id: u64,
    /// Whitelisted LP tokens that this solver handles.
    pub lp_tokens: Vec<eth::Address>,
    /// Allowed buy tokens (crvUSD + pool underlyings).
    pub allowed_buy_tokens: Vec<eth::Address>,
    /// Curve Router API URL.
    pub curve_api_url: Url,
    /// Curve Price API URL.
    pub curve_price_api_url: Url,
    /// Node URL for on-chain verification.
    pub node_url: Url,
    /// Slippage buffer in basis points (e.g., 100 = 1%).
    pub slippage_bps: u32,
    /// Maximum deviation between API quote and on-chain get_dy (basis points).
    pub max_quote_deviation_bps: u32,
    /// Gas offset for solution gas estimation.
    pub solution_gas_offset: eth::SignedGas,
    /// The settlement contract address (receiver for swaps).
    pub settlement_contract: eth::Address,
}

struct Inner {
    chain_id: u64,
    lp_tokens: HashSet<eth::Address>,
    allowed_buy_tokens: HashSet<eth::Address>,
    api_client: api::Client,
    price_client: price_api::Client,
    provider: ethrpc::AlloyProvider,
    slippage_bps: u32,
    max_quote_deviation_bps: u32,
    solution_gas_offset: eth::SignedGas,
    settlement_contract: eth::Address,
}

impl Solver {
    /// Creates a new Curve LP solver.
    pub async fn new(config: Config) -> Self {
        let api_client = api::Client::new(config.curve_api_url);
        let price_client = price_api::Client::new(config.curve_price_api_url);
        let web3 = ethrpc::web3(
            Default::default(),
            Default::default(),
            &config.node_url,
            "curve-lp",
        );

        Self {
            inner: Arc::new(Inner {
                chain_id: config.chain_id,
                lp_tokens: config.lp_tokens.into_iter().collect(),
                allowed_buy_tokens: config.allowed_buy_tokens.into_iter().collect(),
                api_client,
                price_client,
                provider: web3.alloy,
                slippage_bps: config.slippage_bps,
                max_quote_deviation_bps: config.max_quote_deviation_bps,
                solution_gas_offset: config.solution_gas_offset,
                settlement_contract: config.settlement_contract,
            }),
        }
    }

    /// Solves the auction, returning solutions for LP token orders.
    pub async fn solve(&self, auction: Auction) -> Vec<Solution> {
        let deadline = auction.deadline.clone();
        let remaining = deadline
            .clone()
            .reduce(DEADLINE_SLACK)
            .remaining()
            .unwrap_or_default();

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

        let inner = self.inner.clone();
        let span = tracing::Span::current();
        let background_work = async move {
            inner.solve(auction, sender).instrument(span).await;
        };

        let handle = tokio::spawn(background_work);

        // Wait for completion or timeout
        match tokio::time::timeout(remaining, handle).await {
            Ok(Ok(())) => {
                // Task completed successfully
            }
            Ok(Err(e)) => {
                tracing::warn!(?e, "solver task panicked");
            }
            Err(_) => {
                tracing::debug!("reached timeout while solving Curve LP orders");
                // Task will be dropped/aborted when handle goes out of scope
            }
        }

        // Now drain the channel - task is done or timed out
        let mut solutions = vec![];
        while let Ok(solution) = receiver.try_recv() {
            solutions.push(solution);
        }

        tracing::info!(num_solutions = solutions.len(), "Curve LP solver completed");
        solutions
    }
}

impl Inner {
    async fn solve(
        &self,
        auction: Auction,
        sender: tokio::sync::mpsc::UnboundedSender<Solution>,
    ) {
        for (i, order) in auction.orders.into_iter().enumerate() {
            // Only handle LP sell orders for whitelisted tokens
            if !self.is_supported_order(&order) {
                continue;
            }

            tracing::debug!(
                order_uid = %order.uid,
                sell_token = ?order.sell.token,
                buy_token = ?order.buy.token,
                "processing Curve LP order"
            );

            match self.solve_order(&order, &auction.tokens, &auction.gas_price).await {
                Ok(solution) => {
                    let solution = solution.with_id(solution::Id(i as u64));
                    if sender.send(solution).is_err() {
                        tracing::debug!("deadline hit, receiver dropped");
                        return;
                    }
                }
                Err(err) => {
                    tracing::warn!(order_uid = %order.uid, ?err, "failed to solve order");
                }
            }
        }
    }

    /// Checks if this order is a supported LP sell order.
    fn is_supported_order(&self, order: &Order) -> bool {
        // Only handle sell orders (user selling LP tokens)
        if order.side != order::Side::Sell {
            return false;
        }

        // Only handle whitelisted LP tokens
        if !self.lp_tokens.contains(&order.sell.token.0) {
            return false;
        }

        // Only allow whitelisted buy tokens
        if !self.allowed_buy_tokens.contains(&order.buy.token.0) {
            return false;
        }

        true
    }

    /// Solves a single LP sell order.
    async fn solve_order(
        &self,
        order: &Order,
        tokens: &auction::Tokens,
        gas_price: &auction::GasPrice,
    ) -> Result<Solution, SolveError> {
        // 1. Query Curve API for optimal route
        let route = self
            .api_client
            .get_route(
                self.chain_id,
                order.sell.token.0,
                order.buy.token.0,
                order.sell.amount,
            )
            .await
            .map_err(SolveError::Api)?;

        tracing::debug!(
            expected_output = %route.expected_output,
            "got route from Curve API"
        );

        // 2. Verify quote on-chain via get_dy
        let onchain_output = self
            .verify_quote_onchain(&route, order.sell.amount)
            .await?;

        // 3. Check deviation between API and on-chain quote
        let deviation_bps = self.calculate_deviation_bps(route.expected_output, onchain_output);
        if deviation_bps > self.max_quote_deviation_bps {
            return Err(SolveError::QuoteDeviation {
                api_output: route.expected_output,
                onchain_output,
                deviation_bps,
            });
        }

        // 4. Apply slippage buffer to on-chain quote (more accurate)
        let min_output = self.apply_slippage(onchain_output);

        // Check if min_output satisfies order's buy amount
        if min_output < order.buy.amount {
            return Err(SolveError::InsufficientOutput {
                min_output,
                required: order.buy.amount,
            });
        }

        // 5. Build solution with custom interaction
        let interaction = interactions::build_exchange_interaction(
            &route,
            order.sell.token,
            order.sell.amount,
            order.buy.token,
            min_output,
            self.settlement_contract,
        );

        // 6. Calculate gas estimate
        // Curve Router swaps typically use 250k-400k gas depending on complexity
        let estimated_gas = eth::Gas(U256::from(350_000)) + self.solution_gas_offset;

        // 7. Calculate fee based on gas
        // Try auction's reference price first, fall back to Curve price API
        let sell_token_price = match tokens.reference_price(&order.sell.token) {
            Some(price) => price,
            None => {
                // Fetch from Curve price API
                let usd_price = self
                    .price_client
                    .get_usd_price("ethereum", order.sell.token.0)
                    .await
                    .map_err(|_| SolveError::NoPriceForSellToken)?;
                auction::Price(eth::Ether(usd_price))
            }
        };

        let fee_in_sell_token = sell_token_price
            .ether_value(eth::Ether(estimated_gas.0.saturating_mul(gas_price.0.0)))
            .ok_or(SolveError::FeeCalculation)?;

        // 8. Build the solution
        let single = solution::Single {
            order: order.clone(),
            input: eth::Asset {
                token: order.sell.token,
                amount: order.sell.amount,
            },
            output: eth::Asset {
                token: order.buy.token,
                amount: min_output,
            },
            interactions: vec![solution::Interaction::Custom(interaction)],
            gas: estimated_gas,
            wrappers: order.wrappers.clone(),
        };

        single
            .into_solution(eth::SellTokenAmount(fee_in_sell_token))
            .ok_or(SolveError::SolutionConstruction)
    }

    /// Verifies the quote on-chain by calling Router.get_dy().
    async fn verify_quote_onchain(
        &self,
        route: &api::Route,
        amount: eth::U256,
    ) -> Result<eth::U256, SolveError> {
        let calldata = router::encode_get_dy(route, amount);

        let tx = TransactionRequest::default()
            .to(router::ROUTER_ADDRESS)
            .input(calldata.into());

        let result = self
            .provider
            .call(tx)
            .await
            .map_err(|e| SolveError::OnchainVerification(e.to_string()))?;

        router::decode_get_dy_result(&result)
            .map_err(|e| SolveError::OnchainVerification(e.to_string()))
    }

    /// Calculates the deviation between two values in basis points.
    fn calculate_deviation_bps(&self, a: eth::U256, b: eth::U256) -> u32 {
        if a.is_zero() || b.is_zero() {
            return u32::MAX;
        }
        let (larger, smaller) = if a > b { (a, b) } else { (b, a) };
        let diff = larger.saturating_sub(smaller);
        let bps = diff.saturating_mul(U256::from(10_000)) / smaller;
        bps.try_into().unwrap_or(u32::MAX)
    }

    /// Applies slippage buffer to the output amount.
    fn apply_slippage(&self, amount: eth::U256) -> eth::U256 {
        // min_output = amount * (10000 - slippage_bps) / 10000
        let multiplier = U256::from(10_000 - self.slippage_bps);
        amount.saturating_mul(multiplier) / U256::from(10_000)
    }
}

#[derive(Debug)]
pub enum SolveError {
    Api(api::Error),
    OnchainVerification(String),
    QuoteDeviation {
        api_output: eth::U256,
        onchain_output: eth::U256,
        deviation_bps: u32,
    },
    InsufficientOutput {
        min_output: eth::U256,
        required: eth::U256,
    },
    NoPriceForSellToken,
    FeeCalculation,
    SolutionConstruction,
}

impl fmt::Display for SolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SolveError::Api(e) => write!(f, "Curve API error: {}", e),
            SolveError::OnchainVerification(msg) => {
                write!(f, "on-chain verification failed: {}", msg)
            }
            SolveError::QuoteDeviation {
                api_output,
                onchain_output,
                deviation_bps,
            } => write!(
                f,
                "quote deviation too high: API={}, on-chain={}, deviation={}bps",
                api_output, onchain_output, deviation_bps
            ),
            SolveError::InsufficientOutput {
                min_output,
                required,
            } => write!(
                f,
                "insufficient output: min_output={}, required={}",
                min_output, required
            ),
            SolveError::NoPriceForSellToken => write!(f, "no price available for sell token"),
            SolveError::FeeCalculation => write!(f, "fee calculation failed"),
            SolveError::SolutionConstruction => write!(f, "solution construction failed"),
        }
    }
}

impl std::error::Error for SolveError {}

impl From<api::Error> for SolveError {
    fn from(e: api::Error) -> Self {
        SolveError::Api(e)
    }
}
