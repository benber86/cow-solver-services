//! Curve LP Token Solver
//!
//! A solver specialized for Curve LP token orders. It handles LP sell orders
//! by routing through the Curve Router API and contract.

use {
    crate::domain::{
        auction::{self, Auction, Tokens},
        curve::{
            api,
            legacy_provider::LegacyProvider,
            new_router::NewRouterClient,
            price_api,
            route_provider::{self, QuoteRequest, RouteProvider},
        },
        eth,
        order::{self, Order},
        solution::{self, Solution},
    },
    alloy::primitives::U256,
    futures::stream::StreamExt,
    reqwest::Url,
    serde::Deserialize,
    std::{
        collections::{HashMap, HashSet},
        fmt,
        sync::{Arc, Mutex},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
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
/// Sidechain-only: legacy comparison probe budget. Tighter than the route
/// timeout so a slow/down legacy never delays the real solve.
const LEGACY_TELEMETRY_TIMEOUT: Duration = Duration::from_millis(1500);
/// Failed non-LP general orders are skipped briefly by UID so stale/bad limit
/// orders don't consume the same capped slots every auction.
const GENERAL_ORDER_FAILURE_BACKOFF: Duration = Duration::from_secs(5 * 60);
const MAX_GENERAL_ORDER_FAILURE_BACKOFF_ENTRIES: usize = 4096;

// CoW native-price probe detection constants
/// The sentinel sell_amount CoW uses for native price probes (2^144).
const NATIVE_PRICE_SELL_SENTINEL: U256 = U256::from_limbs([0, 0, 65536, 0]);
const MARKETABILITY_BASE_BPS: u64 = 10_000;
const TOKEN_PRICE_BASE: u128 = 1_000_000_000_000_000_000;

/// Curve Price API chain slug used in the URL path.
///
/// Distinct from other chain mappings in the codebase: Coingecko, for example,
/// uses `arbitrum-one` where Curve uses `arbitrum`. Do not share slugs across
/// APIs.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CurvePriceApiChain {
    Ethereum,
    Arbitrum,
    Xdai,
}

impl CurvePriceApiChain {
    pub fn as_slug(self) -> &'static str {
        match self {
            Self::Ethereum => "ethereum",
            Self::Arbitrum => "arbitrum",
            Self::Xdai => "xdai",
        }
    }
}

/// Backend selection per chain.
#[derive(Debug, Clone)]
pub enum RouteProviderKind {
    Legacy,
    NewRouter { url: Url },
}

/// Chain-scoped configuration. All values here are specific to the chain the
/// solver is running against and are validated together in
/// [`ChainConfig::validated`].
#[derive(Debug, Clone)]
pub struct ChainConfig {
    pub chain_id: u64,
    pub router_address: eth::Address,
    pub wrapped_native_token: eth::Address,
    pub price_api_chain: CurvePriceApiChain,
    pub settlement_contract: eth::Address,
}

#[derive(Debug)]
pub enum ChainConfigError {
    UnsupportedChain(u64),
    PriceApiChainMismatch { chain_id: u64, slug: &'static str },
    ZeroRouterAddress,
}

impl fmt::Display for ChainConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedChain(id) => {
                write!(
                    f,
                    "unsupported chain_id {id}; expected one of 1, 100, 42161"
                )
            }
            Self::PriceApiChainMismatch { chain_id, slug } => write!(
                f,
                "price_api_chain {slug} does not match chain_id {chain_id}"
            ),
            Self::ZeroRouterAddress => {
                write!(f, "router_address must be a non-zero 20-byte address")
            }
        }
    }
}

impl std::error::Error for ChainConfigError {}

impl ChainConfig {
    /// Validates internal consistency of the TOML:
    /// - `chain_id` is one we support
    /// - `price_api_chain` matches `chain_id` (it's the dual of chain_id for
    ///   the Curve Price API's URL scheme, so mismatch is always a bug)
    /// - `router_address` is non-zero (cheap typo / empty-string guard)
    ///
    /// Does **not** validate `wrapped_native_token` or `settlement_contract`
    /// against canonical on-chain deployments — the solver trusts its config
    /// for those, so forks / test deployments can override without patching
    /// code.
    pub fn validated(self) -> Result<Self, ChainConfigError> {
        let expected_slug = match self.chain_id {
            1 => CurvePriceApiChain::Ethereum,
            42161 => CurvePriceApiChain::Arbitrum,
            100 => CurvePriceApiChain::Xdai,
            _ => return Err(ChainConfigError::UnsupportedChain(self.chain_id)),
        };

        if self.price_api_chain != expected_slug {
            return Err(ChainConfigError::PriceApiChainMismatch {
                chain_id: self.chain_id,
                slug: self.price_api_chain.as_slug(),
            });
        }

        if self.router_address == eth::Address::default() {
            return Err(ChainConfigError::ZeroRouterAddress);
        }

        Ok(self)
    }
}

/// Curve LP token solver.
pub struct Solver {
    inner: Arc<Inner>,
}

/// Configuration for the Curve LP solver.
pub struct Config {
    pub chain: ChainConfig,
    /// Whitelisted LP tokens that this solver handles.
    /// `None` means accept any sell token.
    pub lp_tokens: Option<Vec<eth::Address>>,
    /// Allowed buy tokens (crvUSD + pool underlyings).
    /// `None` means accept any buy token.
    pub allowed_buy_tokens: Option<Vec<eth::Address>>,
    /// Strict both-sides token allowlist: if set, reject any order whose sell
    /// or buy token is not in this list. Applied independently of
    /// `lp_tokens` / `allowed_buy_tokens`, which are either-side filters —
    /// use this one when you want to confine the solver to a known universe
    /// of tokens regardless of whether an LP is involved.
    pub token_allowlist: Option<Vec<eth::Address>>,
    /// Curve Router API URL.
    pub curve_api_url: Url,
    /// Curve Price API URL.
    pub curve_price_api_url: Url,
    /// Node URL for on-chain verification.
    pub node_url: Url,
    /// Slippage buffer in basis points (e.g., 100 = 1%).
    pub slippage_bps: u32,
    /// Competitive bid haircut in basis points. This controls the amount we
    /// promise to CoW; `slippage_bps` still controls calldata revert safety.
    pub bid_slippage_bps: u32,
    /// Maximum deviation between API quote and on-chain get_dy (basis points).
    pub max_quote_deviation_bps: u32,
    /// Gas offset for solution gas estimation.
    pub solution_gas_offset: eth::SignedGas,
    /// Which backend provides execution quotes.
    pub route_provider: RouteProviderKind,
    /// Maximum non-LP orders to attempt after all LP-priority orders.
    /// `None` means uncapped.
    pub max_general_orders_per_auction: Option<usize>,
    /// Maximum non-LP orders to attempt per (sell token, buy token, side).
    /// `None` means uncapped.
    pub max_general_orders_per_pair: Option<usize>,
    /// Maximum accepted off-market deviation for non-LP general orders, in bps.
    /// `None` disables the pre-router marketability filter.
    pub max_general_order_market_deviation_bps: Option<u32>,
    /// Maximum absolute validity window for non-LP general orders, in seconds.
    /// `None` disables the long-lived general-order filter.
    pub max_general_order_validity_secs: Option<u64>,
    /// Skip non-LP general orders that carry hooks, flashloan hints, or
    /// wrappers. LP-priority orders and quote requests bypass this.
    pub skip_general_orders_with_hooks: bool,
}

struct Inner {
    chain: ChainConfig,
    lp_tokens: Option<HashSet<eth::Address>>,
    allowed_buy_tokens: Option<HashSet<eth::Address>>,
    token_allowlist: Option<HashSet<eth::Address>>,
    provider: Arc<dyn RouteProvider>,
    /// Best-effort legacy probe alongside new-router real solves on
    /// sidechains. `None` on mainnet (legacy is the primary path so the
    /// comparison would be tautological).
    legacy_telemetry: Option<Arc<LegacyProvider>>,
    price_client: price_api::Client,
    slippage_bps: u32,
    bid_slippage_bps: u32,
    solution_gas_offset: eth::SignedGas,
    max_general_orders_per_auction: Option<usize>,
    max_general_orders_per_pair: Option<usize>,
    max_general_order_market_deviation_bps: Option<u32>,
    max_general_order_validity_secs: Option<u64>,
    skip_general_orders_with_hooks: bool,
    general_order_failure_backoff: Mutex<HashMap<order::Uid, Instant>>,
}

#[derive(Clone, Copy)]
struct SelectedCounts {
    total: usize,
    priority: usize,
    general: usize,
    marketability_filtered_general: usize,
    validity_filtered_general: usize,
    hook_filtered_general: usize,
    backoff_filtered_general: usize,
}

