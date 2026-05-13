//! Abstraction over Curve route providers.
//!
//! The solver consumes routes through [`RouteProvider`] rather than coupling
//! to a specific HTTP shape. Two impls live alongside this trait:
//!
//! - the legacy `www.curve.finance/api/router/v1/routes` client, which returns
//!   nested route arrays that we re-encode locally into router calldata, and
//! - the new `*.router.curve.finance/quote` service, which returns
//!   pre-encoded executable calldata directly.
//!
//! Both produce the same [`ExecutableQuote`] artifact, so `solve_order` does
//! not branch on which backend served the quote.

use {
    crate::domain::{curve::api, eth},
    async_trait::async_trait,
    std::fmt,
};

/// What the solver needs in order to build a `CustomInteraction` and ship a
/// solution.
///
/// Intentionally backend-agnostic: it does not expose route arrays / pool
/// addresses. Callers that need to compare against a baseline (e.g. sidechain
/// legacy telemetry) attach extras out-of-band.
#[derive(Debug, Clone)]
pub struct ExecutableQuote {
    /// Output amount the provider expects in `buy_token` units. Pre-slippage.
    pub expected_output: eth::U256,
    /// Router contract address — the `target` of the resulting interaction
    /// and the `spender` of the sell-token allowance.
    pub router_address: eth::Address,
    /// ABI-encoded calldata to call on `router_address` with the sell token
    /// pre-approved for `sell_amount`.
    pub calldata: Vec<u8>,
    /// Optional gas estimate from the provider, if it computed one.
    pub gas_estimate: Option<u64>,
    /// Optional quality tier from the provider, for logging only.
    pub quality: Option<QuoteQuality>,
}

/// Quote-quality tier surfaced by the new router service. We carry it through
/// untouched for telemetry — the solver does not key behaviour off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteQuality {
    /// Cached bucket-curve estimate. Lowest fidelity.
    Interpolated,
    /// Route re-priced against a fresh on-chain snapshot during optimisation.
    Route,
    /// End-to-end simulator-validated against a snapshot. Highest fidelity.
    RouterExecution,
}

impl QuoteQuality {
    /// Stable lowercase slug for logging / tg-monitor.sh parsing.
    pub fn as_slug(self) -> &'static str {
        match self {
            Self::Interpolated => "interpolated",
            Self::Route => "route",
            Self::RouterExecution => "router_execution",
        }
    }
}

/// Inputs the solver passes when requesting a route. `is_quote` controls
/// whether the provider should optimise for fidelity (real solve) or latency
/// (CoW driver quote probe).
#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub sell_token: eth::Address,
    pub buy_token: eth::Address,
    pub sell_amount: eth::U256,
    /// True when this came from an `auction::Id::Quote` — provider may skip
    /// expensive simulator validation in that case.
    pub is_quote: bool,
    /// Settlement contract that will both supply the sell tokens and receive
    /// the buy tokens on-chain. New-router needs this baked into the
    /// calldata; legacy uses it as the local encode receiver.
    pub receiver: eth::Address,
    /// Pre-slippage min-output floor enforced on-chain. The provider may
    /// also enforce it server-side (new-router rejects with 422 if its best
    /// route can't hit it). `None` lets the provider quote freely; callers
    /// should pass `Some` whenever they want hard slippage protection in the
    /// returned calldata.
    pub min_out: Option<eth::U256>,
    /// Gas price in gwei, derived from `auction::GasPrice`. Used by the new
    /// router for gas-adjusted route selection; ignored by legacy.
    pub gas_price_gwei: Option<f64>,
}

impl QuoteRequest {
    /// Computes a slippage-adjusted floor from `expected_output` using the
    /// solver's slippage policy expressed in basis points.
    ///
    /// Mirrors the legacy `apply_slippage` so both providers enforce the
    /// same number — on the legacy path this becomes `_min_dy` in the
    /// re-encoded calldata, on the new-router path it becomes the request's
    /// `min_out` AND `_min_out` in the returned calldata.
    pub fn min_out_with_slippage(expected_output: eth::U256, slippage_bps: u32) -> eth::U256 {
        let factor = eth::U256::from(10_000u32).saturating_sub(eth::U256::from(slippage_bps));
        expected_output.saturating_mul(factor) / eth::U256::from(10_000u32)
    }
}

