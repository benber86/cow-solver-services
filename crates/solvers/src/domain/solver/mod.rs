//! Solver implementations.

mod baseline;
pub mod curve_lp;

pub use baseline::{Config, Request, Route, Segment};

use crate::domain::{auction, solution};

/// A solver that can handle auctions.
pub enum Solver {
    /// Baseline solver using on-chain liquidity.
    Baseline(baseline::Solver),
    /// Curve LP token solver.
    CurveLp(curve_lp::Solver),
}

impl Solver {
    /// Creates a new baseline solver.
    pub async fn new(config: Config) -> Self {
        Self::Baseline(baseline::Solver::new(config).await)
    }

    /// Creates a new Curve LP solver.
    pub async fn new_curve_lp(config: curve_lp::Config) -> Self {
        Self::CurveLp(curve_lp::Solver::new(config).await)
    }

    /// Solves the auction.
    pub async fn solve(&self, auction: auction::Auction) -> Vec<solution::Solution> {
        match self {
            Solver::Baseline(solver) => solver.solve(auction).await,
            Solver::CurveLp(solver) => solver.solve(auction).await,
        }
    }
}
