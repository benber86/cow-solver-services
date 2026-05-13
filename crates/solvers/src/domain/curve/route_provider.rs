//! Abstraction over Curve route providers (legacy v1 API vs new /quote service).

use {
    crate::domain::{curve::api, eth},
    async_trait::async_trait,
    std::fmt,
};

#[derive(Debug, Clone)]
pub struct ExecutableQuote {
    pub expected_output: eth::U256,
    pub router_address: eth::Address,
    pub calldata: Vec<u8>,
    pub gas_estimate: Option<u64>,
    pub quality: Option<QuoteQuality>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteQuality {
    Interpolated,
    Route,
    RouterExecution,
}

impl QuoteQuality {
    /// Stable slug for log/tg-monitor.sh parsing.
    pub fn as_slug(self) -> &'static str {
        match self {
            Self::Interpolated => "interpolated",
            Self::Route => "route",
            Self::RouterExecution => "router_execution",
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub sell_token: eth::Address,
    pub buy_token: eth::Address,
    pub sell_amount: eth::U256,
    pub is_quote: bool,
    pub receiver: eth::Address,
    pub min_out: Option<eth::U256>,
    pub gas_price_gwei: Option<f64>,
}

impl QuoteRequest {
    pub fn min_out_with_slippage(expected_output: eth::U256, slippage_bps: u32) -> eth::U256 {
        let factor = eth::U256::from(10_000u32).saturating_sub(eth::U256::from(slippage_bps));
        expected_output.saturating_mul(factor) / eth::U256::from(10_000u32)
    }
}

#[derive(Debug)]
pub enum Error {
    Api(api::Error),
    CalldataUnavailable(String),
    RouterAddressMismatch {
        expected: eth::Address,
        got: eth::Address,
    },
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

#[async_trait]
pub trait RouteProvider: Send + Sync {
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
        // Saturating sub clamps a misconfigured slippage_bps to a zero floor
        // instead of underflowing into a huge value.
        let expected = eth::U256::from(1_000_000u64);
        let floor = QuoteRequest::min_out_with_slippage(expected, 20_000);
        assert_eq!(floor, eth::U256::ZERO);
    }

    #[test]
    fn quote_quality_slugs_are_stable() {
        // tg-monitor.sh greps these — renames break the wire format.
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