/// Errors a [`RouteProvider`] can surface.
///
/// `Api` wraps the legacy client's error so existing call-sites that match on
/// `api::Error` keep working through the trait; new-router-specific failure
/// modes get their own variants.
#[derive(Debug)]
pub enum Error {
    /// Underlying transport / parse / validation failure.
    Api(api::Error),
    /// New-router said calldata is not available (e.g. quote with no
    /// `receiver`, or `min_out` exceeded). Real-solve callers must treat
    /// this as fatal — see [`ExecutableQuote::calldata`] contract.
    CalldataUnavailable(String),
    /// Returned router address didn't match `ChainConfig::router_address`.
    /// Always a hard failure: granting allowance to an unexpected spender
    /// is the exact thing we refuse to do, no matter how plausible the
    /// quote looks.
    RouterAddressMismatch {
        expected: eth::Address,
        got: eth::Address,
    },
    /// Returned `final_token` in the calldata didn't match the requested
    /// `buy_token`. Defense in depth: the on-chain router enforces this too,
    /// but failing early is cheaper than a revert.
    FinalTokenMismatch {
        expected: eth::Address,
        got: eth::Address,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(e) => write!(f, "{e}"),
            Self::CalldataUnavailable(msg) => write!(f, "calldata unavailable: {msg}"),
            Self::RouterAddressMismatch { expected, got } => write!(
                f,
                "router address mismatch: expected {expected:?}, got {got:?}"
            ),
            Self::FinalTokenMismatch { expected, got } => write!(
                f,
                "final token mismatch: expected {expected:?}, got {got:?}"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<api::Error> for Error {
    fn from(e: api::Error) -> Self {
        Self::Api(e)
    }
}

/// Provider abstraction. One impl per backend (legacy, new-router).
#[async_trait]
pub trait RouteProvider: Send + Sync {
    /// Fetches a route and returns an executable artifact ready for
    /// `CustomInteraction` construction.
    async fn quote(&self, req: &QuoteRequest) -> Result<ExecutableQuote, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_out_with_slippage_100_bps_is_99_percent() {
        let expected = eth::U256::from(1_000_000u64);
        let floor = QuoteRequest::min_out_with_slippage(expected, 100);
        assert_eq!(floor, eth::U256::from(990_000u64));
    }

    #[test]
    fn min_out_with_slippage_zero_bps_returns_expected() {
        let expected = eth::U256::from(123_456_789u64);
        let floor = QuoteRequest::min_out_with_slippage(expected, 0);
        assert_eq!(floor, expected);
    }

    #[test]
    fn min_out_with_slippage_caps_at_zero_when_bps_above_10000() {
        // Defensive: a misconfigured slippage_bps shouldn't underflow into a
        // gigantic floor. Saturating sub clamps the factor to zero.
        let expected = eth::U256::from(1_000_000u64);
        let floor = QuoteRequest::min_out_with_slippage(expected, 20_000);
        assert_eq!(floor, eth::U256::ZERO);
    }

    #[test]
    fn quote_quality_slugs_are_stable() {
        // tg-monitor.sh greps these — renames are a wire-format break.
        assert_eq!(QuoteQuality::Interpolated.as_slug(), "interpolated");
        assert_eq!(QuoteQuality::Route.as_slug(), "route");
        assert_eq!(QuoteQuality::RouterExecution.as_slug(), "router_execution");
    }

    #[test]
    fn error_display_includes_addresses_on_router_mismatch() {
        let err = Error::RouterAddressMismatch {
            expected: eth::Address::repeat_byte(0xAA),
            got: eth::Address::repeat_byte(0xBB),
        };
        let s = format!("{err}");
        assert!(s.contains("expected"));
        assert!(s.contains("got"));
    }
}
