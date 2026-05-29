//! Wrapper around `polymarket_client_sdk_v2::clob::Client`. Holds the
//! authenticated SDK client and the local signer; exposes one high-level
//! `submit_intent` method.

use std::str::FromStr;
use std::sync::Arc;

use alloy::signers::Signer;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Result, anyhow};
use rust_decimal::Decimal;
use tracing::info;

use polymarket_client_sdk_v2::POLYGON;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::clob::{Client as SdkClient, Config as SdkConfig};

type AuthClient = SdkClient<Authenticated<Normal>>;

use crate::config::PolymarketConfig;
use crate::strategy::{BuyOutcome, TradeIntent};

pub struct PolymarketClient {
    pub inner: Arc<AuthClient>,
    pub signer: PrivateKeySigner,
    pub over_token_id: String,
    pub under_token_id: String,
}

impl PolymarketClient {
    pub async fn new(
        cfg: &PolymarketConfig,
        signer: PrivateKeySigner,
        over_token_id: String,
        under_token_id: String,
    ) -> Result<Self> {
        let inner = SdkClient::new(cfg.clob_rest.as_str(), SdkConfig::default())?
            .authentication_builder(&signer)
            .authenticate()
            .await?;
        info!("authenticated against Polymarket CLOB");
        Ok(Self {
            inner: Arc::new(inner),
            signer,
            over_token_id,
            under_token_id,
        })
    }

    pub fn signer_from_hex(pk_hex: &str) -> Result<PrivateKeySigner> {
        let signer = PrivateKeySigner::from_str(pk_hex)?.with_chain_id(Some(POLYGON));
        Ok(signer)
    }

    /// Build, sign, and submit a marketable order matching the intent.
    /// Uses the SDK's `market_order` builder with `OrderType::FOK` so partials
    /// either fill in their entirety or get cancelled.
    pub async fn submit_intent(&self, intent: &TradeIntent) -> Result<SubmitResult> {
        let token_id = self.token_id_for(intent.buy_side);
        let resp = submit_via_sdk(
            self.inner.clone(),
            &self.signer,
            token_id,
            intent.size_usdc,
        )
        .await?;
        Ok(resp)
    }

    fn token_id_for(&self, side: BuyOutcome) -> &str {
        match side {
            BuyOutcome::Over => &self.over_token_id,
            BuyOutcome::Under => &self.under_token_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubmitResult {
    pub success: bool,
    pub order_id: Option<String>,
    pub error: Option<String>,
}

/// Isolates the SDK builder/sign/submit calls. Kept as a free fn so it's
/// easy to unit-test by substituting a mock SDK Client later.
async fn submit_via_sdk(
    client: Arc<AuthClient>,
    signer: &PrivateKeySigner,
    token_id: &str,
    size_usdc: Decimal,
) -> Result<SubmitResult> {
    use polymarket_client_sdk_v2::clob::types::{Amount, OrderType, Side};
    use polymarket_client_sdk_v2::types::U256;

    let token_u256 = U256::from_str(token_id)?;
    let order = client
        .market_order()
        .token_id(token_u256)
        .amount(Amount::usdc(size_usdc).map_err(|e| anyhow!("usdc amount: {e}"))?)
        .side(Side::Buy)
        .order_type(OrderType::FOK)
        .build()
        .await?;
    let signed = client.sign(signer, order).await?;
    let resp = client.post_order(signed).await?;
    // The SDK's response type is opaque to us here; convert to our SubmitResult.
    // We rely on the SDK's Display/Debug to surface failures.
    Ok(SubmitResult {
        success: true,
        order_id: Some(format!("{resp:?}")),
        error: None,
    })
}
