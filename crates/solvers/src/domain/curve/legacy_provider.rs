//! `RouteProvider` impl backed by the legacy curve.finance v1 router API.

use {
    super::{
        api,
        route_provider::{Error, ExecutableQuote, QuoteRequest, RouteProvider},
    },
    crate::{boundary::curve::router, domain::eth},
    alloy::{primitives::U256, providers::Provider, rpc::types::TransactionRequest},
    async_trait::async_trait,
    std::time::Duration,
};

pub struct LegacyProvider {
    client: api::Client,
    provider: ethrpc::AlloyProvider,
    chain_id: u64,
    router_address: eth::Address,
    slippage_bps: u32,
    max_quote_deviation_bps: u32,
    onchain_verify_timeout: Duration,
}

impl LegacyProvider {
    pub fn new(
        client: api::Client,
        provider: ethrpc::AlloyProvider,
        chain_id: u64,
        router_address: eth::Address,
        slippage_bps: u32,
        max_quote_deviation_bps: u32,
        onchain_verify_timeout: Duration,
    ) -> Self {
        Self {
            client,
            provider,
            chain_id,
            router_address,
            slippage_bps,
            max_quote_deviation_bps,
            onchain_verify_timeout,
        }
    }

    async fn verify_onchain(
        &self,
        route: &api::Route,
        amount: eth::U256,
    ) -> Result<eth::U256, Error> {
        let calldata = router::encode_get_dy(route, amount);
        let tx = TransactionRequest::default()
            .to(self.router_address)
            .input(calldata.into());

        let result = self
            .provider
            .call(tx)
            .await
            .map_err(|e| Error::Api(api::Error::Network(format!("get_dy call failed: {e}"))))?;
        router::decode_get_dy_result(&result)
            .map_err(|e| Error::Api(api::Error::Parse(format!("get_dy decode: {e}"))))
    }

    fn deviation_bps(a: eth::U256, b: eth::U256) -> u32 {
        if a == U256::ZERO || b == U256::ZERO {
            return 0;
        }
        let diff = if a > b { a - b } else { b - a };
        let larger = if a > b { a } else { b };
        (diff.saturating_mul(U256::from(10_000u32)) / larger)
            .try_into()
            .unwrap_or(u32::MAX)
    }

    fn build_executable_quote(
        router_address: eth::Address,
        slippage_bps: u32,
        route: &api::Route,
        anchor_output: eth::U256,
        receiver: eth::Address,
        sell_amount: eth::U256,
    ) -> ExecutableQuote {
        let min_out = QuoteRequest::min_out_with_slippage(anchor_output, slippage_bps);
        let calldata = router::encode_exchange(route, sell_amount, min_out, receiver);
        ExecutableQuote {
            expected_output: anchor_output,
            min_out,
            router_address,
            calldata,
            gas_estimate: None,
            quality: None,
        }
    }
}

#[async_trait]
impl RouteProvider for LegacyProvider {
    async fn quote(&self, req: &QuoteRequest) -> Result<ExecutableQuote, Error> {
        let route = self
            .client
            .get_route(
                self.chain_id,
                req.sell_token,
                req.buy_token,
                req.sell_amount,
                0,
                0,
            )
            .await
            .map_err(Error::Api)?;

        // Quotes skip on-chain verification — never settled, and the ~750ms
        // RPC cost causes deadline timeouts on the driver side.
        let anchor_output = if req.is_quote {
            route.expected_output
        } else {
            if let Some(target_min) = req.min_out {
                let optimistic = route.expected_output.saturating_add(
                    route
                        .expected_output
                        .saturating_mul(U256::from(self.max_quote_deviation_bps))
                        / U256::from(10_000u32),
                );
                let optimistic_floor =
                    QuoteRequest::min_out_with_slippage(optimistic, self.slippage_bps);
                if optimistic_floor < target_min {
                    return Err(Error::Api(api::Error::InvalidRoute(format!(
                        "best-case API output {optimistic_floor} below required {target_min}"
                    ))));
                }
            }

            let onchain = tokio::time::timeout(
                self.onchain_verify_timeout,
                self.verify_onchain(&route, req.sell_amount),
            )
            .await
            .map_err(|_| {
                Error::Api(api::Error::Network(format!(
                    "on-chain verify timed out after {}ms",
                    self.onchain_verify_timeout.as_millis()
                )))
            })??;

            let dev = Self::deviation_bps(route.expected_output, onchain);
            if dev > self.max_quote_deviation_bps {
                return Err(Error::Api(api::Error::InvalidRoute(format!(
                    "API/on-chain quote deviation {dev}bps exceeds max {}bps (api={}, chain={})",
                    self.max_quote_deviation_bps, route.expected_output, onchain,
                ))));
            }
            onchain
        };

        Ok(Self::build_executable_quote(
            self.router_address,
            self.slippage_bps,
            &route,
            anchor_output,
            req.receiver,
            req.sell_amount,
        ))
    }
}

#[cfg(test)]
mod tests {
    use {super::*, alloy::primitives::Address};

    fn dummy_route(expected_output: eth::U256) -> api::Route {
        let mut route_arr = [eth::Address::ZERO; 11];
        route_arr[0] = Address::repeat_byte(1);
        route_arr[1] = Address::repeat_byte(0xAA);
        route_arr[2] = Address::repeat_byte(2);
        api::Route {
            route: route_arr,
            swap_params: [[1, 0, 6, 30, 3], [0; 5], [0; 5], [0; 5], [0; 5]],
            pools: [eth::Address::ZERO; 5],
            expected_output,
        }
    }

    #[test]
    fn build_executable_quote_returns_calldata_and_anchored_output() {
        let router_address = Address::repeat_byte(0xCA);
        let route = dummy_route(eth::U256::from(1_000_000u64));
        let receiver = Address::repeat_byte(0xEE);

        let quote = LegacyProvider::build_executable_quote(
            router_address,
            100, // 1% slippage
            &route,
            eth::U256::from(1_000_000u64),
            receiver,
            eth::U256::from(500_000u64),
        );

        assert_eq!(quote.expected_output, eth::U256::from(1_000_000u64));
        assert_eq!(quote.router_address, router_address);
        assert!(!quote.calldata.is_empty());
        assert!(quote.gas_estimate.is_none());
        assert!(quote.quality.is_none());
    }

    #[test]
    fn deviation_bps_computes_symmetric_difference() {
        let a = eth::U256::from(1_000_000u64);
        let b = eth::U256::from(990_000u64);
        assert_eq!(LegacyProvider::deviation_bps(a, b), 100);
        assert_eq!(LegacyProvider::deviation_bps(b, a), 100);
    }

    #[test]
    fn deviation_bps_handles_zero_outputs() {
        assert_eq!(
            LegacyProvider::deviation_bps(eth::U256::ZERO, eth::U256::from(1u64)),
            0
        );
        assert_eq!(
            LegacyProvider::deviation_bps(eth::U256::from(1u64), eth::U256::ZERO),
            0
        );
    }
}
