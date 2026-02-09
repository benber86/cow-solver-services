//! Curve LP solver integration tests.
//!
//! These tests require network access to Curve APIs and an Ethereum RPC node.
//! They are ignored by default and should be run manually with:
//!
//! ```sh
//! cargo test -p solvers curve_lp -- --ignored
//! ```
//!
//! Make sure to set up a valid config file at `configs/local/curve-lp.local.toml`
//! before running these tests.

use {crate::tests, serde_json::json, std::time::Duration};

async fn create_solver_engine() -> tests::SolverEngine {
    // Use the local config with real API keys
    // Path is relative to crates/solvers/ (where tests run from)
    let config_path = std::env::var("CURVE_LP_CONFIG")
        .unwrap_or_else(|_| "../../configs/local/curve-lp.local.toml".to_string());

    // Give the solver time to initialize (RPC connection, etc.)
    tokio::time::timeout(
        Duration::from_secs(30),
        tests::SolverEngine::new("curvelp", tests::Config::File(config_path.into())),
    )
    .await
    .expect("solver engine failed to start within 30 seconds")
}

/// Test selling TricryptoUSDT LP token for crvUSD.
#[tokio::test]
#[ignore = "requires network access to Curve APIs and RPC node"]
async fn tricrypto_usdt_to_crvusd() {
    let engine = create_solver_engine().await;

    let solution = engine
        .solve(json!({
            "id": "1",
            "tokens": {
                "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4": {
                    "decimals": 18,
                    "symbol": "TricryptoUSDT",
                    "availableBalance": "1000000000000000000",
                    "trusted": true
                },
                "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E": {
                    "decimals": 18,
                    "symbol": "crvUSD",
                    "referencePrice": "598672283383404855983005159",
                    "availableBalance": "0",
                    "trusted": true
                }
            },
            "orders": [
                {
                    "uid": "0x0101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101",
                    "sellToken": "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4",
                    "buyToken": "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E",
                    "sellAmount": "1000000000000000000",
                    "fullSellAmount": "1000000000000000000",
                    "buyAmount": "1",
                    "fullBuyAmount": "1",
                    "feePolicies": [],
                    "validTo": 0,
                    "kind": "sell",
                    "owner": "0x5b1e2c2762667331bc91648052f646d1b0d35984",
                    "partiallyFillable": false,
                    "preInteractions": [],
                    "postInteractions": [],
                    "sellTokenSource": "erc20",
                    "buyTokenDestination": "erc20",
                    "class": "market",
                    "appData": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "signingScheme": "presign",
                    "signature": "0x"
                }
            ],
            "liquidity": [],
            "effectiveGasPrice": "15000000000",
            "deadline": "2099-01-01T00:00:00.000Z",
            "surplusCapturingJitOrderOwners": []
        }))
        .await;

    let solutions = solution["solutions"].as_array().unwrap();
    assert_eq!(solutions.len(), 1, "expected 1 solution for TricryptoUSDT");
}

/// Test selling crv3crypto LP token for crvUSD.
#[tokio::test]
#[ignore = "requires network access to Curve APIs and RPC node"]
async fn crv3crypto_to_crvusd() {
    let engine = create_solver_engine().await;

    let solution = engine
        .solve(json!({
            "id": "1",
            "tokens": {
                "0xc4AD29ba4B3c580e6D59105FFf484999997675Ff": {
                    "decimals": 18,
                    "symbol": "crv3crypto",
                    "availableBalance": "1000000000000000000",
                    "trusted": true
                },
                "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E": {
                    "decimals": 18,
                    "symbol": "crvUSD",
                    "referencePrice": "598672283383404855983005159",
                    "availableBalance": "0",
                    "trusted": true
                }
            },
            "orders": [
                {
                    "uid": "0x0202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202",
                    "sellToken": "0xc4AD29ba4B3c580e6D59105FFf484999997675Ff",
                    "buyToken": "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E",
                    "sellAmount": "1000000000000000000",
                    "fullSellAmount": "1000000000000000000",
                    "buyAmount": "1",
                    "fullBuyAmount": "1",
                    "feePolicies": [],
                    "validTo": 0,
                    "kind": "sell",
                    "owner": "0x5b1e2c2762667331bc91648052f646d1b0d35984",
                    "partiallyFillable": false,
                    "preInteractions": [],
                    "postInteractions": [],
                    "sellTokenSource": "erc20",
                    "buyTokenDestination": "erc20",
                    "class": "market",
                    "appData": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "signingScheme": "presign",
                    "signature": "0x"
                }
            ],
            "liquidity": [],
            "effectiveGasPrice": "15000000000",
            "deadline": "2099-01-01T00:00:00.000Z",
            "surplusCapturingJitOrderOwners": []
        }))
        .await;

    let solutions = solution["solutions"].as_array().unwrap();
    assert_eq!(solutions.len(), 1, "expected 1 solution for crv3crypto");
}

