use {
    super::OnchainComponents,
    crate::setup::{wait_for_condition, TIMEOUT},
    clap::Parser,
    docker::db::Db,
    ethcontract::{H160, H256},
    model::{
        app_data::{AppDataDocument, AppDataHash},
        auction::AuctionWithId,
        order::{Order, OrderCreation, OrderUid},
        quote::{OrderQuoteRequest, OrderQuoteResponse},
        solver_competition::SolverCompetitionAPI,
        trade::Trade,
    },
    reqwest::{Client, StatusCode, Url},
    std::time::Duration,
};

pub const ORDERS_ENDPOINT: &str = "api/v1/orders";
pub const QUOTING_ENDPOINT: &str = "api/v1/quote";
pub const ACCOUNT_ENDPOINT: &str = "api/v1/account";
pub const AUCTION_ENDPOINT: &str = "api/v1/auction";
pub const TRADES_ENDPOINT: &str = "api/v1/trades";
pub const VERSION_ENDPOINT: &str = "api/v1/version";
pub const SOLVER_COMPETITION_ENDPOINT: &str = "api/v1/solver_competition";

/// Wrapper over offchain services.
/// Exposes various utility methods for tests.
pub struct Services<'a> {
    onchain: &'a OnchainComponents,
    http: Client,
    db: Db,
    api_url: once_cell::sync::OnceCell<Url>,
}

