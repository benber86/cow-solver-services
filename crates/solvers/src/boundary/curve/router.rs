//! Curve Router contract interface for on-chain quote verification.

use {
    crate::domain::{curve::api::Route, eth},
    alloy::{
        primitives::{Address, U256},
        sol,
        sol_types::SolCall,
    },
    std::fmt,
};

/// Curve Router contract address on mainnet (v1.2).
pub const ROUTER_ADDRESS: Address = alloy::primitives::address!("45312ea0eFf7E09C83CBE249fa1d7598c4C8cd4e");

// Define the Curve Router contract interface using alloy's sol! macro
sol! {
    #[derive(Debug)]
    interface ICurveRouter {
        /// Get the expected output amount for a route.
        function get_dy(
            address[11] memory _route,
            uint256[5][5] memory _swap_params,
            uint256 _amount,
            address[5] memory _pools
        ) external view returns (uint256);

        /// Execute a swap through the router.
        function exchange(
            address[11] memory _route,
            uint256[5][5] memory _swap_params,
            uint256 _amount,
            uint256 _min_dy,
            address[5] memory _pools,
            address _receiver
        ) external payable returns (uint256);
    }
}

/// Encodes a `get_dy` call for on-chain quote verification.
pub fn encode_get_dy(route: &Route, amount: eth::U256) -> Vec<u8> {
    let call = ICurveRouter::get_dyCall {
        _route: route.route,
        _swap_params: convert_swap_params(&route.swap_params),
        _amount: amount,
        _pools: route.pools,
    };
    call.abi_encode()
}

/// Encodes an `exchange` call for the settlement.
pub fn encode_exchange(
    route: &Route,
    amount: eth::U256,
    min_dy: eth::U256,
    receiver: eth::Address,
) -> Vec<u8> {
    let call = ICurveRouter::exchangeCall {
        _route: route.route,
        _swap_params: convert_swap_params(&route.swap_params),
        _amount: amount,
        _min_dy: min_dy,
        _pools: route.pools,
        _receiver: receiver,
    };
    call.abi_encode()
}

/// Decodes the result of a `get_dy` call.
pub fn decode_get_dy_result(data: &[u8]) -> Result<eth::U256, DecodeError> {
    let result = ICurveRouter::get_dyCall::abi_decode_returns(data)
        .map_err(|e| DecodeError(e.to_string()))?;
    Ok(result)
}

/// Convert swap params from u64 arrays to U256 arrays as expected by the contract.
fn convert_swap_params(params: &[[u64; 5]; 5]) -> [[U256; 5]; 5] {
    let mut result = [[U256::ZERO; 5]; 5];
    for (i, row) in params.iter().enumerate() {
        for (j, val) in row.iter().enumerate() {
            result[i][j] = U256::from(*val);
        }
    }
    result
}

#[derive(Debug)]
pub struct DecodeError(String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_get_dy() {
        let route = Route {
            route: [Address::ZERO; 11],
            swap_params: [[0; 5]; 5],
            pools: [Address::ZERO; 5],
            expected_output: U256::ZERO,
        };

        let encoded = encode_get_dy(&route, U256::from(1000u64));
        // Should start with the function selector for get_dy
        assert!(!encoded.is_empty());
    }
}
