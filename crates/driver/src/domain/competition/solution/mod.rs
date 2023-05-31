use {
    crate::{
        boundary,
        domain::{
            competition::{self, order},
            eth,
        },
        infra::{
            self,
            blockchain::{self, Ethereum},
            simulator,
            solver::Solver,
            time,
            Simulator,
        },
    },
    futures::future::try_join_all,
    itertools::Itertools,
    std::collections::HashMap,
};

pub mod interaction;
pub mod settlement;
pub mod trade;

pub use {interaction::Interaction, settlement::Settlement, trade::Trade};

/// A solution represents a set of orders which the solver has found an optimal
/// way to settle. A [`Solution`] is generated by a solver as a response to a
/// [`competition::Auction`].
#[derive(Debug, Clone)]
pub struct Solution {
    pub id: Id,
    /// Trades settled by this solution.
    pub trades: Vec<Trade>,
    pub prices: ClearingPrices,
    pub interactions: Vec<Interaction>,
    pub weth: eth::WethAddress,
    /// The solver which generated this solution.
    pub solver: Solver,
    pub risk: Risk,
}

impl Solution {
    /// Approval interactions necessary for encoding the settlement.
    pub async fn approvals(
        &self,
        eth: &Ethereum,
    ) -> Result<impl Iterator<Item = eth::allowance::Approval>, Error> {
        let settlement_contract = &eth.contracts().settlement();
        let allowances = try_join_all(self.allowances().map(|required| async move {
            eth.allowance(settlement_contract.address().into(), required.0.spender)
                .await
                .map(|existing| (required, existing))
        }))
        .await?;
        let approvals = allowances.into_iter().filter_map(|(required, existing)| {
            required
                .approval(&existing)
                // As a gas optimization, we always approve the max amount possible. This minimizes
                // the number of approvals necessary, and therefore minimizes the approval fees over time. This is a
                // potential security issue, but its effects are minimized and only exploitable if
                // solvers use insecure contracts.
                .map(eth::allowance::Approval::max)
        });
        Ok(approvals)
    }

    /// An empty solution has no user trades and a score of 0.
    pub fn is_empty(&self) -> bool {
        self.user_trades().next().is_none()
    }

    /// Return the trades which fulfill non-liquidity auction orders. These are
    /// the orders placed by end users.
    fn user_trades(&self) -> impl Iterator<Item = &trade::Fulfillment> {
        self.trades.iter().filter_map(|trade| match trade {
            Trade::Fulfillment(fulfillment) => match fulfillment.order().kind {
                order::Kind::Market | order::Kind::Limit { .. } => Some(fulfillment),
                order::Kind::Liquidity => None,
            },
            Trade::Jit(_) => None,
        })
    }

    /// Return the allowances in a normalized form, where there is only one
    /// allowance per [`eth::allowance::Spender`], and they're ordered
    /// deterministically.
    fn allowances(&self) -> impl Iterator<Item = eth::allowance::Required> {
        let mut normalized = HashMap::new();
        // TODO: we need to carry the "internalize" flag with the allowances,
        // since we don't want to include approvals for interactions that are
        // meant to be internalized anyway.
        let allowances = self.interactions.iter().flat_map(Interaction::allowances);
        for allowance in allowances {
            let amount = normalized
                .entry(allowance.0.spender)
                .or_insert(eth::U256::zero());
            *amount = amount.saturating_add(allowance.0.amount);
        }
        normalized
            .into_iter()
            .map(|(spender, amount)| eth::Allowance { spender, amount }.into())
            .sorted()
    }

    /// Encode the solution into a [`Settlement`], which can be used to execute
    /// the solution onchain.
    pub async fn encode(
        self,
        auction: &competition::Auction,
        eth: &Ethereum,
        simulator: &Simulator,
    ) -> Result<Settlement, Error> {
        Settlement::encode(self, auction, eth, simulator).await
    }

    /// The clearing prices, represented as a list of assets. If there are any
    /// orders which buy ETH, this will contain the correct ETH price.
    pub fn prices(&self) -> Result<Vec<eth::Asset>, Error> {
        let prices = self
            .prices
            .0
            .iter()
            .map(|(&token, &amount)| eth::Asset { token, amount });

        if self.user_trades().any(|trade| trade.order().buys_eth()) {
            // The solution contains an order which buys ETH. Solvers only produce solutions
            // for ERC20 tokens, while the driver adds special [`Interaction`]s to
            // wrap/unwrap the ETH tokens into WETH, and sends orders to the solver with
            // WETH instead of ETH. Once the driver receives the solution which fulfills an
            // ETH order, a clearing price for ETH needs to be added, equal to the
            // WETH clearing price.

            // If no order trades WETH, the WETH price is not necessary, only the ETH
            // price is needed. Remove the unneeded WETH price, which slightly reduces
            // gas used by the settlement.
            let mut prices = if self.user_trades().all(|trade| {
                trade.order().sell.token != self.weth.0 && trade.order().buy.token != self.weth.0
            }) {
                prices
                    .filter(|price| price.token != self.weth.0)
                    .collect_vec()
            } else {
                prices.collect_vec()
            };

            // Add a clearing price for ETH equal to WETH.
            prices.push(eth::Asset {
                token: eth::ETH_TOKEN,
                amount: self
                    .prices
                    .0
                    .get(&self.weth.into())
                    .ok_or(Error::MissingWethClearingPrice)?
                    .to_owned(),
            });

            return Ok(prices);
        }

        // TODO: We should probably filter out all unused prices.

        Ok(prices.collect_vec())
    }

