pub mod exit;

use anyhow::Result;
use redb::{Database, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

use crate::polymarket::client::PolymarketClient;
use crate::strategy::TradeIntent;

const ORDERS: TableDefinition<&str, &[u8]> = TableDefinition::new("orders");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrderState {
    /// Persisted before the order goes out so a crash mid-flight can be
    /// reconciled without double-submitting.
    Pending,
    Submitted { order_id: String },
    Filled { order_id: String, size: String },
    Rejected { reason: String },
}

pub struct Executor {
    db: Arc<Database>,
    poly: Arc<PolymarketClient>,
}

impl Executor {
    pub fn open(db_path: &Path, poly: Arc<PolymarketClient>) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(db_path)?;
        {
            let tx = db.begin_write()?;
            tx.open_table(ORDERS)?; // create if absent
            tx.commit()?;
        }
        Ok(Self { db: Arc::new(db), poly })
    }

    /// Idempotent submit: persist the client_order_id as Pending, hand the
    /// intent to the Polymarket SDK, then persist the response. On restart,
    /// callers should call `load(client_order_id)` for any pending IDs and
    /// reconcile against the CLOB before submitting a fresh order.
    pub async fn submit(
        &self,
        client_order_id: &str,
        intent: &TradeIntent,
    ) -> Result<OrderState> {
        self.persist(client_order_id, &OrderState::Pending)?;
        let state = match self.poly.submit_intent(intent).await {
            Ok(r) if r.success => {
                let id = r.order_id.unwrap_or_default();
                info!(client_order_id, order_id = %id, "order submitted");
                OrderState::Submitted { order_id: id }
            }
            Ok(r) => {
                let reason = r.error.unwrap_or_else(|| "unknown".into());
                warn!(client_order_id, reason, "order rejected by CLOB");
                OrderState::Rejected { reason }
            }
            Err(e) => {
                let reason = format!("submit error: {e}");
                warn!(client_order_id, reason, "order submit failed");
                OrderState::Rejected { reason }
            }
        };
        self.persist(client_order_id, &state)?;
        Ok(state)
    }

    fn persist(&self, client_order_id: &str, state: &OrderState) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(ORDERS)?;
            let value = serde_json::to_vec(state)?;
            t.insert(client_order_id, value.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load(&self, client_order_id: &str) -> Result<Option<OrderState>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(ORDERS)?;
        let v = t.get(client_order_id)?;
        match v {
            Some(g) => Ok(Some(serde_json::from_slice(g.value())?)),
            None => Ok(None),
        }
    }
}