impl Solver {
    /// Creates a new Curve LP solver.
    pub async fn new(config: Config) -> Self {
        tracing::info!(
            lp_token_filter_count = config.lp_tokens.as_ref().map_or(0, Vec::len),
            buy_token_filter_count = config.allowed_buy_tokens.as_ref().map_or(0, Vec::len),
            token_allowlist_count = config.token_allowlist.as_ref().map_or(0, Vec::len),
            "initialized Curve LP token filters"
        );

        if config.lp_tokens.is_none()
            && config.allowed_buy_tokens.is_none()
            && config.token_allowlist.is_none()
        {
            tracing::warn!(
                "Curve LP solver is running without token filters; \
                 all sell orders will be attempted and this can cause timeouts"
            );
        }

        let price_client = price_api::Client::new(config.curve_price_api_url);
        let web3 = ethrpc::web3(
            Default::default(),
            Default::default(),
            &config.node_url,
            "curve-lp",
        );

        let legacy = LegacyProvider::new(
            api::Client::new(config.curve_api_url.clone()),
            web3.alloy.clone(),
            config.chain.chain_id,
            config.chain.router_address,
            config.slippage_bps,
            config.max_quote_deviation_bps,
            ONCHAIN_VERIFY_TIMEOUT,
        );

        let (provider, legacy_telemetry): (Arc<dyn RouteProvider>, _) = match config.route_provider
        {
            RouteProviderKind::Legacy => (Arc::new(legacy), None),
            RouteProviderKind::NewRouter { url } => {
                let new_router =
                    NewRouterClient::new(url, config.chain.router_address, config.slippage_bps);
                // Sidechain telemetry: a second legacy client (same chain) that
                // we only ever invoke for log comparison on real solves.
                let tele = LegacyProvider::new(
                    api::Client::new(config.curve_api_url.clone()),
                    web3.alloy.clone(),
                    config.chain.chain_id,
                    config.chain.router_address,
                    config.slippage_bps,
                    config.max_quote_deviation_bps,
                    ONCHAIN_VERIFY_TIMEOUT,
                );
                (Arc::new(new_router), Some(Arc::new(tele)))
            }
        };

        Self {
            inner: Arc::new(Inner {
                chain: config.chain,
                lp_tokens: config.lp_tokens.map(|v| v.into_iter().collect()),
                allowed_buy_tokens: config.allowed_buy_tokens.map(|v| v.into_iter().collect()),
                token_allowlist: config.token_allowlist.map(|v| v.into_iter().collect()),
                provider,
                legacy_telemetry,
                price_client,
                slippage_bps: config.slippage_bps,
                bid_slippage_bps: config.bid_slippage_bps,
                solution_gas_offset: config.solution_gas_offset,
                max_general_orders_per_auction: config.max_general_orders_per_auction,
                max_general_orders_per_pair: config.max_general_orders_per_pair,
                max_general_order_market_deviation_bps: config
                    .max_general_order_market_deviation_bps,
                max_general_order_validity_secs: config.max_general_order_validity_secs,
                skip_general_orders_with_hooks: config.skip_general_orders_with_hooks,
                general_order_failure_backoff: Mutex::new(HashMap::new()),
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
        let priority_orders = auction
            .orders
            .iter()
            .filter(|order| {
                self.inner.rejection_reason(order).is_none() && self.inner.is_priority_order(order)
            })
            .count();
        let auction_id = auction.id;
        let is_quote = matches!(auction.id, auction::Id::Quote);
        let selected_counts =
            self.inner
                .selected_counts(&auction.orders, &auction.tokens, is_quote);

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
            priority_orders,
            selected_orders = selected_counts.total,
            selected_priority_orders = selected_counts.priority,
            selected_general_orders = selected_counts.general,
            marketability_filtered_general_orders = selected_counts.marketability_filtered_general,
            validity_filtered_general_orders = selected_counts.validity_filtered_general,
            hook_filtered_general_orders = selected_counts.hook_filtered_general,
            backoff_filtered_general_orders = selected_counts.backoff_filtered_general,
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
                    priority_orders,
                    selected_orders = selected_counts.total,
                    selected_priority_orders = selected_counts.priority,
                    selected_general_orders = selected_counts.general,
                    marketability_filtered_general_orders =
                        selected_counts.marketability_filtered_general,
                    validity_filtered_general_orders = selected_counts.validity_filtered_general,
                    hook_filtered_general_orders = selected_counts.hook_filtered_general,
                    backoff_filtered_general_orders = selected_counts.backoff_filtered_general,
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
            priority_orders,
            selected_orders = selected_counts.total,
            selected_priority_orders = selected_counts.priority,
            selected_general_orders = selected_counts.general,
            marketability_filtered_general_orders = selected_counts.marketability_filtered_general,
            validity_filtered_general_orders = selected_counts.validity_filtered_general,
            hook_filtered_general_orders = selected_counts.hook_filtered_general,
            backoff_filtered_general_orders = selected_counts.backoff_filtered_general,
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

/// Detects whether an order is a CoW native-price probe.
///
/// The CoW driver generates these Buy-side quote probes to discover native
/// token prices. All four conditions must match. `wrapped_native` is the
/// chain's wrapped native token (WETH on Ethereum/Arbitrum, WXDAI on Gnosis).
fn is_native_price_probe(order: &Order, is_quote: bool, wrapped_native: eth::Address) -> bool {
    is_quote
        && order.side == order::Side::Buy
        && order.sell.amount == NATIVE_PRICE_SELL_SENTINEL
        && order.buy.token.0 == wrapped_native
}

impl Inner {
    async fn solve(&self, auction: Auction, sender: tokio::sync::mpsc::UnboundedSender<Solution>) {
        let is_quote = matches!(auction.id, auction::Id::Quote);
        let mut sent_count: usize = 0;
        let mut receiver_dropped = false;
        let supported: Vec<_> = auction
            .orders
            .into_iter()
            .enumerate()
            .filter(|(_, order)| match self.rejection_reason(order) {
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
            })
            .collect();
        let selected = self.select_orders(supported, &auction.tokens, is_quote);

        let mut stream = futures::stream::iter(selected.into_iter().map(|(i, order)| {
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
                    Ok(solved) => {
                        let legacy_output = solved
                            .legacy
                            .as_ref()
                            .and_then(|l| l.output)
                            .map(|v| v.to_string());
                        let legacy_error = solved.legacy.as_ref().and_then(|l| l.error.clone());
                        let legacy_ms = solved.legacy.as_ref().map(|l| l.elapsed_ms);
                        let delta_bps = solved
                            .legacy
                            .as_ref()
                            .and_then(|l| l.output)
                            .and_then(|v| legacy_delta_bps(solved.output_amount, v));
                        let bid_haircut_bps = match order.side {
                            order::Side::Sell => {
                                output_haircut_bps(solved.expected_output, solved.output_amount)
                            }
                            order::Side::Buy => None,
                        };
                        let quality_slug = solved.quote_quality.map(|q| q.as_slug());
                        tracing::info!(
                            order_uid = %order.uid,
                            sell_token = ?order.sell.token,
                            buy_token = ?order.buy.token,
                            side = ?order.side,
                            sell_amount = %order.sell.amount,
                            order_buy_min = %order.buy.amount,
                            expected_output = %solved.expected_output,
                            calldata_min_out = %solved.calldata_min_out,
                            solution_input = %solved.input_amount,
                            solution_output = %solved.output_amount,
                            effective_sell_amount = %solved.effective_sell_amount,
                            effective_buy_amount = %solved.effective_buy_amount,
                            fee_in_sell_token = %solved.fee_in_sell_token,
                            estimated_gas = %solved.estimated_gas.0,
                            gas_price_wei = %solved.gas_price.0.0,
                            sell_token_reference_price = %solved.sell_token_reference_price.0.0,
                            bid_haircut_bps,
                            route_ms = solved.route_ms,
                            price_fetch_ms = solved.price_fetch_ms,
                            is_quote,
                            new_router_quality = quality_slug,
                            new_router_gas = solved.gas_estimate,
                            legacy_output,
                            legacy_ms,
                            legacy_error,
                            delta_bps,
                            "solved order"
                        );
                        Some((solved.solution.with_id(solution::Id(i as u64)), order))
                    }
                    Err(err) => {
                        self.record_general_order_failure(&order, &err, is_quote);
                        tracing::warn!(order_uid = %order.uid, ?err, "failed to solve order");
                        None
                    }
                }
            }
        }))
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
        // Strict both-sides allowlist: reject if either token is outside the
        // configured universe. LP tokens are part of that universe even when
        // they are only listed in `lp-tokens`, so sidechain configs don't need
        // to duplicate every LP address in both lists.
        if let Some(ref allowlist) = self.token_allowlist {
            let token_allowed = |token| {
                allowlist.contains(&token)
                    || self
                        .lp_tokens
                        .as_ref()
                        .is_some_and(|lp_tokens| lp_tokens.contains(&token))
            };
            if !token_allowed(order.sell.token.0) || !token_allowed(order.buy.token.0) {
                return Some("token_not_allowlisted");
            }
        }

        match order.side {
            order::Side::Sell => {
                if let Some(ref lp_tokens) = self.lp_tokens
                    && self.token_allowlist.is_none()
                {
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
                if let Some(ref lp_tokens) = self.lp_tokens
                    && self.token_allowlist.is_none()
                {
                    let sell_is_lp = lp_tokens.contains(&order.sell.token.0);
                    let buy_is_lp = lp_tokens.contains(&order.buy.token.0);
                    if !sell_is_lp && !buy_is_lp {
                        return Some("no_lp_token_match");
                    }
                }
                if let Some(ref allowed) = self.allowed_buy_tokens {
                    if !allowed.contains(&order.sell.token.0)
                        && !allowed.contains(&order.buy.token.0)
                    {
                        return Some("buy_token_not_allowed");
                    }
                }
                None
            }
        }
    }

    fn is_priority_order(&self, order: &Order) -> bool {
        self.lp_tokens.as_ref().is_some_and(|lp_tokens| {
            lp_tokens.contains(&order.sell.token.0) || lp_tokens.contains(&order.buy.token.0)
        })
    }

    fn general_marketability_rejection_reason(
        &self,
        order: &Order,
        tokens: &Tokens,
    ) -> Option<&'static str> {
        let max_deviation_bps = self.max_general_order_market_deviation_bps?;
        let sell_price = match tokens.reference_price(&order.sell.token) {
            Some(price) => price,
            None => return Some("missing_sell_reference_price"),
        };
        let buy_price = match tokens.reference_price(&order.buy.token) {
            Some(price) => price,
            None => return Some("missing_buy_reference_price"),
        };

        let sell_value = order_value(order.sell.amount, sell_price);
        let buy_value = order_value(order.buy.amount, buy_price);
        if sell_value.is_zero() || buy_value.is_zero() {
            return Some("zero_reference_value");
        }

        let allowed_buy_value = sell_value.saturating_mul(eth::U256::from(
            MARKETABILITY_BASE_BPS + u64::from(max_deviation_bps),
        )) / eth::U256::from(MARKETABILITY_BASE_BPS);
        if buy_value > allowed_buy_value {
            Some("off_market_limit_price")
        } else {
            None
        }
    }

    fn general_validity_rejection_reason(
        &self,
        order: &Order,
        now_epoch_secs: u64,
    ) -> Option<&'static str> {
        let max_validity_secs = self.max_general_order_validity_secs?;
        let max_valid_to = now_epoch_secs.saturating_add(max_validity_secs);
        if u64::from(order.valid_to) > max_valid_to {
            Some("validity_too_long")
        } else {
            None
        }
    }

    fn general_hook_rejection_reason(&self, order: &Order) -> Option<&'static str> {
        if self.skip_general_orders_with_hooks && order.has_hooks() {
            Some("hooks_not_supported_for_general_order")
        } else {
            None
        }
    }

    fn general_order_backoff_active(&self, order: &Order) -> bool {
        let now = Instant::now();
        let mut backoff = self
            .general_order_failure_backoff
            .lock()
            .expect("general order backoff mutex poisoned");
        match backoff.get(&order.uid).copied() {
            Some(expires_at) if expires_at > now => true,
            Some(_) => {
                backoff.remove(&order.uid);
                false
            }
            None => false,
        }
    }

    fn record_general_order_failure(&self, order: &Order, err: &SolveError, is_quote: bool) {
        if is_quote || self.is_priority_order(order) || !should_backoff_general_failure(err) {
            return;
        }

        let now = Instant::now();
        let expires_at = now + GENERAL_ORDER_FAILURE_BACKOFF;
        let mut backoff = self
            .general_order_failure_backoff
            .lock()
            .expect("general order backoff mutex poisoned");
        backoff.insert(order.uid, expires_at);
        if backoff.len() > MAX_GENERAL_ORDER_FAILURE_BACKOFF_ENTRIES {
            backoff.retain(|_, expires_at| *expires_at > now);
        }
        tracing::debug!(
            order_uid = %order.uid,
            sell_token = ?order.sell.token,
            buy_token = ?order.buy.token,
            backoff_secs = GENERAL_ORDER_FAILURE_BACKOFF.as_secs(),
            ?err,
            "backing off failed general order"
        );
    }

    fn select_orders(
        &self,
        supported: Vec<(usize, Order)>,
        tokens: &Tokens,
        is_quote: bool,
    ) -> Vec<(usize, Order)> {
        let mut priority = Vec::new();
        let mut general = Vec::new();
        let mut marketability_filtered_general = 0usize;
        let mut validity_filtered_general = 0usize;
        let mut hook_filtered_general = 0usize;
        let mut backoff_filtered_general = 0usize;
        let now_epoch_secs = current_epoch_secs();

        for item in supported {
            if self.is_priority_order(&item.1) {
                priority.push(item);
            } else if let Some(reason) =
                self.general_marketability_rejection_reason(&item.1, tokens)
            {
                marketability_filtered_general += 1;
                tracing::debug!(
                    order_uid = %item.1.uid,
                    sell_token = ?item.1.sell.token,
                    buy_token = ?item.1.buy.token,
                    reason,
                    "general order skipped by marketability filter"
                );
            } else if let Some(reason) = if is_quote {
                None
            } else {
                self.general_validity_rejection_reason(&item.1, now_epoch_secs)
            } {
                validity_filtered_general += 1;
                tracing::debug!(
                    order_uid = %item.1.uid,
                    sell_token = ?item.1.sell.token,
                    buy_token = ?item.1.buy.token,
                    valid_to = item.1.valid_to,
                    max_general_order_validity_secs = self.max_general_order_validity_secs,
                    reason,
                    "general order skipped by validity filter"
                );
            } else if let Some(reason) = if is_quote {
                None
            } else {
                self.general_hook_rejection_reason(&item.1)
            } {
                hook_filtered_general += 1;
                tracing::debug!(
                    order_uid = %item.1.uid,
                    sell_token = ?item.1.sell.token,
                    buy_token = ?item.1.buy.token,
                    has_pre_interactions = item.1.has_pre_interactions,
                    has_post_interactions = item.1.has_post_interactions,
                    has_flashloan_hint = item.1.flashloan_hint.is_some(),
                    wrappers = item.1.wrappers.len(),
                    reason,
                    "general order skipped by hook filter"
                );
            } else if self.general_order_backoff_active(&item.1) {
                backoff_filtered_general += 1;
                tracing::debug!(
                    order_uid = %item.1.uid,
                    sell_token = ?item.1.sell.token,
                    buy_token = ?item.1.buy.token,
                    "general order skipped by failure backoff"
                );
            } else {
                general.push(item);
            }
        }

        priority.sort_by_key(|(i, order)| (std::cmp::Reverse(order_notional(order, tokens)), *i));
        general.sort_by_key(|(i, order)| (std::cmp::Reverse(order_notional(order, tokens)), *i));

        if let Some(per_pair) = self.max_general_orders_per_pair {
            let mut counts = HashMap::<(eth::Address, eth::Address, u8), usize>::new();
            general.retain(|(_, order)| {
                let key = (
                    order.sell.token.0,
                    order.buy.token.0,
                    order_side_key(order.side),
                );
                let count = counts.entry(key).or_default();
                if *count >= per_pair {
                    false
                } else {
                    *count += 1;
                    true
                }
            });
        }

        if let Some(limit) = self.max_general_orders_per_auction {
            general.truncate(limit);
        }

        tracing::debug!(
            priority_orders = priority.len(),
            selected_general_orders = general.len(),
            marketability_filtered_general_orders = marketability_filtered_general,
            validity_filtered_general_orders = validity_filtered_general,
            hook_filtered_general_orders = hook_filtered_general,
            backoff_filtered_general_orders = backoff_filtered_general,
            max_general_orders_per_auction = self.max_general_orders_per_auction,
            max_general_orders_per_pair = self.max_general_orders_per_pair,
            max_general_order_market_deviation_bps = self.max_general_order_market_deviation_bps,
            max_general_order_validity_secs = self.max_general_order_validity_secs,
            skip_general_orders_with_hooks = self.skip_general_orders_with_hooks,
            "selected Curve LP solver orders"
        );

        priority.extend(general);
        priority
    }

    fn selected_counts(&self, orders: &[Order], tokens: &Tokens, is_quote: bool) -> SelectedCounts {
        let mut priority = 0usize;
        let mut general = Vec::new();
        let mut marketability_filtered_general = 0usize;
        let mut validity_filtered_general = 0usize;
        let mut hook_filtered_general = 0usize;
        let mut backoff_filtered_general = 0usize;
        let now_epoch_secs = current_epoch_secs();

        for (i, order) in orders
            .iter()
            .enumerate()
            .filter(|(_, order)| self.rejection_reason(order).is_none())
        {
            if self.is_priority_order(order) {
                priority += 1;
            } else if self
                .general_marketability_rejection_reason(order, tokens)
                .is_some()
            {
                marketability_filtered_general += 1;
            } else if !is_quote
                && self
                    .general_validity_rejection_reason(order, now_epoch_secs)
                    .is_some()
            {
                validity_filtered_general += 1;
            } else if !is_quote && self.general_hook_rejection_reason(order).is_some() {
                hook_filtered_general += 1;
            } else if self.general_order_backoff_active(order) {
                backoff_filtered_general += 1;
            } else {
                general.push((i, order));
            }
        }

        general.sort_by_key(|(i, order)| (std::cmp::Reverse(order_notional(order, tokens)), *i));

        if let Some(per_pair) = self.max_general_orders_per_pair {
            let mut counts = HashMap::<(eth::Address, eth::Address, u8), usize>::new();
            general.retain(|(_, order)| {
                let key = (
                    order.sell.token.0,
                    order.buy.token.0,
                    order_side_key(order.side),
                );
                let count = counts.entry(key).or_default();
                if *count >= per_pair {
                    false
                } else {
                    *count += 1;
                    true
                }
            });
        }

        if let Some(limit) = self.max_general_orders_per_auction {
            general.truncate(limit);
        }

        SelectedCounts {
            total: priority + general.len(),
            priority,
            general: general.len(),
            marketability_filtered_general,
            validity_filtered_general,
            hook_filtered_general,
            backoff_filtered_general,
        }
    }

    async fn solve_order(
        &self,
        order: &Order,
        tokens: &auction::Tokens,
        gas_price: &auction::GasPrice,
        is_quote: bool,
    ) -> Result<SolvedOrder, SolveError> {
        if is_native_price_probe(order, is_quote, self.chain.wrapped_native_token) {
            return self.solve_native_price_probe(order).await;
        }

        // Sidechain real-solve only: fire the legacy comparison NOW, in
        // parallel with everything else. The handle is polled non-blockingly
        // just before logging — if legacy hasn't finished by then, we drop it
        // rather than delaying the solve.
        let legacy_telemetry = if !is_quote {
            self.spawn_legacy_telemetry(order, gas_price)
        } else {
            None
        };

        let route_start = std::time::Instant::now();
        let request = QuoteRequest {
            sell_token: order.sell.token.0,
            buy_token: order.buy.token.0,
            sell_amount: order.sell.amount,
            is_quote,
            receiver: self.chain.settlement_contract,
            min_out: Some(order.buy.amount),
            gas_price_gwei: gas_price_to_gwei(gas_price),
        };

        let route_fut = async {
            let result =
                tokio::time::timeout(ROUTE_REQUEST_TIMEOUT, self.provider.quote(&request)).await;
            let route_ms = route_start.elapsed().as_millis() as u64;
            (result, route_ms)
        };

        let needs_price = tokens.reference_price(&order.sell.token).is_none();
        let price_start = std::time::Instant::now();
        let price_fetch = async {
            if needs_price {
                let result = tokio::time::timeout(
                    PRICE_FETCH_TIMEOUT,
                    self.price_client.get_eth_price(
                        self.chain.price_api_chain.as_slug(),
                        self.chain.wrapped_native_token,
                        order.sell.token.0,
                    ),
                )
                .await
                .ok()
                .and_then(Result::ok);
                let price_ms = price_start.elapsed().as_millis() as u64;
                (result, price_ms)
            } else {
                (None, 0)
            }
        };

        let ((quote_result, route_ms), (fetched_price, price_fetch_ms)) =
            tokio::join!(route_fut, price_fetch);
        let quote = quote_result
            .map_err(|_| {
                SolveError::Provider(route_provider::Error::Api(api::Error::Network(format!(
                    "route request timed out after {}ms",
                    ROUTE_REQUEST_TIMEOUT.as_millis()
                ))))
            })?
            .map_err(SolveError::Provider)?;

        tracing::debug!(
            expected_output = %quote.expected_output,
            min_out = %quote.min_out,
            route_ms,
            price_fetch_ms,
            is_quote,
            quality = ?quote.quality,
            "provider quote"
        );

        if quote.min_out < order.buy.amount {
            return Err(SolveError::InsufficientOutput {
                min_output: quote.min_out,
                required: order.buy.amount,
            });
        }

        let interaction = solution::CustomInteraction {
            target: quote.router_address,
            value: eth::Ether(eth::U256::ZERO),
            calldata: quote.calldata.clone(),
            internalize: false,
            inputs: vec![eth::Asset {
                token: order.sell.token,
                amount: order.sell.amount,
            }],
            outputs: vec![eth::Asset {
                token: order.buy.token,
                amount: quote.min_out,
            }],
            allowances: vec![solution::Allowance {
                spender: quote.router_address,
                asset: eth::Asset {
                    token: order.sell.token,
                    amount: order.sell.amount,
                },
            }],
        };

        // Prefer the provider's gas estimate when present (new-router).
        // Legacy returns None — fall back to the historical constant.
        let raw_gas = quote
            .gas_estimate
            .map(U256::from)
            .unwrap_or_else(|| U256::from(350_000u64));
        let estimated_gas = eth::Gas(raw_gas) + self.solution_gas_offset;

        // 6. Calculate fee based on gas
        let sell_token_price = match tokens.reference_price(&order.sell.token) {
            Some(price) => price,
            None => {
                let eth_price = match fetched_price {
                    Some(price) => price,
                    None => {
                        tracing::warn!(
                            order_uid = %order.uid,
                            sell_token = ?order.sell.token,
                            buy_token = ?order.buy.token,
                            price_fetch_ms,
                            "missing sell token reference price"
                        );
                        return Err(SolveError::NoPriceForSellToken);
                    }
                };
                auction::Price(eth::Ether(eth_price))
            }
        };

        let gas_cost = eth::Ether(estimated_gas.0.saturating_mul(gas_price.0.0));
        let fee_in_sell_token = match sell_token_price.ether_value(gas_cost) {
            Some(fee) => fee,
            None => {
                tracing::warn!(
                    order_uid = %order.uid,
                    sell_token = ?order.sell.token,
                    buy_token = ?order.buy.token,
                    estimated_gas = %estimated_gas.0,
                    gas_price_wei = %gas_price.0.0,
                    gas_cost_wei = %gas_cost.0,
                    sell_token_reference_price = %sell_token_price.0.0,
                    "fee calculation failed while converting gas cost to sell token"
                );
                return Err(SolveError::FeeCalculation);
            }
        };

        // 8. Build the solution
        // For sell orders: input is the full sell amount, output is the
        // competitive bid. Calldata remains protected by quote.min_out.
        // For buy orders: output is the exact desired buy amount. Input must be
        // sell_amount minus fee, because into_solution() adds the surplus fee back
        // to the sell side (input + fee must not exceed order.sell.amount).
        let (input_amount, output_amount) = match order.side {
            order::Side::Sell => (
                order.sell.amount,
                sell_order_bid_output(
                    quote.expected_output,
                    quote.min_out,
                    order.buy.amount,
                    self.bid_slippage_bps,
                ),
            ),
            order::Side::Buy => (
                match order.sell.amount.checked_sub(fee_in_sell_token) {
                    Some(input) => input,
                    None => {
                        tracing::warn!(
                            order_uid = %order.uid,
                            sell_token = ?order.sell.token,
                            buy_token = ?order.buy.token,
                            sell_amount = %order.sell.amount,
                            buy_amount = %order.buy.amount,
                            fee_in_sell_token = %fee_in_sell_token,
                            estimated_gas = %estimated_gas.0,
                            gas_price_wei = %gas_price.0.0,
                            gas_cost_wei = %gas_cost.0,
                            sell_token_reference_price = %sell_token_price.0.0,
                            "fee calculation failed because fee exceeds buy-order max sell"
                        );
                        return Err(SolveError::FeeCalculation);
                    }
                },
                order.buy.amount,
            ),
        };

        let effective = match effective_trade_amounts(
            order,
            input_amount,
            output_amount,
            fee_in_sell_token,
        ) {
            Some(effective) => effective,
            None => {
                tracing::warn!(
                    order_uid = %order.uid,
                    sell_token = ?order.sell.token,
                    buy_token = ?order.buy.token,
                    side = ?order.side,
                    input_amount = %input_amount,
                    output_amount = %output_amount,
                    fee_in_sell_token = %fee_in_sell_token,
                    "solution construction failed while computing effective amounts"
                );
                return Err(SolveError::SolutionConstruction);
            }
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

        let solution = match single.into_solution(eth::SellTokenAmount(fee_in_sell_token)) {
            Some(solution) => solution,
            None => {
                tracing::warn!(
                    order_uid = %order.uid,
                    sell_token = ?order.sell.token,
                    buy_token = ?order.buy.token,
                    side = ?order.side,
                    sell_amount = %order.sell.amount,
                    buy_amount = %order.buy.amount,
                    input_amount = %input_amount,
                    output_amount = %output_amount,
                    effective_sell_amount = %effective.sell,
                    effective_buy_amount = %effective.buy,
                    effective_executed_amount = %effective.executed,
                    fee_in_sell_token = %fee_in_sell_token,
                    estimated_gas = %estimated_gas.0,
                    gas_price_wei = %gas_price.0.0,
                    gas_cost_wei = %gas_cost.0,
                    sell_token_reference_price = %sell_token_price.0.0,
                    limit_price_satisfied = effective.limit_price_satisfied,
                    "solution construction failed after fee accounting"
                );
                return Err(SolveError::SolutionConstruction);
            }
        };

        // Non-blocking poll: attach only if the spawned probe is already
        // ready. Anything still in-flight is abandoned rather than holding
        // the critical path. The probe ran concurrently with the main quote
        // (spawned at the top of this fn), so a fast legacy is usually done.
        let legacy = match legacy_telemetry {
            Some(mut h) => match tokio::time::timeout(Duration::ZERO, &mut h).await {
                Ok(Ok(t)) => Some(t),
                Ok(Err(_)) => None,
                Err(_) => {
                    h.abort();
                    None
                }
            },
            None => None,
        };

        Ok(SolvedOrder {
            solution,
            expected_output: quote.expected_output,
            calldata_min_out: quote.min_out,
            input_amount,
            output_amount,
            effective_sell_amount: effective.sell,
            effective_buy_amount: effective.buy,
            fee_in_sell_token,
            estimated_gas,
            gas_price: *gas_price,
            sell_token_reference_price: sell_token_price,
            route_ms,
            price_fetch_ms,
            quote_quality: quote.quality,
            gas_estimate: quote.gas_estimate,
            legacy,
        })
    }

    async fn solve_native_price_probe(&self, order: &Order) -> Result<SolvedOrder, SolveError> {
        let route_start = std::time::Instant::now();

        let reverse_req = QuoteRequest {
            sell_token: order.buy.token.0,
            buy_token: order.sell.token.0,
            sell_amount: order.buy.amount,
            is_quote: true,
            receiver: self.chain.settlement_contract,
            min_out: None,
            gas_price_gwei: None,
        };
        let reverse =
            tokio::time::timeout(ROUTE_REQUEST_TIMEOUT, self.provider.quote(&reverse_req))
                .await
                .map_err(|_| {
                    SolveError::Provider(route_provider::Error::Api(api::Error::Network(format!(
                        "reverse route timed out after {}ms",
                        ROUTE_REQUEST_TIMEOUT.as_millis()
                    ))))
                })?
                .map_err(SolveError::Provider)?;

        let reverse_output = reverse.expected_output;
        let padding_bps_attempts = [500u32, 1500u32];
        let mut forward = None;

        for (attempt, &padding_bps) in padding_bps_attempts.iter().enumerate() {
            let estimated_sell = reverse_output.saturating_mul(U256::from(10_000 + padding_bps))
                / U256::from(10_000u32);

            let req = QuoteRequest {
                sell_token: order.sell.token.0,
                buy_token: order.buy.token.0,
                sell_amount: estimated_sell,
                is_quote: true,
                receiver: self.chain.settlement_contract,
                min_out: None,
                gas_price_gwei: None,
            };
            let q = tokio::time::timeout(ROUTE_REQUEST_TIMEOUT, self.provider.quote(&req))
                .await
                .map_err(|_| {
                    SolveError::Provider(route_provider::Error::Api(api::Error::Network(format!(
                        "forward route timed out after {}ms",
                        ROUTE_REQUEST_TIMEOUT.as_millis()
                    ))))
                })?
                .map_err(SolveError::Provider)?;

            tracing::debug!(
                reverse_output = %reverse_output,
                estimated_sell = %estimated_sell,
                forward_output = %q.expected_output,
                attempt,
                padding_bps,
                "native price probe routing"
            );

            if q.expected_output >= order.buy.amount {
                forward = Some((q, estimated_sell));
                break;
            }
        }

        let (fwd, estimated_sell) = forward.ok_or(SolveError::InsufficientOutput {
            min_output: U256::ZERO,
            required: order.buy.amount,
        })?;

        let route_ms = route_start.elapsed().as_millis() as u64;

        // Probes never settle; calldata may be empty (new-router quote mode).
        let interaction = solution::CustomInteraction {
            target: fwd.router_address,
            value: eth::Ether(eth::U256::ZERO),
            calldata: fwd.calldata,
            internalize: false,
            inputs: vec![eth::Asset {
                token: order.sell.token,
                amount: estimated_sell,
            }],
            outputs: vec![eth::Asset {
                token: order.buy.token,
                amount: order.buy.amount,
            }],
            allowances: vec![solution::Allowance {
                spender: fwd.router_address,
                asset: eth::Asset {
                    token: order.sell.token,
                    amount: estimated_sell,
                },
            }],
        };

        let single = solution::Single {
            order: order.clone(),
            input: eth::Asset {
                token: order.sell.token,
                amount: estimated_sell,
            },
            output: eth::Asset {
                token: order.buy.token,
                amount: order.buy.amount,
            },
            interactions: vec![solution::Interaction::Custom(interaction)],
            gas: eth::Gas(
                fwd.gas_estimate
                    .map(U256::from)
                    .unwrap_or_else(|| U256::from(350_000u64)),
            ) + self.solution_gas_offset,
            wrappers: order.wrappers.clone(),
        };

        let effective = effective_trade_amounts(order, estimated_sell, order.buy.amount, U256::ZERO)
            .ok_or(SolveError::SolutionConstruction)?;

        let solution = single
            .into_solution(eth::SellTokenAmount(U256::ZERO))
            .ok_or(SolveError::SolutionConstruction)?;

        Ok(SolvedOrder {
            solution,
            expected_output: order.buy.amount,
            calldata_min_out: U256::ZERO,
            input_amount: estimated_sell,
            output_amount: order.buy.amount,
            effective_sell_amount: effective.sell,
            effective_buy_amount: effective.buy,
            fee_in_sell_token: U256::ZERO,
            estimated_gas: eth::Gas(
                fwd.gas_estimate
                    .map(U256::from)
                    .unwrap_or_else(|| U256::from(350_000u64)),
            ) + self.solution_gas_offset,
            gas_price: auction::GasPrice(eth::Ether(U256::ZERO)),
            sell_token_reference_price: auction::Price(eth::Ether(U256::ZERO)),
            route_ms,
            price_fetch_ms: 0,
            quote_quality: fwd.quality,
            gas_estimate: fwd.gas_estimate,
            legacy: None,
        })
    }

    /// Fires the legacy provider in the background for telemetry purposes.
    /// The returned handle yields the legacy output (None on failure/timeout)
    /// and the elapsed milliseconds — caller awaits both before emitting the
    /// "solved order" log.
    fn spawn_legacy_telemetry(
        &self,
        order: &Order,
        gas_price: &auction::GasPrice,
    ) -> Option<tokio::task::JoinHandle<LegacyTelemetry>> {
        let tele = self.legacy_telemetry.clone()?;
        let req = QuoteRequest {
            sell_token: order.sell.token.0,
            buy_token: order.buy.token.0,
            sell_amount: order.sell.amount,
            is_quote: true, // skip on-chain verify for the probe
            receiver: self.chain.settlement_contract,
            min_out: None,
            gas_price_gwei: gas_price_to_gwei(gas_price),
        };
        Some(tokio::spawn(async move {
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(LEGACY_TELEMETRY_TIMEOUT, tele.quote(&req)).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let (output, error) = match result {
                Ok(Ok(q)) => (Some(q.expected_output), None),
                Ok(Err(e)) => (None, Some(e.to_string())),
                Err(_) => (None, Some("timeout".to_string())),
            };
            LegacyTelemetry {
                output,
                elapsed_ms,
                error,
            }
        }))
    }
}

#[derive(Debug, Default)]
struct LegacyTelemetry {
    output: Option<eth::U256>,
    elapsed_ms: u64,
    error: Option<String>,
}

struct SolvedOrder {
    solution: Solution,
    expected_output: eth::U256,
    calldata_min_out: eth::U256,
    input_amount: eth::U256,
    output_amount: eth::U256,
    effective_sell_amount: eth::U256,
    effective_buy_amount: eth::U256,
    fee_in_sell_token: eth::U256,
    estimated_gas: eth::Gas,
    gas_price: auction::GasPrice,
    sell_token_reference_price: auction::Price,
    route_ms: u64,
    price_fetch_ms: u64,
    quote_quality: Option<route_provider::QuoteQuality>,
    gas_estimate: Option<u64>,
    legacy: Option<LegacyTelemetry>,
}

/// Basis-point delta between new-router and legacy outputs. Positive = new
/// router won, negative = legacy won. Returns None if either is zero.
fn legacy_delta_bps(new_router: eth::U256, legacy: eth::U256) -> Option<i32> {
    if legacy.is_zero() || new_router.is_zero() {
        return None;
    }
    let (diff, sign) = if new_router >= legacy {
        (new_router - legacy, 1i32)
    } else {
        (legacy - new_router, -1i32)
    };
    let bps_u256 = diff.saturating_mul(U256::from(10_000u32)) / legacy;
    let bps_i32: i32 = bps_u256.try_into().ok()?;
    Some(sign * bps_i32)
}

/// Basis-point haircut between the provider's expected output and the amount
/// promised to CoW. Negative means the promised output exceeds expected output
/// because the order floor or calldata floor dominated.
fn output_haircut_bps(expected: eth::U256, output: eth::U256) -> Option<i32> {
    if expected.is_zero() || output.is_zero() {
        return None;
    }
    let (diff, sign) = if expected >= output {
        (expected - output, 1i32)
    } else {
        (output - expected, -1i32)
    };
    let bps_u256 = diff.saturating_mul(U256::from(10_000u32)) / expected;
    let bps_i32: i32 = bps_u256.try_into().ok()?;
    Some(sign * bps_i32)
}

fn order_notional(order: &Order, tokens: &Tokens) -> eth::U256 {
    let (token, amount) = match order.side {
        order::Side::Sell => (&order.sell.token, order.sell.amount),
        order::Side::Buy => (&order.buy.token, order.buy.amount),
    };
    let price = match tokens.reference_price(token) {
        Some(price) => price.0.0,
        None => return eth::U256::ZERO,
    };
    amount.saturating_mul(price) / eth::U256::from(TOKEN_PRICE_BASE)
}

fn order_value(amount: eth::U256, price: auction::Price) -> eth::U256 {
    amount.saturating_mul(price.0.0) / eth::U256::from(TOKEN_PRICE_BASE)
}

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn order_side_key(side: order::Side) -> u8 {
    match side {
        order::Side::Buy => 0,
        order::Side::Sell => 1,
    }
}

fn sell_order_bid_output(
    expected_output: eth::U256,
    calldata_min_out: eth::U256,
    order_floor: eth::U256,
    bid_slippage_bps: u32,
) -> eth::U256 {
    QuoteRequest::min_out_with_slippage(expected_output, bid_slippage_bps)
        .max(calldata_min_out)
        .max(order_floor)
}

struct EffectiveTradeAmounts {
    sell: eth::U256,
    buy: eth::U256,
    executed: eth::U256,
    limit_price_satisfied: bool,
}

fn effective_trade_amounts(
    order: &Order,
    input_amount: eth::U256,
    output_amount: eth::U256,
    fee_in_sell_token: eth::U256,
) -> Option<EffectiveTradeAmounts> {
    let surplus_fee = if order.solver_determines_fee() {
        fee_in_sell_token
    } else {
        U256::ZERO
    };

    let (sell, buy) = match order.side {
        order::Side::Buy => (input_amount.checked_add(surplus_fee)?, output_amount),
        order::Side::Sell => {
            let sell = input_amount
                .checked_add(surplus_fee)?
                .min(order.sell.amount);
            let buy = sell
                .checked_sub(surplus_fee)?
                .checked_mul(output_amount)?
                .checked_div(input_amount)?;
            (sell, buy)
        }
    };

    let limit_price_satisfied =
        order.sell.amount.checked_mul(buy)? >= order.buy.amount.checked_mul(sell)?;
    let executed = match order.side {
        order::Side::Buy => buy,
        order::Side::Sell => sell.checked_sub(surplus_fee)?,
    };

    Some(EffectiveTradeAmounts {
        sell,
        buy,
        executed,
        limit_price_satisfied,
    })
}

fn should_backoff_general_failure(err: &SolveError) -> bool {
    match err {
        SolveError::InsufficientOutput { .. } => true,
        SolveError::Provider(route_provider::Error::CalldataUnavailable(msg)) => {
            is_route_failure_backoff_message(msg)
        }
        SolveError::Provider(route_provider::Error::Api(api::Error::InvalidRoute(_))) => true,
        SolveError::Provider(route_provider::Error::Api(api::Error::Api {
            status,
            message,
        })) => *status == 422 || is_route_failure_backoff_message(message),
        _ => false,
    }
}

fn is_route_failure_backoff_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "http 422",
        "http 503",
        "min_out",
        "no route",
        "no_route",
        "snapshot",
        "sim_validation",
        "simulator validation",
        "repeated",
        "below required",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn gas_price_to_gwei(gas_price: &auction::GasPrice) -> Option<f64> {
    let wei: u128 = gas_price.0.0.try_into().ok()?;
    Some((wei as f64) / 1e9)
}

#[derive(Debug)]
pub enum SolveError {
    Provider(route_provider::Error),
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
            SolveError::Provider(e) => write!(f, "route provider: {}", e),
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

#[cfg(test)]
mod tests {
    use super::*;

    // Slippage / deviation_bps moved into LegacyProvider; tests live there.

    #[test]
    fn legacy_delta_bps_positive_when_new_router_wins() {
        let new_router = U256::from(1_010_000u64);
        let legacy = U256::from(1_000_000u64);
        assert_eq!(legacy_delta_bps(new_router, legacy), Some(100));
    }

    #[test]
    fn legacy_delta_bps_negative_when_legacy_wins() {
        let new_router = U256::from(990_000u64);
        let legacy = U256::from(1_000_000u64);
        assert_eq!(legacy_delta_bps(new_router, legacy), Some(-100));
    }

    #[test]
    fn legacy_delta_bps_returns_none_on_zero_either_side() {
        assert!(legacy_delta_bps(U256::ZERO, U256::from(1u64)).is_none());
        assert!(legacy_delta_bps(U256::from(1u64), U256::ZERO).is_none());
    }

    #[test]
    fn output_haircut_bps_positive_when_bid_below_expected() {
        let expected = U256::from(1_000_000u64);
        let output = U256::from(999_500u64);
        assert_eq!(output_haircut_bps(expected, output), Some(5));
    }

    #[test]
    fn output_haircut_bps_negative_when_floor_dominates() {
        let expected = U256::from(1_000_000u64);
        let output = U256::from(1_001_000u64);
        assert_eq!(output_haircut_bps(expected, output), Some(-10));
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
        assert!(
            sent < 100,
            "task should stop when receiver is dropped, sent {sent}"
        );
    }

    const WETH_MAINNET: eth::Address =
        alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const WXDAI_GNOSIS: eth::Address =
        alloy::primitives::address!("e91D153E0b41518A2Ce8Dd3D7944Fa863463a97d");

    fn probe_order_with_buy_token(buy_token: eth::Address) -> Order {
        Order {
            uid: order::Uid([0u8; 56]),
            sell: eth::Asset {
                token: eth::TokenAddress(alloy::primitives::address!(
                    "ecb0f0d68c19bdaadaebe24f6752a4db34e2c2cb"
                )),
                amount: NATIVE_PRICE_SELL_SENTINEL,
            },
            buy: eth::Asset {
                token: eth::TokenAddress(buy_token),
                amount: U256::from(100_000_000_000_000_000u128),
            },
            side: order::Side::Buy,
            class: order::Class::Market,
            valid_to: u32::MAX,
            partially_fillable: false,
            has_pre_interactions: false,
            has_post_interactions: false,
            flashloan_hint: None,
            wrappers: vec![],
        }
    }

    #[test]
    fn test_native_price_probe_detection_ethereum() {
        let probe_order = probe_order_with_buy_token(WETH_MAINNET);

        assert!(is_native_price_probe(&probe_order, true, WETH_MAINNET));

        let mut o = probe_order.clone();
        o.buy.amount = U256::from(200_000_000_000_000_000u128);
        assert!(is_native_price_probe(&o, true, WETH_MAINNET));

        assert!(!is_native_price_probe(&probe_order, false, WETH_MAINNET));

        let mut o = probe_order.clone();
        o.sell.amount = U256::from(1_000_000u64);
        assert!(!is_native_price_probe(&o, true, WETH_MAINNET));

        let mut o = probe_order.clone();
        o.buy.token = eth::TokenAddress(alloy::primitives::address!(
            "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        ));
        assert!(!is_native_price_probe(&o, true, WETH_MAINNET));

        let mut o = probe_order.clone();
        o.side = order::Side::Sell;
        assert!(!is_native_price_probe(&o, true, WETH_MAINNET));
    }

    #[test]
    fn test_native_price_probe_detection_gnosis() {
        // On Gnosis, the probe buys WXDAI (not WETH).
        let probe_order = probe_order_with_buy_token(WXDAI_GNOSIS);
        assert!(is_native_price_probe(&probe_order, true, WXDAI_GNOSIS));

        // A mainnet probe (buy=WETH) on a Gnosis-configured solver is not a probe.
        let weth_probe = probe_order_with_buy_token(WETH_MAINNET);
        assert!(!is_native_price_probe(&weth_probe, true, WXDAI_GNOSIS));
    }

    fn test_chain_config() -> ChainConfig {
        ChainConfig {
            chain_id: 1,
            router_address: alloy::primitives::address!("45312ea0eFf7E09C83CBE249fa1d7598c4C8cd4e"),
            wrapped_native_token: WETH_MAINNET,
            price_api_chain: CurvePriceApiChain::Ethereum,
            settlement_contract: alloy::primitives::address!(
                "9008D19f58AAbD9eD0D60971565AA8510560ab41"
            ),
        }
    }

    struct NoopProvider;

    #[async_trait::async_trait]
    impl RouteProvider for NoopProvider {
        async fn quote(
            &self,
            _req: &QuoteRequest,
        ) -> Result<route_provider::ExecutableQuote, route_provider::Error> {
            Err(route_provider::Error::CalldataUnavailable("noop".into()))
        }
    }

    fn test_inner(slippage_bps: u32, _max_quote_deviation_bps: u32) -> Inner {
        Inner {
            chain: test_chain_config(),
            lp_tokens: None,
            allowed_buy_tokens: None,
            token_allowlist: None,
            provider: Arc::new(NoopProvider),
            legacy_telemetry: None,
            price_client: price_api::Client::new("http://localhost:1".parse().unwrap()),
            slippage_bps,
            bid_slippage_bps: slippage_bps,
            solution_gas_offset: eth::SignedGas::default(),
            max_general_orders_per_auction: None,
            max_general_orders_per_pair: None,
            max_general_order_market_deviation_bps: None,
            max_general_order_validity_secs: None,
            skip_general_orders_with_hooks: false,
            general_order_failure_backoff: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn chain_config_validates_mainnet() {
        test_chain_config().validated().expect("mainnet valid");
    }

    #[test]
    fn chain_config_validates_arbitrum() {
        ChainConfig {
            chain_id: 42161,
            router_address: alloy::primitives::address!("2191718CD32d02B8E60BAdFFeA33E4B5DD9A0A0D"),
            wrapped_native_token: alloy::primitives::address!(
                "82aF49447D8a07e3bd95BD0d56f35241523fBab1"
            ),
            price_api_chain: CurvePriceApiChain::Arbitrum,
            settlement_contract: alloy::primitives::address!(
                "9008D19f58AAbD9eD0D60971565AA8510560ab41"
            ),
        }
        .validated()
        .expect("arbitrum valid");
    }

    #[test]
    fn chain_config_validates_gnosis() {
        ChainConfig {
            chain_id: 100,
            router_address: alloy::primitives::address!("0DCDED3545D565bA3B19E683431381007245d983"),
            wrapped_native_token: WXDAI_GNOSIS,
            price_api_chain: CurvePriceApiChain::Xdai,
            settlement_contract: alloy::primitives::address!(
                "9008D19f58AAbD9eD0D60971565AA8510560ab41"
            ),
        }
        .validated()
        .expect("gnosis valid");
    }

    #[test]
    fn chain_config_rejects_unknown_chain() {
        let bad = ChainConfig {
            chain_id: 137, // polygon — not supported by this solver yet
            ..test_chain_config()
        };
        assert!(matches!(
            bad.validated(),
            Err(ChainConfigError::UnsupportedChain(137))
        ));
    }

    #[test]
    fn chain_config_rejects_zero_router_address() {
        let bad = ChainConfig {
            router_address: eth::Address::default(),
            ..test_chain_config()
        };
        assert!(matches!(
            bad.validated(),
            Err(ChainConfigError::ZeroRouterAddress)
        ));
    }

    #[test]
    fn chain_config_allows_noncanonical_settlement_and_wrapped_native() {
        // The validator deliberately does not compare these fields to
        // canonical on-chain addresses — a fork or test deployment must be
        // able to override without patching code. Only chain consistency and
        // the non-zero router shape are enforced.
        let fork = ChainConfig {
            wrapped_native_token: eth::Address::repeat_byte(0xaa),
            settlement_contract: eth::Address::repeat_byte(0xbb),
            ..test_chain_config()
        };
        fork.validated()
            .expect("fork-style override should validate");
    }

    #[test]
    fn chain_config_rejects_slug_chain_mismatch() {
        let bad = ChainConfig {
            price_api_chain: CurvePriceApiChain::Arbitrum,
            ..test_chain_config()
        };
        assert!(matches!(
            bad.validated(),
            Err(ChainConfigError::PriceApiChainMismatch { .. })
        ));
    }

    #[test]
    fn curve_price_api_chain_parses_valid_slugs() {
        #[derive(Deserialize)]
        struct Wrap {
            chain: CurvePriceApiChain,
        }
        fn parse(s: &str) -> Result<CurvePriceApiChain, toml::de::Error> {
            toml::from_str::<Wrap>(&format!(r#"chain = "{s}""#)).map(|w| w.chain)
        }
        assert_eq!(parse("ethereum").unwrap(), CurvePriceApiChain::Ethereum);
        assert_eq!(parse("arbitrum").unwrap(), CurvePriceApiChain::Arbitrum);
        assert_eq!(parse("xdai").unwrap(), CurvePriceApiChain::Xdai);
    }

    #[test]
    fn curve_price_api_chain_rejects_coingecko_slug() {
        // Coingecko uses "arbitrum-one" but Curve uses "arbitrum". Reject the
        // Coingecko form at load time so it can't sneak into config.
        #[derive(Deserialize)]
        struct Wrap {
            #[allow(dead_code)]
            chain: CurvePriceApiChain,
        }
        fn parse(s: &str) -> Result<(), toml::de::Error> {
            toml::from_str::<Wrap>(&format!(r#"chain = "{s}""#)).map(|_| ())
        }
        assert!(parse("arbitrum-one").is_err());
        assert!(parse("mainnet").is_err());
        assert!(parse("gnosis").is_err());
    }

    #[test]
    fn sell_order_bid_output_uses_tighter_bid_floor_without_lowering_calldata_floor() {
        let expected = U256::from(1_000_000u64);
        let calldata_floor = U256::from(990_000u64);
        let order_floor = U256::from(950_000u64);

        assert_eq!(
            sell_order_bid_output(expected, calldata_floor, order_floor, 20),
            U256::from(998_000u64)
        );
    }

    #[test]
    fn sell_order_bid_output_never_goes_below_calldata_or_order_floor() {
        let expected = U256::from(1_000_000u64);
        let calldata_floor = U256::from(990_000u64);
        let order_floor = U256::from(995_000u64);

        assert_eq!(
            sell_order_bid_output(expected, calldata_floor, order_floor, 200),
            order_floor
        );
    }

    // --- token_allowlist filter tests ---

    fn sell_order(sell: eth::Address, buy: eth::Address) -> Order {
        Order {
            uid: order::Uid([0u8; 56]),
            sell: eth::Asset {
                token: eth::TokenAddress(sell),
                amount: U256::from(1_000_000u128),
            },
            buy: eth::Asset {
                token: eth::TokenAddress(buy),
                amount: U256::from(1u128),
            },
            side: order::Side::Sell,
            class: order::Class::Market,
            valid_to: u32::MAX,
            partially_fillable: false,
            has_pre_interactions: false,
            has_post_interactions: false,
            flashloan_hint: None,
            wrappers: vec![],
        }
    }

    fn sell_order_with_amount(sell: eth::Address, buy: eth::Address, amount: u128) -> Order {
        Order {
            sell: eth::Asset {
                token: eth::TokenAddress(sell),
                amount: U256::from(amount),
            },
            ..sell_order(sell, buy)
        }
    }

    fn sell_order_with_amounts(
        sell: eth::Address,
        buy: eth::Address,
        sell_amount: u128,
        buy_amount: u128,
    ) -> Order {
        Order {
            sell: eth::Asset {
                token: eth::TokenAddress(sell),
                amount: U256::from(sell_amount),
            },
            buy: eth::Asset {
                token: eth::TokenAddress(buy),
                amount: U256::from(buy_amount),
            },
            ..sell_order(sell, buy)
        }
    }

    fn with_uid(mut order: Order, byte: u8) -> Order {
        order.uid = order::Uid([byte; 56]);
        order
    }

    fn with_valid_to(mut order: Order, valid_to: u32) -> Order {
        order.valid_to = valid_to;
        order
    }

    fn with_pre_interactions(mut order: Order) -> Order {
        order.has_pre_interactions = true;
        order
    }

    fn tokens_with_unit_prices(tokens: &[eth::Address]) -> auction::Tokens {
        auction::Tokens(
            tokens
                .iter()
                .copied()
                .map(|token| {
                    (
                        eth::TokenAddress(token),
                        auction::Token {
                            decimals: Some(18),
                            symbol: None,
                            reference_price: Some(auction::Price(eth::Ether(U256::from(
                                1_000_000_000_000_000_000u128,
                            )))),
                            available_balance: U256::ZERO,
                            trusted: true,
                        },
                    )
                })
                .collect(),
        )
    }

    #[tokio::test]
    async fn token_allowlist_accepts_when_both_sides_in_list() {
        let a = eth::Address::repeat_byte(0xaa);
        let b = eth::Address::repeat_byte(0xbb);
        let mut inner = test_inner(100, 50);
        inner.token_allowlist = Some([a, b].into_iter().collect());
        assert_eq!(inner.rejection_reason(&sell_order(a, b)), None);
        assert_eq!(inner.rejection_reason(&sell_order(b, a)), None);
    }

    #[tokio::test]
    async fn token_allowlist_rejects_when_sell_not_in_list() {
        let a = eth::Address::repeat_byte(0xaa);
        let b = eth::Address::repeat_byte(0xbb);
        let shitcoin = eth::Address::repeat_byte(0xcc);
        let mut inner = test_inner(100, 50);
        inner.token_allowlist = Some([a, b].into_iter().collect());
        assert_eq!(
            inner.rejection_reason(&sell_order(shitcoin, a)),
            Some("token_not_allowlisted")
        );
    }

    #[tokio::test]
    async fn token_allowlist_rejects_when_buy_not_in_list() {
        let a = eth::Address::repeat_byte(0xaa);
        let b = eth::Address::repeat_byte(0xbb);
        let shitcoin = eth::Address::repeat_byte(0xcc);
        let mut inner = test_inner(100, 50);
        inner.token_allowlist = Some([a, b].into_iter().collect());
        assert_eq!(
            inner.rejection_reason(&sell_order(a, shitcoin)),
            Some("token_not_allowlisted")
        );
    }

    #[tokio::test]
    async fn token_allowlist_rejects_when_neither_side_in_list() {
        let a = eth::Address::repeat_byte(0xaa);
        let b = eth::Address::repeat_byte(0xbb);
        let shitcoin1 = eth::Address::repeat_byte(0xcc);
        let shitcoin2 = eth::Address::repeat_byte(0xdd);
        let mut inner = test_inner(100, 50);
        inner.token_allowlist = Some([a, b].into_iter().collect());
        assert_eq!(
            inner.rejection_reason(&sell_order(shitcoin1, shitcoin2)),
            Some("token_not_allowlisted")
        );
    }

    #[tokio::test]
    async fn token_allowlist_absent_accepts_anything() {
        // Without the filter, rejection_reason returns None regardless of
        // tokens (both other filters are also None in test_inner).
        let inner = test_inner(100, 50);
        assert_eq!(
            inner.rejection_reason(&sell_order(
                eth::Address::repeat_byte(0xcc),
                eth::Address::repeat_byte(0xdd),
            )),
            None
        );
    }

    #[tokio::test]
    async fn token_allowlist_combines_with_lp_tokens_as_priority() {
        // With a broad allowlist present, lp-tokens no longer rejects non-LP
        // pairs. It extends the safety universe and marks LP-involved orders
        // as higher priority.
        let lp = eth::Address::repeat_byte(0x01);
        let allowlisted = eth::Address::repeat_byte(0x02);
        let elsewhere = eth::Address::repeat_byte(0x03);
        let a2 = eth::Address::repeat_byte(0x04);

        let mut inner = test_inner(100, 50);
        inner.lp_tokens = Some([lp].into_iter().collect());
        inner.token_allowlist = Some([allowlisted, a2].into_iter().collect());

        assert_eq!(inner.rejection_reason(&sell_order(lp, allowlisted)), None);
        assert!(inner.is_priority_order(&sell_order(lp, allowlisted)));

        assert_eq!(
            inner.rejection_reason(&sell_order(lp, elsewhere)),
            Some("token_not_allowlisted")
        );

        assert_eq!(inner.rejection_reason(&sell_order(allowlisted, a2)), None);
        assert!(!inner.is_priority_order(&sell_order(allowlisted, a2)));
    }

    #[tokio::test]
    async fn lp_tokens_without_token_allowlist_remains_hard_filter() {
        let lp = eth::Address::repeat_byte(0x01);
        let a = eth::Address::repeat_byte(0x02);
        let b = eth::Address::repeat_byte(0x03);

        let mut inner = test_inner(100, 50);
        inner.lp_tokens = Some([lp].into_iter().collect());

        assert_eq!(inner.rejection_reason(&sell_order(lp, a)), None);
        assert_eq!(
            inner.rejection_reason(&sell_order(a, b)),
            Some("no_lp_token_match")
        );
    }

    #[test]
    fn select_orders_processes_all_lp_orders_before_capped_general_orders() {
        let lp = eth::Address::repeat_byte(0x01);
        let usdc = eth::Address::repeat_byte(0x02);
        let weth = eth::Address::repeat_byte(0x03);
        let usdt = eth::Address::repeat_byte(0x04);

        let mut inner = test_inner(100, 50);
        inner.lp_tokens = Some([lp].into_iter().collect());
        inner.token_allowlist = Some([usdc, weth, usdt].into_iter().collect());
        inner.max_general_orders_per_auction = Some(2);
        inner.max_general_orders_per_pair = Some(1);

        let orders = vec![
            (0, sell_order_with_amount(usdc, weth, 100)),
            (1, sell_order_with_amount(lp, usdc, 1)),
            (2, sell_order_with_amount(usdc, weth, 200)),
            (3, sell_order_with_amount(usdt, usdc, 50)),
            (4, sell_order_with_amount(weth, usdt, 25)),
            (5, sell_order_with_amount(lp, weth, 2)),
        ];
        let tokens = tokens_with_unit_prices(&[lp, usdc, weth, usdt]);

        let selected = inner.select_orders(orders, &tokens, false);
        let selected_indices: Vec<_> = selected.into_iter().map(|(i, _)| i).collect();

        // Both LP orders are retained and come first, even though their
        // notional is smaller than the general orders. General orders are
        // capped globally and per pair after notional ranking.
        assert_eq!(selected_indices, vec![5, 1, 2, 3]);
    }

    #[test]
    fn select_orders_filters_off_market_general_orders_only() {
        let lp = eth::Address::repeat_byte(0x01);
        let usdc = eth::Address::repeat_byte(0x02);
        let weth = eth::Address::repeat_byte(0x03);

        let mut inner = test_inner(100, 50);
        inner.lp_tokens = Some([lp].into_iter().collect());
        inner.token_allowlist = Some([lp, usdc, weth].into_iter().collect());
        inner.max_general_order_market_deviation_bps = Some(200);

        let orders = vec![
            (0, sell_order_with_amounts(usdc, weth, 100, 103)),
            (1, sell_order_with_amounts(usdc, weth, 100, 101)),
            (2, sell_order_with_amounts(lp, usdc, 1, 1_000_000)),
        ];
        let tokens = tokens_with_unit_prices(&[lp, usdc, weth]);

        let selected = inner.select_orders(orders, &tokens, false);
        let selected_indices: Vec<_> = selected.into_iter().map(|(i, _)| i).collect();

        // General order 0 is >2% off-market and skipped. The LP-priority order
        // bypasses marketability even though its limit price is absurd.
        assert_eq!(selected_indices, vec![2, 1]);
    }

    #[test]
    fn selected_counts_include_marketability_filtered_general_orders() {
        let usdc = eth::Address::repeat_byte(0x02);
        let weth = eth::Address::repeat_byte(0x03);

        let mut inner = test_inner(100, 50);
        inner.max_general_order_market_deviation_bps = Some(200);

        let orders = vec![
            sell_order_with_amounts(usdc, weth, 100, 103),
            sell_order_with_amounts(usdc, weth, 100, 101),
        ];
        let tokens = tokens_with_unit_prices(&[usdc, weth]);

        let counts = inner.selected_counts(&orders, &tokens, false);
        assert_eq!(counts.total, 1);
        assert_eq!(counts.priority, 0);
        assert_eq!(counts.general, 1);
        assert_eq!(counts.marketability_filtered_general, 1);
    }

    #[test]
    fn select_orders_filters_long_lived_general_orders_only() {
        let lp = eth::Address::repeat_byte(0x01);
        let usdc = eth::Address::repeat_byte(0x02);
        let weth = eth::Address::repeat_byte(0x03);
        let now = current_epoch_secs();

        let mut inner = test_inner(100, 50);
        inner.lp_tokens = Some([lp].into_iter().collect());
        inner.token_allowlist = Some([lp, usdc, weth].into_iter().collect());
        inner.max_general_order_validity_secs = Some(86_400);

        let orders = vec![
            (
                0,
                with_valid_to(
                    sell_order(usdc, weth),
                    now.saturating_add(2 * 86_400) as u32,
                ),
            ),
            (
                1,
                with_valid_to(sell_order(usdc, weth), now.saturating_add(3_600) as u32),
            ),
            (2, with_valid_to(sell_order(lp, usdc), u32::MAX)),
        ];
        let tokens = tokens_with_unit_prices(&[lp, usdc, weth]);

        let selected = inner.select_orders(orders, &tokens, false);
        let selected_indices: Vec<_> = selected.into_iter().map(|(i, _)| i).collect();

        assert_eq!(selected_indices, vec![2, 1]);
    }

    #[test]
    fn select_orders_filters_hooked_general_orders_only() {
        let lp = eth::Address::repeat_byte(0x01);
        let usdc = eth::Address::repeat_byte(0x02);
        let weth = eth::Address::repeat_byte(0x03);

        let mut inner = test_inner(100, 50);
        inner.lp_tokens = Some([lp].into_iter().collect());
        inner.token_allowlist = Some([lp, usdc, weth].into_iter().collect());
        inner.skip_general_orders_with_hooks = true;

        let orders = vec![
            (0, with_pre_interactions(sell_order(usdc, weth))),
            (1, sell_order(usdc, weth)),
            (2, with_pre_interactions(sell_order(lp, usdc))),
        ];
        let tokens = tokens_with_unit_prices(&[lp, usdc, weth]);

        let selected = inner.select_orders(orders, &tokens, false);
        let selected_indices: Vec<_> = selected.into_iter().map(|(i, _)| i).collect();

        assert_eq!(selected_indices, vec![2, 1]);
    }

    #[test]
    fn quote_requests_bypass_general_validity_and_hook_filters() {
        let usdc = eth::Address::repeat_byte(0x02);
        let weth = eth::Address::repeat_byte(0x03);

        let mut inner = test_inner(100, 50);
        inner.max_general_order_validity_secs = Some(86_400);
        inner.skip_general_orders_with_hooks = true;

        let orders = vec![(
            0,
            with_pre_interactions(with_valid_to(sell_order(usdc, weth), u32::MAX)),
        )];
        let tokens = tokens_with_unit_prices(&[usdc, weth]);

        let selected = inner.select_orders(orders, &tokens, true);
        let selected_indices: Vec<_> = selected.into_iter().map(|(i, _)| i).collect();

        assert_eq!(selected_indices, vec![0]);
    }

    #[test]
    fn effective_trade_amounts_reduce_sell_order_output_by_fee() {
        let usdc = eth::Address::repeat_byte(0x02);
        let weth = eth::Address::repeat_byte(0x03);
        let mut order = sell_order_with_amounts(usdc, weth, 1_000, 900);
        order.class = order::Class::Limit;

        let effective =
            effective_trade_amounts(&order, U256::from(1_000), U256::from(950), U256::from(10))
                .unwrap();

        assert_eq!(effective.sell, U256::from(1_000));
        assert_eq!(effective.buy, U256::from(940));
        assert_eq!(effective.executed, U256::from(990));
        assert!(effective.limit_price_satisfied);
    }

    #[test]
    fn effective_trade_amounts_buy_order_adds_fee_to_input() {
        let usdc = eth::Address::repeat_byte(0x02);
        let weth = eth::Address::repeat_byte(0x03);
        let mut order = sell_order_with_amounts(usdc, weth, 1_000, 1);
        order.side = order::Side::Buy;
        order.class = order::Class::Limit;

        let effective =
            effective_trade_amounts(&order, U256::from(990), U256::from(1), U256::from(10))
                .unwrap();

        assert_eq!(effective.sell, U256::from(1_000));
        assert_eq!(effective.buy, U256::from(1));
        assert_eq!(effective.executed, U256::from(1));
        assert!(effective.limit_price_satisfied);
    }

    #[test]
    fn select_orders_skips_backed_off_general_orders_only() {
        let lp = eth::Address::repeat_byte(0x01);
        let usdc = eth::Address::repeat_byte(0x02);
        let weth = eth::Address::repeat_byte(0x03);

        let mut inner = test_inner(100, 50);
        inner.lp_tokens = Some([lp].into_iter().collect());
        inner.token_allowlist = Some([lp, usdc, weth].into_iter().collect());

        let bad_general = with_uid(sell_order(usdc, weth), 0xA1);
        let ok_general = with_uid(sell_order(usdc, weth), 0xA2);
        let lp_priority_same_uid = with_uid(sell_order(lp, usdc), 0xA1);
        let err = SolveError::Provider(route_provider::Error::CalldataUnavailable(
            "HTTP 422: min_out_exceeds_route_outputs".into(),
        ));
        inner.record_general_order_failure(&bad_general, &err, false);

        let orders = vec![
            (0, bad_general.clone()),
            (1, ok_general),
            (2, lp_priority_same_uid),
        ];
        let tokens = tokens_with_unit_prices(&[lp, usdc, weth]);

        let selected = inner.select_orders(orders, &tokens, false);
        let selected_indices: Vec<_> = selected.into_iter().map(|(i, _)| i).collect();
        assert_eq!(selected_indices, vec![2, 1]);

        let counts = inner.selected_counts(&[bad_general], &tokens, false);
        assert_eq!(counts.total, 0);
        assert_eq!(counts.backoff_filtered_general, 1);
    }
}
