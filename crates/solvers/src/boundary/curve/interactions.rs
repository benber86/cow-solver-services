//! Build CustomInteraction for Curve Router exchange calls.

use {
    crate::{
        boundary::curve::router::{self, ROUTER_ADDRESS},
        domain::{curve::api::Route, eth, solution},
    },
};

/// Builds a CustomInteraction for executing a swap through the Curve Router.
pub fn build_exchange_interaction(
    route: &Route,
    sell_token: eth::TokenAddress,
    sell_amount: eth::U256,
    buy_token: eth::TokenAddress,
    min_output: eth::U256,
    receiver: eth::Address,
) -> solution::CustomInteraction {
    let calldata = router::encode_exchange(route, sell_amount, min_output, receiver);

    solution::CustomInteraction {
        target: ROUTER_ADDRESS,
        value: eth::Ether(eth::U256::ZERO),
        calldata,
        internalize: false,
        inputs: vec![eth::Asset {
            token: sell_token,
            amount: sell_amount,
        }],
        outputs: vec![eth::Asset {
            token: buy_token,
            amount: min_output,
        }],
        allowances: vec![solution::Allowance {
            spender: ROUTER_ADDRESS,
            asset: eth::Asset {
                token: sell_token,
                amount: sell_amount,
            },
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn test_build_exchange_interaction() {
        let route = Route {
            route: [Address::ZERO; 11],
            swap_params: [[0; 5]; 5],
            pools: [Address::ZERO; 5],
            expected_output: eth::U256::from(1000u64),
        };

        let sell_token = eth::TokenAddress(Address::repeat_byte(1));
        let buy_token = eth::TokenAddress(Address::repeat_byte(2));
        let receiver = Address::repeat_byte(3);

        let interaction = build_exchange_interaction(
            &route,
            sell_token,
            eth::U256::from(1000u64),
            buy_token,
            eth::U256::from(990u64),
            receiver,
        );

        assert_eq!(interaction.target, ROUTER_ADDRESS);
        assert_eq!(interaction.inputs.len(), 1);
        assert_eq!(interaction.outputs.len(), 1);
        assert_eq!(interaction.allowances.len(), 1);
        assert_eq!(interaction.inputs[0].token, sell_token);
        assert_eq!(interaction.outputs[0].token, buy_token);
    }
}
