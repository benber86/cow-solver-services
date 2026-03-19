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
    futures::stream::StreamExt,
    reqwest::Url,
    std::{collections::HashSet, fmt, sync::Arc, time::Duration},
    tracing::Instrument,
};

/// The amount of time we aim the solver to finish before the deadline.
const DEADLINE_SLACK: chrono::Duration = chrono::Duration::milliseconds(500);

/// Maximum number of orders solved concurrently (bounds network fan-out).
const MAX_CONCURRENT_ORDERS: usize = 8;
/// Maximum time spent waiting for the Curve routing API per order.
const ROUTE_REQUEST_TIMEOUT: Duration = Duration::from_millis(2500);
/// Maximum time spent waiting for on-chain quote verification per order.
const ONCHAIN_VERIFY_TIMEOUT: Duration = Duration::from_millis(1500);
/// Maximum time spent waiting for token price fallback per order.
const PRICE_FETCH_TIMEOUT: Duration = Duration::from_millis(1200);

/// Curve LP token solver.
pub struct Solver {
    inner: Arc<Inner>,
}

/// Configuration for the Curve LP solver.
pub struct Config {
    /// Chain ID (1 for mainnet).
    pub chain_id: u64,
    /// Whitelisted LP tokens that this solver handles.
    /// `None` means accept any sell token.
    pub lp_tokens: Option<Vec<eth::Address>>,
    /// Allowed buy tokens (crvUSD + pool underlyings).
    /// `None` means accept any buy token.
    pub allowed_buy_tokens: Option<Vec<eth::Address>>,
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
    lp_tokens: Option<HashSet<eth::Address>>,
    allowed_buy_tokens: Option<HashSet<eth::Address>>,
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
        tracing::info!(
            lp_token_filter_count = config.lp_tokens.as_ref().map_or(0, Vec::len),
            buy_token_filter_count = config.allowed_buy_tokens.as_ref().map_or(0, Vec::len),
            "initialized Curve LP token filters"
        );

        if config.lp_tokens.is_none() && config.allowed_buy_tokens.is_none() {
            tracing::warn!(
                "Curve LP solver is running without token filters; \
                 all sell orders will be attempted and this can cause timeouts"
            );
        }

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
                lp_tokens: config.lp_tokens.map(|v| v.into_iter().collect()),
                allowed_buy_tokens: config.allowed_buy_tokens.map(|v| v.into_iter().collect()),
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
        let start = std::time::Instant::now();
        let deadline = auction.deadline.clone();
        let remaining = deadline
            .clone()
            .reduce(DEADLINE_SLACK)
            .remaining()
            .unwrap_or_default();
        let total_orders = auction.orders.len();
        let supported_orders = auction
            .orders
            .iter()
            .filter(|order| self.inner.rejection_reason(order).is_none())
            .count();
        let auction_id = auction.id;
        let is_quote = matches!(auction.id, auction::Id::Quote);

        // For quote auctions, extract token info before moving auction
        let (quote_sell_token, quote_buy_token, quote_sell_amount) = if is_quote {
            auction
                .orders
                .first()
                .map(|o| (Some(o.sell.token), Some(o.buy.token), Some(o.sell.amount)))
                .unwrap_or((None, None, None))
        } else {
            (None, None, None)
        };

        tracing::info!(
            total_orders,
            supported_orders,
            remaining_ms = remaining.as_millis(),
            "starting Curve LP solver"
        );

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

        let inner = self.inner.clone();
        let span = tracing::Span::current();
        let background_work = async move {
            inner.solve(auction, sender).instrument(span).await;
        };

        let mut handle = tokio::spawn(background_work);

        // Wait for completion or timeout
        let mut timed_out = false;
        match tokio::time::timeout(remaining, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(?e, "solver task panicked"),
            Err(_) => {
                timed_out = true;
                tracing::debug!(
                    total_orders,
                    supported_orders,
                    remaining_ms = remaining.as_millis(),
                    "reached timeout while solving Curve LP orders"
                );
                handle.abort();
            }
        }

        // Now drain the channel - task is done or timed out
        let mut solutions = vec![];
        while let Ok(solution) = receiver.try_recv() {
            solutions.push(solution);
        }

        let elapsed = start.elapsed();
        tracing::info!(
            auction_id = %auction_id,
            is_quote,
            total_orders,
            supported_orders,
            num_solutions = solutions.len(),
            elapsed_ms = elapsed.as_millis() as u64,
            budget_ms = remaining.as_millis() as u64,
            timed_out,
            sell_token = ?quote_sell_token,
            buy_token = ?quote_buy_token,
            sell_amount = ?quote_sell_amount,
            "solve_completed"
        );
        solutions
    }
}

