//! Constant product pool.

use crate::domain::eth;
use std::ops::Deref;

/// Uniswap-v2 like pool state.
pub struct Pool {
    pub reserves: Reserves,
    pub fee: eth::Rational,
}

/// Constant product reserves.
pub struct Reserves([eth::Asset; 2]);

impl Deref for Reserves {
    type Target = [eth::Asset; 2];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
