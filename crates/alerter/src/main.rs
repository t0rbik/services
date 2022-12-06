// This application observes the order book api and tries to determine if the solver is down. It
// does this by checking if no trades have been made recently and if so checking if it finds a
// matchable order according to an external price api (0x). If this is the case it alerts.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use model::{
    order::{OrderClass, OrderKind, OrderStatus, OrderUid, BUY_ETH_ADDRESS},
    u256_decimal,
};
use primitive_types::{H160, U256};
use prometheus::{IntGauge, IntGaugeVec};
use reqwest::Client;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use strum::VariantNames as _;
use url::Url;

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Order {
    kind: OrderKind,
    buy_token: H160,
    #[serde(with = "u256_decimal")]
    buy_amount: U256,
    sell_token: H160,
    #[serde(with = "u256_decimal")]
    sell_amount: U256,
    uid: OrderUid,
    status: OrderStatus,
    creation_date: DateTime<Utc>,
    partially_fillable: bool,
    is_liquidity_order: bool,
    #[serde(flatten)]
    class: OrderClass,
}

struct OrderBookApi {
    base: Url,
    client: Client,
}

impl OrderBookApi {
    pub fn new(client: Client, base_url: &str) -> Self {
        Self {
            base: base_url.parse().unwrap(),
            client,
        }
    }

    pub async fn solvable_orders(&self) -> reqwest::Result<Vec<Order>> {
        #[derive(serde::Deserialize)]
        struct Auction {
            orders: Vec<Order>,
        }
        let url = self.base.join("api/v1/auction").unwrap();
        let auction: Auction = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(auction.orders)
    }

    pub async fn order(&self, uid: &OrderUid) -> reqwest::Result<Order> {
        let url = self.base.join(&format!("api/v1/orders/{}", uid)).unwrap();
        self.client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }
}

// Converts the eth placeholder address to weth. Leaves other addresses untouched.
fn convert_eth_to_weth(token: H160) -> H160 {
    let weth: H160 = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse()
        .unwrap();
    if token == BUY_ETH_ADDRESS {
        weth
    } else {
        token
    }
}

struct ZeroExApi {
    base: Url,
    client: Client,
}

impl ZeroExApi {
    pub fn new(client: Client) -> Self {
        Self {
            base: "https://api.0x.org".parse().unwrap(),
            client,
        }
    }

    pub async fn can_be_settled(&self, order: &Order) -> Result<bool> {
        let mut url = self.base.join("swap/v1/price").unwrap();
        let (amount_name, amount) = match order.kind {
            OrderKind::Buy => ("buyAmount", order.buy_amount),
            OrderKind::Sell => ("sellAmount", order.sell_amount),
        };
        let buy_token = convert_eth_to_weth(order.buy_token);
        url.query_pairs_mut()
            .append_pair("sellToken", &format!("{:#x}", order.sell_token))
            .append_pair("buyToken", &format!("{:#x}", buy_token))
            .append_pair(amount_name, &amount.to_string());

        #[derive(Debug, serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct Response {
            #[serde(with = "u256_decimal")]
            pub sell_amount: U256,
            #[serde(with = "u256_decimal")]
            pub buy_amount: U256,
        }

        let response: Response = self
            .client
            .get(url.clone())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        tracing::debug!(url = url.as_str(), ?response, "0x");

        Ok(response.sell_amount <= order.sell_amount && response.buy_amount >= order.buy_amount)
    }
}

struct Alerter {
    orderbook_api: OrderBookApi,
    zeroex_api: ZeroExApi,
    config: AlertConfig,
    last_observed_trade: Instant,
    last_alert: Option<Instant>,
    // order and for how long it has been matchable
    open_orders: Vec<(Order, Option<Instant>)>,
    metrics: &'static Metrics,
}