impl Inner {
    async fn solve(
        &self,
        auction: Auction,
        sender: tokio::sync::mpsc::UnboundedSender<Solution>,
    ) {
        let is_quote = matches!(auction.id, auction::Id::Quote);
        let mut sent_count: usize = 0;
        let mut receiver_dropped = false;
        let mut stream = futures::stream::iter(
            auction
                .orders
                .into_iter()
                .enumerate()
                .filter(|(_, order)| {
                    match self.rejection_reason(order) {
                        None => true,
                        Some(reason) if is_quote => {
                            tracing::debug!(
                                order_uid = %order.uid,
                                sell_token = ?order.sell.token,
                                buy_token = ?order.buy.token,
                                reason,
                                "order not supported"
                            );
                            false
                        }
                        Some(_) => false,
                    }
                })
                .map(|(i, order)| {
                    let tokens = &auction.tokens;
                    let gas_price = &auction.gas_price;
                    async move {
                        tracing::debug!(
                            order_uid = %order.uid,
                            sell_token = ?order.sell.token,
                            buy_token = ?order.buy.token,
                            "processing Curve LP order"
                        );

                        match self.solve_order(&order, tokens, gas_price, is_quote).await {
                            Ok((solution, output_amount)) => {
                                tracing::info!(
                                    order_uid = %order.uid,
                                    sell_token = ?order.sell.token,
                                    buy_token = ?order.buy.token,
                                    side = ?order.side,
                                    sell_amount = %order.sell.amount,
                                    order_buy_min = %order.buy.amount,
                                    solution_output = %output_amount,
                                    "solved order"
                                );
                                Some((solution.with_id(solution::Id(i as u64)), order))
                            }
                            Err(err) => {
                                tracing::warn!(order_uid = %order.uid, ?err, "failed to solve order");
                                None
                            }
                        }
                    }
                }),
        )
        .buffer_unordered(MAX_CONCURRENT_ORDERS);

        while let Some(result) = stream.next().await {
            if let Some((solution, order)) = result {
                if sender.send(solution).is_err() {
                    tracing::debug!(
                        order_uid = %order.uid,
                        sell_token = ?order.sell.token,
                        buy_token = ?order.buy.token,
                        is_quote,
                        solutions_sent = sent_count,
                        "deadline hit, receiver dropped"
                    );
                    receiver_dropped = true;
                    break;
                }
                sent_count += 1;
            }
        }

        if receiver_dropped {
            tracing::info!(
                is_quote,
                solutions_sent = sent_count,
                "solve_inner_interrupted"
            );
        }
    }