/// Test selling TricryptoUSDC LP token for crvUSD.
#[tokio::test]
#[ignore = "requires network access to Curve APIs and RPC node"]
async fn tricrypto_usdc_to_crvusd() {
    let engine = create_solver_engine().await;

    let solution = engine
        .solve(json!({
            "id": "1",
            "tokens": {
                "0x7F86Bf177Dd4F3494b841a37e810A34dD56c829B": {
                    "decimals": 18,
                    "symbol": "TricryptoUSDC",
                    "availableBalance": "1000000000000000000",
                    "trusted": true
                },
                "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E": {
                    "decimals": 18,
                    "symbol": "crvUSD",
                    "referencePrice": "598672283383404855983005159",
                    "availableBalance": "0",
                    "trusted": true
                }
            },
            "orders": [
                {
                    "uid": "0x0303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303",
                    "sellToken": "0x7F86Bf177Dd4F3494b841a37e810A34dD56c829B",
                    "buyToken": "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E",
                    "sellAmount": "1000000000000000000",
                    "fullSellAmount": "1000000000000000000",
                    "buyAmount": "1",
                    "fullBuyAmount": "1",
                    "feePolicies": [],
                    "validTo": 0,
                    "kind": "sell",
                    "owner": "0x5b1e2c2762667331bc91648052f646d1b0d35984",
                    "partiallyFillable": false,
                    "preInteractions": [],
                    "postInteractions": [],
                    "sellTokenSource": "erc20",
                    "buyTokenDestination": "erc20",
                    "class": "market",
                    "appData": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "signingScheme": "presign",
                    "signature": "0x"
                }
            ],
            "liquidity": [],
            "effectiveGasPrice": "15000000000",
            "deadline": "2099-01-01T00:00:00.000Z",
            "surplusCapturingJitOrderOwners": []
        }))
        .await;

    let solutions = solution["solutions"].as_array().unwrap();
    assert_eq!(solutions.len(), 1, "expected 1 solution for TricryptoUSDC");
}

/// Test all 3 LP tokens in a single auction.
#[tokio::test]
#[ignore = "requires network access to Curve APIs and RPC node"]
async fn all_lp_tokens_to_crvusd() {
    let engine = create_solver_engine().await;

    let solution = engine
        .solve(json!({
            "id": "1",
            "tokens": {
                "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4": {
                    "decimals": 18,
                    "symbol": "TricryptoUSDT",
                    "availableBalance": "1000000000000000000",
                    "trusted": true
                },
                "0xc4AD29ba4B3c580e6D59105FFf484999997675Ff": {
                    "decimals": 18,
                    "symbol": "crv3crypto",
                    "availableBalance": "1000000000000000000",
                    "trusted": true
                },
                "0x7F86Bf177Dd4F3494b841a37e810A34dD56c829B": {
                    "decimals": 18,
                    "symbol": "TricryptoUSDC",
                    "availableBalance": "1000000000000000000",
                    "trusted": true
                },
                "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E": {
                    "decimals": 18,
                    "symbol": "crvUSD",
                    "referencePrice": "598672283383404855983005159",
                    "availableBalance": "0",
                    "trusted": true
                }
            },
            "orders": [
                {
                    "uid": "0x0101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101",
                    "sellToken": "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4",
                    "buyToken": "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E",
                    "sellAmount": "1000000000000000000",
                    "fullSellAmount": "1000000000000000000",
                    "buyAmount": "1",
                    "fullBuyAmount": "1",
                    "feePolicies": [],
                    "validTo": 0,
                    "kind": "sell",
                    "owner": "0x5b1e2c2762667331bc91648052f646d1b0d35984",
                    "partiallyFillable": false,
                    "preInteractions": [],
                    "postInteractions": [],
                    "sellTokenSource": "erc20",
                    "buyTokenDestination": "erc20",
                    "class": "market",
                    "appData": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "signingScheme": "presign",
                    "signature": "0x"
                },
                {
                    "uid": "0x0202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202",
                    "sellToken": "0xc4AD29ba4B3c580e6D59105FFf484999997675Ff",
                    "buyToken": "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E",
                    "sellAmount": "1000000000000000000",
                    "fullSellAmount": "1000000000000000000",
                    "buyAmount": "1",
                    "fullBuyAmount": "1",
                    "feePolicies": [],
                    "validTo": 0,
                    "kind": "sell",
                    "owner": "0x5b1e2c2762667331bc91648052f646d1b0d35984",
                    "partiallyFillable": false,
                    "preInteractions": [],
                    "postInteractions": [],
                    "sellTokenSource": "erc20",
                    "buyTokenDestination": "erc20",
                    "class": "market",
                    "appData": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "signingScheme": "presign",
                    "signature": "0x"
                },
                {
                    "uid": "0x0303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303",
                    "sellToken": "0x7F86Bf177Dd4F3494b841a37e810A34dD56c829B",
                    "buyToken": "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E",
                    "sellAmount": "1000000000000000000",
                    "fullSellAmount": "1000000000000000000",
                    "buyAmount": "1",
                    "fullBuyAmount": "1",
                    "feePolicies": [],
                    "validTo": 0,
                    "kind": "sell",
                    "owner": "0x5b1e2c2762667331bc91648052f646d1b0d35984",
                    "partiallyFillable": false,
                    "preInteractions": [],
                    "postInteractions": [],
                    "sellTokenSource": "erc20",
                    "buyTokenDestination": "erc20",
                    "class": "market",
                    "appData": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "signingScheme": "presign",
                    "signature": "0x"
                }
            ],
            "liquidity": [],
            "effectiveGasPrice": "15000000000",
            "deadline": "2099-01-01T00:00:00.000Z",
            "surplusCapturingJitOrderOwners": []
        }))
        .await;

    let solutions = solution["solutions"].as_array().unwrap();
    assert_eq!(solutions.len(), 3, "expected 3 solutions for all LP tokens");
}

