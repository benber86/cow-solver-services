//! Configuration for the Curve LP solver.

use {
    crate::domain::{eth, solver::curve_lp},
    reqwest::Url,
    serde::Deserialize,
    shared::price_estimation::gas::SETTLEMENT_OVERHEAD,
    std::path::Path,
    tokio::fs,
};

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Config {
    /// Chain ID (1 for mainnet).
    chain_id: u64,

    /// Whitelisted LP tokens that this solver handles.
    lp_tokens: Vec<eth::Address>,

    /// Allowed buy tokens (crvUSD + pool underlyings).
    allowed_buy_tokens: Vec<eth::Address>,

    /// Curve Router API URL.
    curve_api_url: Url,

    /// Curve Price API URL.
    curve_price_api_url: Url,

    /// Node URL for on-chain verification.
    node_url: Url,

    /// Slippage buffer in basis points (e.g., 100 = 1%).
    #[serde(default = "default_slippage_bps")]
    slippage_bps: u32,

    /// Maximum deviation between API quote and on-chain get_dy (basis points).
    #[serde(default = "default_max_quote_deviation_bps")]
    max_quote_deviation_bps: u32,

    /// Gas offset for solution gas estimation.
    #[serde(default = "default_gas_offset")]
    solution_gas_offset: i64,

    /// Settlement contract address.
    settlement_contract: eth::Address,
}

fn default_slippage_bps() -> u32 {
    100 // 1%
}

fn default_max_quote_deviation_bps() -> u32 {
    50 // 0.5%
}

fn default_gas_offset() -> i64 {
    SETTLEMENT_OVERHEAD.try_into().unwrap()
}

/// Load the Curve LP solver configuration from a TOML file.
///
/// # Panics
///
/// This method panics if the config is invalid or on I/O errors.
pub async fn load(path: &Path) -> curve_lp::Config {
    let data = fs::read_to_string(path)
        .await
        .unwrap_or_else(|e| panic!("I/O error while reading {path:?}: {e:?}"));

    let config: Config = toml::de::from_str(&data).unwrap_or_else(|err| {
        if std::env::var("TOML_TRACE_ERROR").is_ok_and(|v| v == "1") {
            panic!("failed to parse TOML config at {path:?}: {err:#?}")
        } else {
            panic!(
                "failed to parse TOML config at: {path:?}. Set TOML_TRACE_ERROR=1 to print \
                 parsing error but this may leak secrets."
            )
        }
    });

    curve_lp::Config {
        chain_id: config.chain_id,
        lp_tokens: config.lp_tokens,
        allowed_buy_tokens: config.allowed_buy_tokens,
        curve_api_url: config.curve_api_url,
        curve_price_api_url: config.curve_price_api_url,
        node_url: config.node_url,
        slippage_bps: config.slippage_bps,
        max_quote_deviation_bps: config.max_quote_deviation_bps,
        solution_gas_offset: config.solution_gas_offset.into(),
        settlement_contract: config.settlement_contract,
    }
}