impl<'a> Services<'a> {
    pub async fn new(onchain: &'a OnchainComponents, db: Db) -> Services<'a> {
        Self {
            onchain,
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            db,
            api_url: Default::default(),
        }
    }

    fn api_autopilot_arguments(&self) -> impl Iterator<Item = String> {
        [
            "--price-estimators=Baseline|0x0000000000000000000000000000000000000001".to_string(),
            "--native-price-estimators=Baseline".to_string(),
            "--amount-to-estimate-prices-with=1000000000000000000".to_string(),
            "--block-stream-poll-interval-seconds=1".to_string(),
            format!("--db-url={}", self.db.url().as_str()),
        ]
        .into_iter()
    }

    fn api_autopilot_solver_arguments(&self) -> impl Iterator<Item = String> {
        let node_url = format!("http://localhost:{}", self.onchain.rpc_port());
        [
            "--baseline-sources=None".to_string(),
            "--network-block-interval=1".to_string(),
            "--solver-competition-auth=super_secret_key".to_string(),
            format!(
                "--custom-univ2-baseline-sources={:?}|{:?}",
                self.onchain.contracts().uniswap_v2_router.address(),
                H256(shared::sources::uniswap_v2::UNISWAP_INIT),
            ),
            format!(
                "--settlement-contract-address={:?}",
                self.onchain.contracts().gp_settlement.address()
            ),
            format!(
                "--native-token-address={:?}",
                self.onchain.contracts().weth.address()
            ),
            format!(
                "--balancer-v2-vault-address={:?}",
                self.onchain.contracts().balancer_vault.address()
            ),
            format!("--node-url={node_url}"),
            format!("--simulation-node-url={node_url}"),
        ]
        .into_iter()
    }

    /// Start the autopilot service in a background task.
    pub fn start_autopilot(&self, extra_args: Vec<String>) {
        let args = [
            "autopilot".to_string(),
            "--auction-update-interval=1".to_string(),
            format!(
                "--ethflow-contract={:?}",
                self.onchain.contracts().ethflow.address()
            ),
            "--skip-event-sync=true".to_string(),
            "--solve-deadline=2".to_string(),
            "--metrics-address=127.0.0.1:0".to_string(),
        ]
        .into_iter()
        .chain(self.api_autopilot_solver_arguments())
        .chain(self.api_autopilot_arguments())
        .chain(extra_args);

        let args = autopilot::arguments::Arguments::try_parse_from(args).unwrap();
        tokio::task::spawn(autopilot::run(args));
    }

    /// Start the api service in a background tasks.
    /// Wait until the service is responsive.
    pub async fn start_api(&self, extra_args: Vec<String>) {
        let args = [
            "orderbook".to_string(),
            "--enable-presign-orders=true".to_string(),
            "--enable-eip1271-orders=true".to_string(),
            "--bind-address=127.0.0.1:0".to_string(),
            "--metrics-address=127.0.0.1:0".to_string(),
            format!(
                "--hooks-contract-address={:?}",
                self.onchain.contracts().hooks.address()
            ),
        ]
        .into_iter()
        .chain(self.api_autopilot_solver_arguments())
        .chain(self.api_autopilot_arguments())
        .chain(extra_args.into_iter());

        let args = orderbook::arguments::Arguments::try_parse_from(args).unwrap();
        let (bind, bind_receiver) = tokio::sync::oneshot::channel();
        tokio::task::spawn(orderbook::run(args, Some(bind)));
        let api_addr = bind_receiver.await.unwrap();
        self.api_url
            .set(format!("http://{api_addr}").parse().unwrap())
            .unwrap();

        self.wait_for_api_to_come_up().await;
    }

    /// Start the solver service in a background task.
    pub fn start_old_driver(&self, private_key: &[u8; 32], extra_args: Vec<String>) {
        let args = [
            "solver".to_string(),
            format!("--solver-account={}", hex::encode(private_key)),
            "--settle-interval=1".to_string(),
            "--metrics-port=0".to_string(),
            format!(
                "--transaction-submission-nodes=http://localhost:{}",
                self.onchain.rpc_port()
            ),
            format!(
                "--ethflow-contract={:?}",
                self.onchain.contracts().ethflow.address()
            ),
            format!("--orderbook-url={}", self.api_url().as_str()),
        ]
        .into_iter()
        .chain(self.api_autopilot_solver_arguments())
        .chain(extra_args);

        let args = solver::arguments::Arguments::try_parse_from(args).unwrap();
        tokio::task::spawn(solver::run::run(args));
    }

    /// Start the solver service in a background task with a custom http solver
    /// only.
    pub fn start_old_driver_custom_solver(
        &self,
        solver_url: Option<Url>,
        solver_account: H160,
        extra_args: Vec<String>,
    ) {
        let args = [
            "solver".to_string(),
            format!(
                "--external-solvers=Custom|{}|{:#x}|false",
                solver_url
                    .unwrap_or("http://localhost:8000".parse().unwrap())
                    .as_str(),
                solver_account
            ),
            "--solvers=None".to_string(),
            format!("--solver-account={:#x}", solver_account),
            "--settle-interval=1".to_string(),
            format!(
                "--transaction-submission-nodes=http://localhost:{}",
                self.onchain.rpc_port()
            ),
            format!(
                "--ethflow-contract={:?}",
                self.onchain.contracts().ethflow.address()
            ),
        ]
        .into_iter()
        .chain(self.api_autopilot_solver_arguments())
        .chain(extra_args);

        let args = solver::arguments::Arguments::try_parse_from(args).unwrap();
        tokio::task::spawn(solver::run(args));
    }

    async fn wait_for_api_to_come_up(&self) {
        let is_up = || async {
            reqwest::get(format!("{}{AUCTION_ENDPOINT}", self.api_url().as_str()))
                .await
                .is_ok()
        };

        tracing::info!("Waiting for API to come up.");
        wait_for_condition(TIMEOUT, is_up)
            .await
            .expect("waiting for API timed out");
    }

    pub async fn get_auction(&self) -> AuctionWithId {
        let response = self
            .http
            .get(format!("{}{AUCTION_ENDPOINT}", self.api_url().as_str()))
            .send()
            .await
            .unwrap();

        let status = response.status();
        let body = response.text().await.unwrap();

        assert_eq!(status, StatusCode::OK, "{body}");

        serde_json::from_str(&body).unwrap()
    }

    pub async fn get_solver_competition(
        &self,
        hash: H256,
    ) -> Result<SolverCompetitionAPI, StatusCode> {
        let response = self
            .http
            .get(format!(
                "{}{SOLVER_COMPETITION_ENDPOINT}/by_tx_hash/{hash:?}",
                self.api_url().as_str()
            ))
            .send()
            .await
            .unwrap();

        let status = response.status();
        let body = response.text().await.unwrap();

        match status {
            StatusCode::OK => Ok(serde_json::from_str(&body).unwrap()),
            code => Err(code),
        }
    }

    pub async fn get_trades(&self, order: &OrderUid) -> Result<Vec<Trade>, StatusCode> {
        let url = format!("{}api/v1/trades?orderUid={order}", self.api_url().as_str());
        let response = self.http.get(url).send().await.unwrap();

        let status = response.status();
        let body = response.text().await.unwrap();

        match status {
            StatusCode::OK => Ok(serde_json::from_str(&body).unwrap()),
            code => Err(code),
        }
    }

    /// Create an [`Order`].
    /// If the response status code is not `201`, return the status and the
    /// body.
    pub async fn create_order(
        &self,
        order: &OrderCreation,
    ) -> Result<OrderUid, (StatusCode, String)> {
        let placement = self
            .http
            .post(format!("{}{ORDERS_ENDPOINT}", self.api_url().as_str()))
            .json(order)
            .send()
            .await
            .unwrap();

        let status = placement.status();
        let body = placement.text().await.unwrap();

        match status {
            StatusCode::CREATED => Ok(serde_json::from_str(&body).unwrap()),
            code => Err((code, body)),
        }
    }

    /// Submit an [`model::quote::OrderQuote`].
    /// If the response status is not `200`, return the status and the body.
    pub async fn submit_quote(
        &self,
        quote: &OrderQuoteRequest,
    ) -> Result<OrderQuoteResponse, (StatusCode, String)> {
        let quoting = self
            .http
            .post(&format!("{}{QUOTING_ENDPOINT}", self.api_url().as_str()))
            .json(&quote)
            .send()
            .await
            .unwrap();

        let status = quoting.status();
        let body = quoting.text().await.unwrap();

        match status {
            StatusCode::OK => Ok(serde_json::from_str(&body).unwrap()),
            code => Err((code, body)),
        }
    }

    pub async fn solvable_orders(&self) -> usize {
        self.get_auction().await.auction.orders.len()
    }

    /// Retrieve an [`Order`]. If the respons status is not `200`, return the
    /// status and the body.
    pub async fn get_order(&self, uid: &OrderUid) -> Result<Order, (StatusCode, String)> {
        let response = self
            .http
            .get(format!(
                "{}{ORDERS_ENDPOINT}/{uid}",
                self.api_url().as_str()
            ))
            .send()
            .await
            .unwrap();

        let status = response.status();
        let body = response.text().await.unwrap();

        match status {
            StatusCode::OK => Ok(serde_json::from_str(&body).unwrap()),
            code => Err((code, body)),
        }
    }

    pub async fn get_app_data_document(
        &self,
        app_data: AppDataHash,
    ) -> Result<AppDataDocument, (StatusCode, String)> {
        let response = self
            .http
            .get(format!(
                "{}api/v1/app_data/{app_data:?}",
                self.api_url().as_str()
            ))
            .send()
            .await
            .unwrap();

        let status = response.status();
        let body = response.text().await.unwrap();

        match status {
            StatusCode::OK => Ok(serde_json::from_str(&body).unwrap()),
            code => Err((code, body)),
        }
    }

    pub async fn get_app_data(
        &self,
        app_data: AppDataHash,
    ) -> Result<String, (StatusCode, String)> {
        Ok(self.get_app_data_document(app_data).await?.full_app_data)
    }

    pub async fn put_app_data_document(
        &self,
        app_data: AppDataHash,
        document: AppDataDocument,
    ) -> Result<(), (StatusCode, String)> {
        let response = self
            .http
            .put(format!(
                "{}api/v1/app_data/{app_data:?}",
                self.api_url().as_str()
            ))
            .json(&document)
            .send()
            .await
            .unwrap();

        let status = response.status();
        let body = response.text().await.unwrap();

        if status.is_success() {
            Ok(())
        } else {
            Err((status, body))
        }
    }

    pub async fn put_app_data(
        &self,
        app_data: AppDataHash,
        full_app_data: &str,
    ) -> Result<(), (StatusCode, String)> {
        self.put_app_data_document(
            app_data,
            AppDataDocument {
                full_app_data: full_app_data.to_owned(),
            },
        )
        .await
    }

    pub fn client(&self) -> &Client {
        &self.http
    }

    /// Returns the underlying postgres connection pool that can be used do
    /// execute raw SQL queries.
    pub fn db(&self) -> &DbConnection {
        self.db.connection()
    }

    pub fn api_url(&self) -> Url {
        self.api_url.get().expect("api already initialized").clone()
    }
}

pub type DbConnection = sqlx::Pool<sqlx::Postgres>;
