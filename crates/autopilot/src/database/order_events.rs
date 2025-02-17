pub use database::order_events::OrderEventLabel;
use {
    anyhow::{Context, Result},
    chrono::{DateTime, Utc},
    database::{
        byte_array::ByteArray,
        order_events::{self, OrderEvent},
    },
    model::order::OrderUid,
};

impl super::Postgres {
    /// Inserts the given events with the current timestamp into the DB.
    /// If this function encounters an error it will only be printed. More
    /// elaborate error handling is not necessary because this is just
    /// debugging information.
    pub async fn store_order_events(&self, events: &[(OrderUid, OrderEventLabel)]) {
        if let Err(err) = store_order_events(self, events, Utc::now()).await {
            tracing::warn!(?err, "failed to insert order events");
        }
    }
}

async fn store_order_events(
    db: &super::Postgres,
    events: &[(OrderUid, OrderEventLabel)],
    timestamp: DateTime<Utc>,
) -> Result<()> {
    let mut ex = db.0.begin().await.context("begin transaction")?;
    for (uid, label) in events {
        let event = OrderEvent {
            order_uid: ByteArray(uid.0),
            timestamp,
            label: *label,
        };

        order_events::insert_order_event(&mut ex, &event).await?
    }
    ex.commit().await?;
    Ok(())
}
