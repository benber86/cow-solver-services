//! Configuration for the Curve LP solver.

use {
    crate::domain::{
        eth,
        solver::curve_lp::{self, ChainConfig, CurvePriceApiChain},
    },
    reqwest::Url,
    serde::Deserialize,
    shared::price_estimation::gas::SETTLEMENT_OVERHEAD,
    std::path::Path,
    tokio::fs,
};

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Config {
    /// Chain ID (1 for mainnet, 100 for Gnosis, 42161 for Arbitrum).
    chain_id: u64,

    /// Curve router contract address (per-chain; see curve-router-ng).
    router_address: eth::Address,

    /// Wrapped native token address (WETH on Ethereum/Arbitrum, WXDAI on Gnosis).
    /// Not validated against a canonical deployment; the solver trusts the
    /// config so forks / test deployments can override.
    wrapped_native_token: eth::Address,

    /// Curve Price API chain slug. Note: Curve uses "arbitrum" (not Coingecko's
    /// "arbitrum-one") and "xdai" for Gnosis.
    price_api_chain: CurvePriceApiChain,

    /// Whitelisted LP tokens that this solver handles.
    /// Omit to accept any sell token.
    #[serde(default)]
    lp_tokens: Option<Vec<eth::Address>>,

    /// Allowed buy tokens (crvUSD + pool underlyings).
    /// Omit to accept any buy token.
    #[serde(default)]
    allowed_buy_tokens: Option<Vec<eth::Address>>,

    /// Strict both-sides token allowlist. If set, the solver rejects any
    /// order where either `sell.token` or `buy.token` is not in this list.
    /// Independent of (and stricter than) `lp_tokens` / `allowed_buy_tokens`,
    /// which are either-side filters.
    #[serde(default)]
    token_allowlist: Option<Vec<eth::Address>>,

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

    /// Settlement contract address. Not validated against a canonical
    /// deployment; the solver trusts the config so forks / test deployments
    /// can override.
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

    let chain = ChainConfig {
        chain_id: config.chain_id,
        router_address: config.router_address,
        wrapped_native_token: config.wrapped_native_token,
        price_api_chain: config.price_api_chain,
        settlement_contract: config.settlement_contract,
    }
    .validated()
    .unwrap_or_else(|e| panic!("invalid chain config in {path:?}: {e}"));

    curve_lp::Config {
        chain,
        lp_tokens: config.lp_tokens,
        allowed_buy_tokens: config.allowed_buy_tokens,
        token_allowlist: config.token_allowlist,
        curve_api_url: config.curve_api_url,
        curve_price_api_url: config.curve_price_api_url,
        node_url: config.node_url,
        slippage_bps: config.slippage_bps,
        max_quote_deviation_bps: config.max_quote_deviation_bps,
        solution_gas_offset: config.solution_gas_offset.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a TOML string (substituting in a dummy value for any
    /// `${NODE_URL}`) and runs it through the same validation pipeline as
    /// `load`. Catches shape / missing-field / validation errors without
    /// needing a filesystem or env setup.
    fn parse_and_validate(raw_toml: &str) -> Result<ChainConfig, String> {
        let substituted = raw_toml.replace("${NODE_URL}", "https://dummy.invalid/");
        let parsed: Config = toml::de::from_str(&substituted)
            .map_err(|e| format!("parse error: {e:#?}"))?;
        ChainConfig {
            chain_id: parsed.chain_id,
            router_address: parsed.router_address,
            wrapped_native_token: parsed.wrapped_native_token,
            price_api_chain: parsed.price_api_chain,
            settlement_contract: parsed.settlement_contract,
        }
        .validated()
        .map_err(|e| e.to_string())
    }

    #[test]
    fn example_mainnet_config_is_valid() {
        let raw = include_str!("../../../config/example.curve-lp.toml");
        parse_and_validate(raw).expect("mainnet example should parse and validate");
    }

    #[test]
    fn example_arbitrum_config_is_valid() {
        let raw = include_str!("../../../config/example.curve-lp.arbitrum.toml");
        parse_and_validate(raw).expect("arbitrum example should parse and validate");
    }

    #[test]
    fn example_gnosis_config_is_valid() {
        let raw = include_str!("../../../config/example.curve-lp.gnosis.toml");
        parse_and_validate(raw).expect("gnosis example should parse and validate");
    }

    #[test]
    fn local_config_is_valid() {
        let raw = include_str!("../../../../../configs/local/curve-lp.local.toml");
        parse_and_validate(raw).expect("local config should parse and validate");
    }

    #[test]
    fn prod_config_is_valid() {
        let raw = include_str!("../../../../../deploy/curve-lp/curve-lp.prod.toml");
        parse_and_validate(raw).expect("prod config should parse and validate");
    }

    #[test]
    fn staging_config_is_valid() {
        let raw = include_str!("../../../../../deploy/curve-lp/curve-lp.staging.toml");
        parse_and_validate(raw).expect("staging config should parse and validate");
    }

    #[test]
    fn arbitrum_deploy_config_is_valid() {
        let raw = include_str!("../../../../../deploy/curve-lp/curve-lp.arbitrum.toml");
        parse_and_validate(raw).expect("arbitrum deploy config should parse and validate");
    }

    #[test]
    fn gnosis_deploy_config_is_valid() {
        let raw = include_str!("../../../../../deploy/curve-lp/curve-lp.gnosis.toml");
        parse_and_validate(raw).expect("gnosis deploy config should parse and validate");
    }

    #[test]
    fn arbitrum_staging_deploy_config_is_valid() {
        let raw = include_str!("../../../../../deploy/curve-lp/curve-lp.arbitrum-staging.toml");
        parse_and_validate(raw)
            .expect("arbitrum staging deploy config should parse and validate");
    }

    #[test]
    fn gnosis_staging_deploy_config_is_valid() {
        let raw = include_str!("../../../../../deploy/curve-lp/curve-lp.gnosis-staging.toml");
        parse_and_validate(raw)
            .expect("gnosis staging deploy config should parse and validate");
    }

    /// A minimal TOML document with just the required fields, plus whatever
    /// the caller appends. Used by the token-allowlist serde tests to avoid
    /// coupling them to any of the real deploy TOMLs (whose `token-allowlist`
    /// is commented out, which defeats the purpose of the assertion).
    fn minimal_toml(extra: &str) -> String {
        format!(
            r#"
chain-id = 1
router-address = "0x45312ea0eFf7E09C83CBE249fa1d7598c4C8cd4e"
wrapped-native-token = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
price-api-chain = "ethereum"
node-url = "https://dummy.invalid/"
curve-api-url = "https://dummy.invalid/"
curve-price-api-url = "https://dummy.invalid/"
settlement-contract = "0x9008D19f58AAbD9eD0D60971565AA8510560ab41"
{extra}
"#
        )
    }

    #[test]
    fn token_allowlist_populated_parses_and_preserves_addresses() {
        let raw = minimal_toml(
            r#"
token-allowlist = [
    "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1",
    "0xaf88d065e77c8cC2239327C5EDb3A432268e5831",
]
"#,
        );
        let parsed: Config = toml::de::from_str(&raw).expect("should parse");
        let list = parsed
            .token_allowlist
            .expect("token_allowlist should be present");
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0],
            "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1"
                .parse::<eth::Address>()
                .unwrap()
        );
        assert_eq!(
            list[1],
            "0xaf88d065e77c8cC2239327C5EDb3A432268e5831"
                .parse::<eth::Address>()
                .unwrap()
        );
    }

    #[test]
    fn token_allowlist_omitted_defaults_to_none() {
        // Proves `#[serde(default)]` is wired on the field — without it the
        // deserializer would reject a document that omits the key.
        let raw = minimal_toml("");
        let parsed: Config = toml::de::from_str(&raw).expect("should parse");
        assert!(parsed.token_allowlist.is_none());
    }

    #[test]
    fn token_allowlist_wrong_key_spelling_is_rejected() {
        // `deny_unknown_fields` is set on Config, so the snake_case variant
        // (or any typo) fails parsing. This locks in the kebab-case key.
        let raw = minimal_toml(
            r#"
token_allowlist = [ "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1" ]
"#,
        );
        let result: Result<Config, _> = toml::de::from_str(&raw);
        assert!(
            result.is_err(),
            "snake_case token_allowlist should be rejected by deny_unknown_fields"
        );
    }
}
