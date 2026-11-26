//! Simulating a quote's pre-calls and its swap as one settlement would run
//! them: same sender, same block, in order.

use {
    super::route_provider::ExecutableQuote,
    crate::domain::eth,
    alloy::{
        primitives::{Address, U256},
        providers::Provider,
        rpc::types::{
            BlockOverrides,
            TransactionInput,
            TransactionRequest,
            simulate::{SimBlock, SimulatePayload},
        },
        sol,
        sol_types::SolCall,
    },
    std::fmt,
};

sol! {
    function transfer(address to, uint256 amount) external returns (bool);
    function approve(address spender, uint256 amount) external returns (bool);
    function balanceOf(address owner) external view returns (uint256);
}

#[derive(Debug)]
pub enum Error {
    /// The node could not run the simulation at all.
    Rpc(String),
    /// The order's owner could not fund the settlement in simulation, so the
    /// swap never ran and nothing was learned about it.
    Funding(String),
    /// A pre-call or the swap reverted.
    Reverted {
        step: &'static str,
        reason: String,
    },
    BelowFloor {
        received: U256,
        floor: U256,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(msg) => write!(f, "simulation rpc failed: {msg}"),
            Self::Funding(msg) => write!(f, "could not fund the simulated settlement: {msg}"),
            Self::Reverted { step, reason } => write!(f, "{step} reverted: {reason}"),
            Self::BelowFloor { received, floor } => {
                write!(f, "received {received} below the floor {floor}")
            }
        }
    }
}

pub struct Request<'a> {
    pub settlement: Address,
    pub owner: Address,
    pub sell_token: Address,
    pub sell_amount: U256,
    pub buy_token: Address,
    pub quote: &'a ExecutableQuote,
    /// Block timestamp to simulate at: the one the quote was priced for.
    pub timestamp: u64,
}

fn call(from: Address, to: Address, data: Vec<u8>, value: U256) -> TransactionRequest {
    TransactionRequest::default()
        .from(from)
        .to(to)
        .value(value)
        .input(TransactionInput::new(data.into()))
}

// What the settlement does around the solver's interactions, minus the parts a
// simulation cannot reproduce (the owner's signature moving the sell tokens).
fn calls(req: &Request<'_>) -> Vec<TransactionRequest> {
    let quote = req.quote;
    let balance = || {
        call(
            req.settlement,
            req.buy_token,
            balanceOfCall {
                owner: req.settlement,
            }
            .abi_encode(),
            U256::ZERO,
        )
    };
    let approve = |amount: U256| {
        call(
            req.settlement,
            req.sell_token,
            approveCall {
                spender: quote.router_address,
                amount,
            }
            .abi_encode(),
            U256::ZERO,
        )
    };

    let mut calls = vec![
        call(
            req.owner,
            req.sell_token,
            transferCall {
                to: req.settlement,
                amount: req.sell_amount,
            }
            .abi_encode(),
            U256::ZERO,
        ),
        // Tokens like USDT refuse to change a non-zero allowance.
        approve(U256::ZERO),
        approve(req.sell_amount),
        balance(),
    ];
    calls.extend(
        quote
            .pre_calls
            .iter()
            .map(|pre| call(req.settlement, pre.target, pre.calldata.clone(), pre.value)),
    );
    calls.push(call(
        req.settlement,
        quote.router_address,
        quote.calldata.clone(),
        U256::ZERO,
    ));
    calls.push(balance());
    calls
}

const TRANSFER: usize = 0;
const APPROVE: usize = 2;
const BALANCE_BEFORE: usize = 3;
const FIRST_PRE_CALL: usize = 4;

/// Runs the quote's pre-calls and swap from the settlement contract in one
/// simulated block and returns how much of the buy token arrived.
pub async fn simulate(
    provider: &ethrpc::AlloyProvider,
    req: Request<'_>,
) -> Result<eth::U256, Error> {
    let calls = calls(&req);
    let swap = calls.len() - 2;
    let balance_after = calls.len() - 1;
    let payload = SimulatePayload {
        block_state_calls: vec![SimBlock {
            block_overrides: Some(BlockOverrides {
                time: Some(req.timestamp),
                ..Default::default()
            }),
            state_overrides: None,
            calls,
        }],
        trace_transfers: false,
        validation: false,
        return_full_transactions: false,
    };
    let blocks = provider
        .simulate(&payload)
        .await
        .map_err(|err| Error::Rpc(err.to_string()))?;
    let results = &blocks
        .first()
        .ok_or_else(|| Error::Rpc("no simulated block returned".into()))?
        .calls;
    if results.len() != balance_after + 1 {
        return Err(Error::Rpc(format!(
            "expected {} call results, got {}",
            balance_after + 1,
            results.len()
        )));
    }
    let reason = |index: usize| {
        results[index]
            .error
            .as_ref()
            .map(|err| err.message.clone())
            .unwrap_or_else(|| "no reason given".into())
    };
    for index in [TRANSFER, APPROVE] {
        if !results[index].status {
            return Err(Error::Funding(reason(index)));
        }
    }
    if let Some(index) = (FIRST_PRE_CALL..swap).find(|index| !results[*index].status) {
        return Err(Error::Reverted {
            step: "pre-call",
            reason: reason(index),
        });
    }
    if !results[swap].status {
        return Err(Error::Reverted {
            step: "swap",
            reason: reason(swap),
        });
    }
    let balance = |index: usize| {
        let data = &results[index].return_data;
        (results[index].status && data.len() >= 32).then(|| U256::from_be_slice(&data[..32]))
    };
    let (Some(before), Some(after)) = (balance(BALANCE_BEFORE), balance(balance_after)) else {
        return Err(Error::Rpc("buy token balance could not be read".into()));
    };
    let received = after.saturating_sub(before);
    if received < req.quote.min_out {
        return Err(Error::BelowFloor {
            received,
            floor: req.quote.min_out,
        });
    }
    Ok(received)
}