    /// Returns `None` if the order is supported, or a static reason string if
    /// it should be rejected.
    fn rejection_reason(&self, order: &Order) -> Option<&'static str> {
        match order.side {
            order::Side::Sell => {
                if let Some(ref lp_tokens) = self.lp_tokens {
                    let sell_is_lp = lp_tokens.contains(&order.sell.token.0);
                    let buy_is_lp = lp_tokens.contains(&order.buy.token.0);
                    if !sell_is_lp && !buy_is_lp {
                        return Some("no_lp_token_match");
                    }
                }
                if let Some(ref allowed) = self.allowed_buy_tokens {
                    if !allowed.contains(&order.buy.token.0)
                        && !allowed.contains(&order.sell.token.0)
                    {
                        return Some("buy_token_not_allowed");
                    }
                }
                None
            }
            order::Side::Buy => {
                if let Some(ref lp_tokens) = self.lp_tokens {
                    if !lp_tokens.contains(&order.buy.token.0) {
                        return Some("buy_token_not_in_lp_list");
                    }
                }
                if let Some(ref allowed) = self.allowed_buy_tokens {
                    if !allowed.contains(&order.sell.token.0) {
                        return Some("sell_token_not_allowed");
                    }
                }
                None
            }
        }
    }

    /// Solves a single LP order (sell or buy).
    ///
    /// When `is_quote` is true, skip the expensive on-chain `get_dy`
    /// verification and use the Curve API output directly. Quotes are
    /// not executed on-chain, so the extra safety check is unnecessary
    /// and the ~750ms RPC call causes deadline timeouts.
    async fn solve_order(
        &self,
        order: &Order,
        tokens: &auction::Tokens,
        gas_price: &auction::GasPrice,
        is_quote: bool,
    ) -> Result<(Solution, eth::U256), SolveError> {
        // Get token decimals (default to 18 for LP tokens, 6 for stables)
        let sell_token_decimals = tokens
            .get(&order.sell.token)
            .and_then(|t| t.decimals)
            .unwrap_or(18);
        let buy_token_decimals = tokens
            .get(&order.buy.token)
            .and_then(|t| t.decimals)
            .unwrap_or(18);

        // 1. Get route from Curve API (fail fast if upstream is slow).
        let route = tokio::time::timeout(
            ROUTE_REQUEST_TIMEOUT,
            self.api_client.get_route(
                self.chain_id,
                order.sell.token.0,
                order.buy.token.0,
                order.sell.amount,
                sell_token_decimals,
                buy_token_decimals,
            ),
        )
        .await
        .map_err(|_| {
            SolveError::Api(api::Error::Network(format!(
                "route request timed out after {}ms",
                ROUTE_REQUEST_TIMEOUT.as_millis()
            )))
        })?
        .map_err(SolveError::Api)?;

        tracing::debug!(
            expected_output = %route.expected_output,
            "got route from Curve API"
        );

        // Fast-fail: if even the best-case API quote (allowing max deviation
        // upward) can't fill the order after slippage, skip the expensive
        // on-chain get_dy call.
        let optimistic_output = route.expected_output.saturating_add(
            route
                .expected_output
                .saturating_mul(U256::from(self.max_quote_deviation_bps))
                / U256::from(10_000u32),
        );
        if self.apply_slippage(optimistic_output) < order.buy.amount {
            return Err(SolveError::InsufficientOutput {
                min_output: self.apply_slippage(optimistic_output),
                required: order.buy.amount,
            });
        }

        // 2. Verify on-chain (skipped for quotes to avoid deadline timeouts).
        let needs_price = tokens.reference_price(&order.sell.token).is_none();
        let price_fetch = async {
            if needs_price {
                tokio::time::timeout(
                    PRICE_FETCH_TIMEOUT,
                    self.price_client.get_eth_price("ethereum", order.sell.token.0),
                )
                .await
                .ok()
                .and_then(Result::ok)
            } else {
                None
            }
        };

        let (min_output, fetched_price) = if is_quote {
            // For quotes, trust the Curve API output and skip on-chain
            // verification. Apply slippage to the API estimate directly.
            let fetched_price = price_fetch.await;
            let min_output = self.apply_slippage(route.expected_output);
            tracing::debug!(
                api_output = %route.expected_output,
                min_output = %min_output,
                "skipping on-chain verification for quote"
            );
            (min_output, fetched_price)
        } else {
            let verify = tokio::time::timeout(
                ONCHAIN_VERIFY_TIMEOUT,
                self.verify_quote_onchain(&route, order.sell.amount),
            );

            let (onchain_result, fetched_price) = tokio::join!(verify, price_fetch);
            let onchain_output = onchain_result
                .map_err(|_| {
                    SolveError::OnchainVerification(format!(
                        "verification timed out after {}ms",
                        ONCHAIN_VERIFY_TIMEOUT.as_millis()
                    ))
                })??;

            // Check deviation between API and on-chain quote
            let deviation_bps =
                self.calculate_deviation_bps(route.expected_output, onchain_output);
            if deviation_bps > self.max_quote_deviation_bps {
                return Err(SolveError::QuoteDeviation {
                    api_output: route.expected_output,
                    onchain_output,
                    deviation_bps,
                });
            }

            (self.apply_slippage(onchain_output), fetched_price)
        };

        if min_output < order.buy.amount {
            return Err(SolveError::InsufficientOutput {
                min_output,
                required: order.buy.amount,
            });
        }

        // 4. Build solution with custom interaction
        let interaction = interactions::build_exchange_interaction(
            &route,
            order.sell.token,
            order.sell.amount,
            order.buy.token,
            min_output,
            self.settlement_contract,
        );

        // 5. Calculate gas estimate
        let estimated_gas = eth::Gas(U256::from(350_000)) + self.solution_gas_offset;

        // 6. Calculate fee based on gas
        let sell_token_price = match tokens.reference_price(&order.sell.token) {
            Some(price) => price,
            None => {
                let eth_price = fetched_price.ok_or(SolveError::NoPriceForSellToken)?;
                auction::Price(eth::Ether(eth_price))
            }
        };

        let fee_in_sell_token = sell_token_price
            .ether_value(eth::Ether(estimated_gas.0.saturating_mul(gas_price.0.0)))
            .ok_or(SolveError::FeeCalculation)?;

        // 8. Build the solution
        // For sell orders: input is the full sell amount, output is slippage-adjusted.
        // For buy orders: output is the exact desired buy amount. Input must be
        // sell_amount minus fee, because into_solution() adds the surplus fee back
        // to the sell side (input + fee must not exceed order.sell.amount).
        let (input_amount, output_amount) = match order.side {
            order::Side::Sell => (order.sell.amount, min_output),
            order::Side::Buy => (
                order
                    .sell
                    .amount
                    .checked_sub(fee_in_sell_token)
                    .ok_or(SolveError::FeeCalculation)?,
                order.buy.amount,
            ),
        };

        let single = solution::Single {
            order: order.clone(),
            input: eth::Asset {
                token: order.sell.token,
                amount: input_amount,
            },
            output: eth::Asset {
                token: order.buy.token,
                amount: output_amount,
            },
            interactions: vec![solution::Interaction::Custom(interaction)],
            gas: estimated_gas,
            wrappers: order.wrappers.clone(),
        };

        let solution = single
            .into_solution(eth::SellTokenAmount(fee_in_sell_token))
            .ok_or(SolveError::SolutionConstruction)?;
        Ok((solution, output_amount))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slippage_100bps() {
        let inner = test_inner(100, 500);
        let amount = U256::from(10_000u64);
        // 1% slippage: 10000 * 9900 / 10000 = 9900
        assert_eq!(inner.apply_slippage(amount), U256::from(9_900u64));
    }

    #[tokio::test]
    async fn deviation_bps_symmetric() {
        let inner = test_inner(100, 500);
        // 5% deviation regardless of direction
        assert_eq!(
            inner.calculate_deviation_bps(U256::from(1050u64), U256::from(1000u64)),
            500
        );
        assert_eq!(
            inner.calculate_deviation_bps(U256::from(1000u64), U256::from(1050u64)),
            500
        );
    }

    #[tokio::test]
    async fn deviation_bps_zero_inputs() {
        let inner = test_inner(100, 500);
        assert_eq!(
            inner.calculate_deviation_bps(U256::ZERO, U256::from(1000u64)),
            u32::MAX
        );
        assert_eq!(
            inner.calculate_deviation_bps(U256::from(1000u64), U256::ZERO),
            u32::MAX
        );
    }

    #[tokio::test]
    async fn timeout_returns_partial_results() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u32>();

        let mut handle = tokio::spawn(async move {
            for i in 0..5 {
                sender.send(i).ok();
                if i == 2 {
                    // Simulate a slow order after sending 3 results
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            }
        });

        let timeout = std::time::Duration::from_millis(50);
        match tokio::time::timeout(timeout, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(?e, "solver task panicked"),
            Err(_) => {
                handle.abort();
            }
        }

        let mut results = vec![];
        while let Ok(val) = receiver.try_recv() {
            results.push(val);
        }

        // Should have the 3 results sent before the sleep
        assert_eq!(results, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn streaming_delivers_results_incrementally() {
        use futures::stream::StreamExt;

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u32>();

        let mut handle = tokio::spawn(async move {
            let futs = (0..3u32).map(|i| async move {
                tokio::time::sleep(std::time::Duration::from_millis(10 * (i as u64 + 1))).await;
                i
            });

            let mut stream = futures::stream::iter(futs).buffer_unordered(8);

            while let Some(val) = stream.next().await {
                if sender.send(val).is_err() {
                    return;
                }
            }
        });

        // Wait for completion (generous timeout)
        let timeout = std::time::Duration::from_secs(2);
        match tokio::time::timeout(timeout, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("task panicked: {:?}", e),
            Err(_) => panic!("timed out"),
        }

        let mut results = vec![];
        while let Ok(val) = receiver.try_recv() {
            results.push(val);
        }

        // All 3 results should be present (order may vary due to buffer_unordered)
        results.sort();
        assert_eq!(results, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn receiver_drop_stops_sender() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<u32>();

        let handle = tokio::spawn(async move {
            for i in 0..100 {
                if sender.send(i).is_err() {
                    return i;
                }
                tokio::task::yield_now().await;
            }
            100
        });

        // Drop receiver immediately
        drop(receiver);

        let sent = handle.await.unwrap();
        // Task should have stopped early because receiver was dropped
        assert!(sent < 100, "task should stop when receiver is dropped, sent {sent}");
    }

    /// Helper to build a minimal Inner for testing pure methods.
    /// Uses dummy URLs that will never be called.
    fn test_inner(slippage_bps: u32, max_quote_deviation_bps: u32) -> Inner {
        Inner {
            chain_id: 1,
            lp_tokens: None,
            allowed_buy_tokens: None,
            api_client: api::Client::new("http://localhost:1".parse().unwrap()),
            price_client: price_api::Client::new("http://localhost:1".parse().unwrap()),
            provider: ethrpc::web3(
                Default::default(),
                Default::default(),
                &"http://localhost:1".parse().unwrap(),
                "test",
            )
            .alloy,
            slippage_bps,
            max_quote_deviation_bps,
            solution_gas_offset: eth::SignedGas::default(),
            settlement_contract: eth::Address::default(),
        }
    }
}
