use {
    crate::domain::competition::{self, solution::settlement},
    serde::Serialize,
    serde_with::{serde_as, DisplayFromStr},
};

impl Solution {
    pub fn from_domain(id: settlement::Id, score: competition::Score) -> Self {
        Self {
            id: id.into(),
            score: score.into(),
        }
    }
}

#[serde_as]
#[derive(Debug, Serialize)]
pub struct Solution {
    #[serde_as(as = "DisplayFromStr")]
    id: u64,
    score: f64,
}