struct AlertConfig {
    // Alert if no trades have been observed for this long.
    time_without_trade: Duration,
    // Give the solver some time to settle an order after it has become solvable before we alert.
    min_order_solvable_time: Duration,
    // Do not alert more often than this.
    min_alert_interval: Duration,
}

#[derive(prometheus_metric_storage::MetricStorage)]
pub struct Metrics {
    /// Set to 0 or 1 depending on whether our alert condition is satisfied
    /// which is that there hasn't been a trade for some time and that there is
    /// an order that has been matchable for some time.
    no_trades_but_matchable_order: IntGauge,

    /// Gauge for counting the number of matchable but open trades in the
    /// orderbook.
    #[metric(labels("class"))]
    matchable_orders_count: IntGaugeVec,
}

impl Alerter {
    pub fn new(orderbook_api: OrderBookApi, zeroex_api: ZeroExApi, config: AlertConfig) -> Self {
        Self {
            orderbook_api,
            zeroex_api,
            config,
            last_observed_trade: Instant::now(),
            last_alert: None,
            open_orders: Vec::new(),
            metrics: Metrics::instance(global_metrics::get_metric_storage_registry()).unwrap(),
        }
    }

    async fn update_open_orders(&mut self) -> Result<()> {
        let mut orders = self
            .orderbook_api
            .solvable_orders()
            .await
            .context("solvable_orders")?
            .into_iter()
            .filter(|order| !order.is_liquidity_order && !order.partially_fillable)
            .map(|order| {
                let existing_time = self
                    .open_orders
                    .iter()
                    .find(|(order_, _)| order_.uid == order.uid)
                    .and_then(|o| o.1);
                (order, existing_time)
            })
            .collect::<Vec<_>>();

        tracing::debug!("found {} open orders", orders.len());

        std::mem::swap(&mut self.open_orders, &mut orders);
        // Keep only orders that were open last update and are not open this update.
        orders.retain(|(order, _)| {
            self.open_orders
                .iter()
                .all(|(open_order, _)| open_order.uid != order.uid)
        });
        for closed_order in orders {
            let order = self.orderbook_api.order(&closed_order.0.uid).await?;
            if order.status == OrderStatus::Fulfilled {
                tracing::debug!(
                    "updating last observed trade because order {} was fulfilled",
                    order.uid
                );
                self.last_observed_trade = Instant::now();
                break;
            }
        }

        tracing::debug!("found no fulfilled orders");
        Ok(())
    }

    pub async fn update(&mut self) -> Result<()> {
        self.update_open_orders().await?;
        let matchable_orders = self.matchable_orders().await?;

        let counts = matchable_orders.iter().fold(
            OrderClass::VARIANTS
                .iter()
                .map(|class| (*class, 0_i64))
                .collect::<HashMap<_, _>>(),
            |mut counts, (order, _)| {
                *counts.get_mut(order.class.as_ref()).unwrap() += 1;
                counts
            },
        );
        for (class, count) in counts {
            self.metrics
                .matchable_orders_count
                .with_label_values(&[class])
                .set(count);
        }

        if self.last_observed_trade.elapsed() <= self.config.time_without_trade {
            self.metrics.no_trades_but_matchable_order.set(0);
            // Delete all matchable timestamps.
            //
            // If we didn't do this what could happen is that first we mark an order as matchable
            // at t0. Then a trade happens so we skip the matchable update loop below because if
            // there was a recent trade we don't want to alert anyway. Then no trade happens for
            // long enough that we want to alert and the order is again matchable.
            // In this case we would alert immediately even though it could be the case that the
            // order wasn't matchable and just now became matchable again. We would wrongly assume
            // it has been matchable since t0 but we did not check this between now and then.
            for (_, instant) in &mut self.open_orders {
                *instant = None;
            }
            return Ok(());
        }

        if let Some((order, _)) = matchable_orders
            .into_iter()
            .find(|(_, solvable)| *solvable > self.config.min_order_solvable_time)
        {
            let should_alert = match self.last_alert {
                None => true,
                Some(instant) => instant.elapsed() >= self.config.min_alert_interval,
            };
            if should_alert {
                self.last_alert = Some(Instant::now());
                self.alert(&order);
            }

            self.metrics.no_trades_but_matchable_order.set(1);
        } else {
            self.metrics.no_trades_but_matchable_order.set(0);
        }

        Ok(())
    }