#[cfg(test)]
mod tests {
    use {super::*, crate::domain::curve::route_provider::PreCall};

    fn quote(pre_calls: Vec<PreCall>) -> ExecutableQuote {
        ExecutableQuote {
            expected_output: U256::from(100),
            min_out: U256::from(99),
            router_address: Address::repeat_byte(0xCA),
            calldata: vec![0xde, 0xad],
            gas_estimate: None,
            quality: None,
            pre_calls,
            report: None,
        }
    }

    #[test]
    fn pre_calls_run_from_the_settlement_between_funding_and_the_swap() {
        let settlement = Address::repeat_byte(0x90);
        let owner = Address::repeat_byte(0x01);
        let bank = Address::repeat_byte(0xBA);
        let quote = quote(vec![PreCall {
            target: bank,
            value: U256::from(7),
            calldata: vec![0xbe, 0xef],
        }]);
        let calls = calls(&Request {
            settlement,
            owner,
            sell_token: Address::repeat_byte(0x11),
            sell_amount: U256::from(1_000),
            buy_token: Address::repeat_byte(0x22),
            quote: &quote,
            timestamp: 1_000,
        });

        assert_eq!(calls.len(), 7);
        assert_eq!(calls[TRANSFER].from, Some(owner));
        let pre = &calls[FIRST_PRE_CALL];
        assert_eq!(pre.from, Some(settlement));
        assert_eq!(pre.to, Some(bank.into()));
        assert_eq!(pre.value, Some(U256::from(7)));
        let swap = &calls[calls.len() - 2];
        assert_eq!(swap.from, Some(settlement));
        assert_eq!(swap.to, Some(quote.router_address.into()));
        assert_eq!(calls[BALANCE_BEFORE].to, calls[calls.len() - 1].to);
    }

    /// Live check against a report-aware router and a mainnet node:
    /// `ROUTER_URL=http://127.0.0.1:3918/quote NODE_URL=... cargo test -p
    /// solvers --lib live_report_route -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_report_route_simulates_from_the_settlement() {
        use crate::domain::curve::{
            new_router::NewRouterClient,
            route_provider::{QuoteRequest, RouteProvider},
        };

        let router_url = std::env::var("ROUTER_URL").expect("ROUTER_URL");
        let node_url = std::env::var("NODE_URL").expect("NODE_URL");
        let settlement = alloy::primitives::address!("9008D19f58AAbD9eD0D60971565AA8510560ab41");
        let crvusd = alloy::primitives::address!("f939E0A03FB07F59A73314E73794Be0E57ac1b4E");
        let weth = alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        // the crvUSD/USDC pool: any account holding the sell token will do
        let owner = alloy::primitives::address!("4DEcE678ceceb27446b35C672dC7d61F30bAD69E");
        let weth_pool = alloy::primitives::address!("656341Ef90b622c6634e0573772FfB7f3669b9f3");
        let unit = U256::from(10u64).pow(U256::from(18));
        // which direction a report helps depends on where the pool sits, so
        // try both; the pools hold plenty of either token to stand in as owner
        let directions = [
            (crvusd, weth, U256::from(16_000u64) * unit, owner),
            (weth, crvusd, U256::from(10u64) * unit, weth_pool),
        ];

        let client = NewRouterClient::new(
            router_url.parse().unwrap(),
            alloy::primitives::address!("a1c7a8360eb4049595a24d6919e74e105b409cb5"),
            30,
            std::time::Duration::ZERO,
        )
        .with_report_bank(Some(crate::domain::curve::route_provider::ReportBank {
            address: alloy::primitives::address!("8a0f866de34631311b7745C5f4eec7Bf1F6849dB"),
            max_fee: U256::ZERO,
        }));
        let mut found = None;
        for (sell_token, buy_token, sell_amount, owner) in directions {
            let quote = client
                .quote(&QuoteRequest {
                    sell_token,
                    buy_token,
                    sell_amount,
                    is_quote: false,
                    receiver: settlement,
                    min_out: Some(U256::from(1)),
                    gas_price_gwei: None,
                    report_aware: true,
                })
                .await
                .expect("router quote");
            if !quote.pre_calls.is_empty() {
                found = Some((sell_token, buy_token, sell_amount, owner, quote));
                break;
            }
        }
        let (crvusd, weth, sell_amount, owner, quote) =
            found.expect("router did not pick the report route in either direction");
        let report = quote.report.clone().expect("report pricing");
        println!("report: {report:?}");

        let web3 = ethrpc::web3(
            Default::default(),
            Default::default(),
            &node_url.parse().unwrap(),
            "test",
        );
        let received = simulate(
            &web3.alloy,
            Request {
                settlement,
                owner,
                sell_token: crvusd,
                sell_amount,
                buy_token: weth,
                quote: &quote,
                timestamp: report.inclusion_timestamp,
            },
        )
        .await
        .expect("joint simulation");
        println!(
            "simulated received {received}, router expected {}, floor {}",
            quote.expected_output, quote.min_out
        );
        assert!(received >= quote.min_out);

        // the same swap without the bank call must do worse: the report is
        // what the route's price rests on
        let mut without_report = quote.clone();
        without_report.pre_calls.clear();
        let outcome = simulate(
            &web3.alloy,
            Request {
                settlement,
                owner,
                sell_token: crvusd,
                sell_amount,
                buy_token: weth,
                quote: &without_report,
                timestamp: report.inclusion_timestamp,
            },
        )
        .await;
        println!("without the bank call: {outcome:?}");
        assert!(outcome.is_err());
    }
}
