use crate::tests::{
    setup,
    setup::{ab_order, ab_pool, ab_solution},
};

/// Test that the best-scoring solution is picked when the /solve endpoint
/// returns multiple valid solutions.
#[tokio::test]
#[ignore]
async fn valid() {
    let test = setup()
        .pool(ab_pool())
        .order(ab_order())
        .solution(ab_solution())
        .solution(ab_solution().reduce_score())
        .done()
        .await;

    test.solve().await.ok().default_score();
    test.reveal().await.ok().orders(&[ab_order().name]);
}

/// Test that the invalid solution is discarded when the /solve endpoint
/// returns multiple solutions.
#[tokio::test]
#[ignore]
async fn invalid() {
    let test = setup()
        .pool(ab_pool())
        .order(ab_order())
        .solution(ab_solution().reduce_score())
        .solution(ab_solution().invalid())
        .done()
        .await;

    test.solve().await.ok().default_score();
    test.reveal().await.ok().orders(&[ab_order().name]);
}