/// Test that the default config (no token lists) accepts any Curve-routable
/// pair. TricryptoUSDT→DAI is routable via the Curve Router.
#[tokio::test]
#[ignore = "requires network access to Curve APIs and RPC node"]
async fn accepts_any_routable_pair() {
    let engine = create_solver_engine().await;

    let solution = engine
        .solve(json!({
            "id": "1",
            "tokens": {
                "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4": {
                    "decimals": 18,
                    "symbol": "TricryptoUSDT",
                    "availableBalance": "1000000000000000000",
                    "trusted": true
                },
                "0x6B175474E89094C44Da98b954EedeAC495271d0F": {
                    "decimals": 18,
                    "symbol": "DAI",
                    "availableBalance": "0",
                    "trusted": true
                }
            },
            "orders": [
                {
                    "uid": "0x0404040404040404040404040404040404040404040404040404040404040404040404040404040404040404040404040404040404040404",
                    "sellToken": "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4",
                    "buyToken": "0x6B175474E89094C44Da98b954EedeAC495271d0F",
                    "sellAmount": "1000000000000000000",
                    "fullSellAmount": "1000000000000000000",
                    "buyAmount": "1",
                    "fullBuyAmount": "1",
                    "feePolicies": [],
                    "validTo": 0,
                    "kind": "sell",
                    "owner": "0x5b1e2c2762667331bc91648052f646d1b0d35984",
                    "partiallyFillable": false,
                    "preInteractions": [],
                    "postInteractions": [],
                    "sellTokenSource": "erc20",
                    "buyTokenDestination": "erc20",
                    "class": "market",
                    "appData": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "signingScheme": "presign",
                    "signature": "0x"
                }
            ],
            "liquidity": [],
            "effectiveGasPrice": "15000000000",
            "deadline": "2099-01-01T00:00:00.000Z",
            "surplusCapturingJitOrderOwners": []
        }))
        .await;

    let solutions = solution["solutions"].as_array().unwrap();
    assert_eq!(
        solutions.len(),
        1,
        "default config should accept any Curve-routable pair"
    );
}