    /// Clearing price for the given token.
    pub fn price(&self, token: eth::TokenAddress) -> Option<eth::U256> {
        // The clearing price of ETH is equal to WETH.
        let token = token.wrap(self.weth);
        self.prices.0.get(&token).map(ToOwned::to_owned)
    }
}

/// Token prices for this solution, expressed using an arbitrary reference
/// unit chosen by the solver. These values are only meaningful in relation
/// to each others.
///
/// The rule which relates two prices for tokens X and Y is:
/// ```
/// amount_x * price_x = amount_y * price_y
/// ```
#[derive(Debug, Clone)]
pub struct ClearingPrices(HashMap<eth::TokenAddress, eth::U256>);

impl ClearingPrices {
    pub fn new(prices: HashMap<eth::TokenAddress, eth::U256>) -> Self {
        Self(prices)
    }
}

/// The time allocated for the solver to solve an auction.
#[derive(Debug, Clone, Copy)]
pub struct SolverTimeout(std::time::Duration);

impl From<std::time::Duration> for SolverTimeout {
    fn from(value: std::time::Duration) -> Self {
        Self(value)
    }
}

impl From<SolverTimeout> for std::time::Duration {
    fn from(value: SolverTimeout) -> Self {
        value.0
    }
}

impl SolverTimeout {
    /// The time limit passed to the solver for solving an auction.
    ///
    /// Solvers are given a time limit that's `buffer` less than the specified
    /// deadline. The reason for this is to allow the solver sufficient time to
    /// search for the most optimal solution, but still ensure there is time
    /// left for the driver to do some other necessary work and forward the
    /// results back to the protocol.
    pub fn new(
        deadline: chrono::DateTime<chrono::Utc>,
        buffer: chrono::Duration,
        now: time::Now,
    ) -> Option<SolverTimeout> {
        let deadline = deadline - now.now() - buffer;
        deadline.to_std().map(Self).ok()
    }

    pub fn deadline(self, now: infra::time::Now) -> chrono::DateTime<chrono::Utc> {
        now.now() + chrono::Duration::from_std(self.0).expect("reasonable solver timeout")
    }
}

/// The solution score. This is often referred to as the "objective value".
#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
pub struct Score(pub eth::U256);

impl From<Score> for eth::U256 {
    fn from(value: Score) -> Self {
        value.0
    }
}

impl From<eth::U256> for Score {
    fn from(value: eth::U256) -> Self {
        Self(value)
    }
}

/// Solver-estimated risk that the settlement might revert. This value is
/// subtracted from the final score of the solution.
#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
pub struct Risk(pub eth::U256);

impl From<Risk> for eth::U256 {
    fn from(value: Risk) -> Self {
        value.0
    }
}

impl From<eth::U256> for Risk {
    fn from(value: eth::U256) -> Self {
        Self(value)
    }
}

impl Risk {
    // TODO(#1533) Improve the risk merging formula. For now it's OK to simply add
    // the risks, since it causes the solvers to under-bid which is less
    // dangerous than over-bidding.
    /// Combine two risk values.
    pub fn merge(self, other: Risk) -> Self {
        Self(self.0 + other.0)
    }
}

/// A unique solution ID. This ID is generated by the solver and only needs to
/// be unique within a single round of competition. This ID is only important in
/// the communication between the driver and the solver, and it is not used by
/// the protocol.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct Id(pub u64);

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Id> for u64 {
    fn from(value: Id) -> Self {
        value.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("blockchain error: {0:?}")]
    Blockchain(#[from] blockchain::Error),
    #[error("boundary error: {0:?}")]
    Boundary(#[from] boundary::Error),
    #[error("missing weth clearing price")]
    MissingWethClearingPrice,
    #[error("simulation error: {0:?}")]
    Simulation(#[from] simulator::Error),
    #[error(
        "invalid asset flow: token amounts entering the settlement do not equal token amounts \
         exiting the settlement"
    )]
    AssetFlow,
    #[error(
        "invalid internalization: solution attempts to internalize tokens which are not trusted"
    )]
    UntrustedInternalization,
    #[error("invalid internalization: uninternalized solution fails to simulate")]
    FailingInternalization,
    #[error("insufficient solver account Ether balance")]
    InsufficientBalance,
    #[error("attempted to merge settlements generated by different solvers")]
    DifferentSolvers,
}