    async fn matchable_orders(&mut self) -> Result<Vec<(Order, Duration)>> {
        let mut result = Vec::new();
        for (order, solvable_since) in &mut self.open_orders {
            let can_be_settled = self
                .zeroex_api
                .can_be_settled(order)
                .await
                .context("can_be_settled")?;

            let now = Instant::now();
            if can_be_settled {
                let solvable_since = *solvable_since.get_or_insert(now);
                result.push((order.clone(), now.duration_since(solvable_since)));
            } else {
                *solvable_since = None;
            }
        }

        Ok(result)
    }

    fn alert(&self, order: &Order) {
        tracing::error!(
            "No orders have been settled in the last {} seconds \
             even though order {} is solvable and has a price that \
             allows it to be settled according to 0x.",
            self.config.time_without_trade.as_secs(),
            order.uid,
        );
    }
}

#[derive(Debug, Parser)]
struct Arguments {
    /// Alerter update interval.
    #[clap(
        long,
        env,
        default_value = "30",
        value_parser = shared::arguments::duration_from_seconds,
    )]
    update_interval: Duration,

    /// Minimum time without a trade before alerting.
    #[clap(
        long,
        env,
        default_value = "600",
        value_parser = shared::arguments::duration_from_seconds,
    )]
    time_without_trade: Duration,

    /// Minimum time an order must have been matchable for before alerting.
    #[clap(
        long,
        env,
        default_value = "180",
        value_parser = shared::arguments::duration_from_seconds,
    )]
    min_order_age: Duration,

    /// Do not repeat the alert more often than this.
    #[clap(
        long,
        env,
        default_value = "1800",
        value_parser = shared::arguments::duration_from_seconds,
    )]
    min_alert_interval: Duration,

    /// How many errors in the update loop (fetching solvable orders or querying 0x) in a row
    /// must happen before we alert about them.
    #[clap(long, env, default_value = "5")]
    errors_in_a_row_before_alert: u32,

    #[clap(long, env, default_value = "https://api.cow.fi/mainnet/")]
    orderbook_api: String,

    #[clap(long, env, default_value = "9588")]
    metrics_port: u16,
}

#[tokio::main]
async fn main() {
    let args = Arguments::parse();
    shared::tracing::initialize("alerter=debug", tracing::Level::ERROR.into());
    shared::exit_process_on_panic::set_panic_hook();
    tracing::info!("running alerter with {:#?}", args);

    global_metrics::setup_metrics_registry(Some("gp_v2_alerter".to_string()), None);
    let filter = shared::metrics::handle_metrics();
    tokio::task::spawn(warp::serve(filter).bind(([0, 0, 0, 0], args.metrics_port)));

    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let mut alerter = Alerter::new(
        OrderBookApi::new(client.clone(), &args.orderbook_api),
        ZeroExApi::new(client),
        AlertConfig {
            time_without_trade: args.time_without_trade,
            min_order_solvable_time: args.min_order_age,
            min_alert_interval: args.min_alert_interval,
        },
    );

    let mut errors_in_a_row = 0;
    loop {
        match alerter.update().await {
            Ok(()) => errors_in_a_row = 0,
            Err(err) if errors_in_a_row < args.errors_in_a_row_before_alert => {
                errors_in_a_row += 1;
                tracing::warn!(?err, "alerter update error");
            }
            Err(err) => {
                errors_in_a_row = 0;
                tracing::error!(?err, "alerter update error");
            }
        }
        tokio::time::sleep(args.update_interval).await;
    }
}