/// Test that explicit token lists still filter correctly. With allowed-buy-tokens
/// set to only crvUSD, a TricryptoUSDT→DAI order should be rejected.
#[tokio::test]
#[ignore = "requires network access to Curve APIs and RPC node"]
async fn rejects_filtered_buy_token() {
    // Use an inline config with explicit token lists
    let config_path = std::env::var("CURVE_LP_CONFIG")
        .unwrap_or_else(|_| "../../configs/local/curve-lp.local.toml".to_string());
    let base_config = tokio::fs::read_to_string(&config_path)
        .await
        .expect("failed to read base config");

    // Append explicit token lists to restrict filtering
    let config_with_filter = format!(
        "{}\n\
         lp-tokens = [\"0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4\"]\n\
         allowed-buy-tokens = [\"0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E\"]\n",
        base_config
    );

    let engine = tokio::time::timeout(
        Duration::from_secs(30),
        tests::SolverEngine::new("curvelp", tests::Config::String(config_with_filter)),
    )
    .await
    .expect("solver engine failed to start within 30 seconds");

    let solution = engine
        .solve(json!({
            "id": "1",
            "tokens": {
                "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4": {
                    "decimals": 18,
                    "symbol": "TricryptoUSDT",
                    "availableBalance": "1000000000000000000",
                    "trusted": true
                },
                "0x6B175474E89094C44Da98b954EedeAC495271d0F": {
                    "decimals": 18,
                    "symbol": "DAI",
                    "availableBalance": "0",
                    "trusted": true
                }
            },
            "orders": [
                {
                    "uid": "0x0404040404040404040404040404040404040404040404040404040404040404040404040404040404040404040404040404040404040404",
                    "sellToken": "0xf5f5B97624542D72A9E06f04804Bf81baA15e2B4",
                    "buyToken": "0x6B175474E89094C44Da98b954EedeAC495271d0F",
                    "sellAmount": "1000000000000000000",
                    "fullSellAmount": "1000000000000000000",
                    "buyAmount": "1",
                    "fullBuyAmount": "1",
                    "feePolicies": [],
                    "validTo": 0,
                    "kind": "sell",
                    "owner": "0x5b1e2c2762667331bc91648052f646d1b0d35984",
                    "partiallyFillable": false,
                    "preInteractions": [],
                    "postInteractions": [],
                    "sellTokenSource": "erc20",
                    "buyTokenDestination": "erc20",
                    "class": "market",
                    "appData": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "signingScheme": "presign",
                    "signature": "0x"
                }
            ],
            "liquidity": [],
            "effectiveGasPrice": "15000000000",
            "deadline": "2099-01-01T00:00:00.000Z",
            "surplusCapturingJitOrderOwners": []
        }))
        .await;

    let solutions = solution["solutions"].as_array().unwrap();
    assert_eq!(
        solutions.len(),
        0,
        "should reject buy token not in allowed-buy-tokens"
    );
}

/// Test selling crvCVXETH LP token for crvUSD.
///
/// Based on historical tx:
/// https://explorer.cow.fi/tx/0x4d94ed032f01b8e2fdf85eb2411c9bb50140ac6cdeb359590059828d78b30884
///
/// This token was NOT in the old whitelist, validating accept-all behavior.
#[tokio::test]
#[ignore = "requires network access to Curve APIs and RPC node"]
async fn crvcvxeth_to_crvusd() {
    let engine = create_solver_engine().await;

    let solution = engine
        .solve(json!({
            "id": "1",
            "tokens": {
                "0x3A283D9c08E8b55966afb64C515f5143cf907611": {
                    "decimals": 18,
                    "symbol": "crvCVXETH",
                    "availableBalance": "58592602838992864786",
                    "trusted": true
                },
                "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E": {
                    "decimals": 18,
                    "symbol": "crvUSD",
                    "referencePrice": "598672283383404855983005159",
                    "availableBalance": "0",
                    "trusted": true
                }
            },
            "orders": [
                {
                    "uid": "0x0505050505050505050505050505050505050505050505050505050505050505050505050505050505050505050505050505050505050505",
                    "sellToken": "0x3A283D9c08E8b55966afb64C515f5143cf907611",
                    "buyToken": "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E",
                    "sellAmount": "58592602838992864786",
                    "fullSellAmount": "58592602838992864786",
                    "buyAmount": "500000000000000000000",
                    "fullBuyAmount": "500000000000000000000",
                    "feePolicies": [],
                    "validTo": 0,
                    "kind": "sell",
                    "owner": "0xc0fc3ddfec95ca45a0d2393f518d3ea1ccf44f8b",
                    "partiallyFillable": true,
                    "preInteractions": [],
                    "postInteractions": [],
                    "sellTokenSource": "erc20",
                    "buyTokenDestination": "erc20",
                    "class": "limit",
                    "appData": "0x058315b749613051abcbf50cf2d605b4fa4a41554ec35d73fd058fc530da559f",
                    "signingScheme": "presign",
                    "signature": "0x"
                }
            ],
            "liquidity": [],
            "effectiveGasPrice": "15000000000",
            "deadline": "2099-01-01T00:00:00.000Z",
            "surplusCapturingJitOrderOwners": []
        }))
        .await;

    let solutions = solution["solutions"].as_array().unwrap();
    assert_eq!(
        solutions.len(),
        1,
        "expected 1 solution for crvCVXETH → crvUSD (previously not whitelisted)"
    );
}
