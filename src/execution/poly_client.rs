use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk::data::{
    client::Client as DataClient,
    types::request::PositionsRequest,
};
use polymarket_client_sdk::auth::{state::Authenticated as AuthenticatedState, Normal, Signer};
use polymarket_client_sdk::clob::{client::Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{
    request::CancelMarketOrderRequest, Side as ClobSide, SignatureType,
};
use polymarket_client_sdk::types::{Address, Decimal};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use tokio::sync::Semaphore;
use tracing::warn;

use crate::models::types::OrderSide;

const DEFAULT_CLOB_URL: &str = "https://clob.polymarket.com";
/// Maximum concurrent API requests (token-bucket style).
const MAX_CONCURRENT_REQUESTS: usize = 8;
/// Max retries on 429 / transient errors before giving up.
const MAX_429_RETRIES: u32 = 3;

/// Thin wrapper around an authenticated Polymarket CLOB client.
/// Includes a Semaphore-based rate limiter shared across all callers.
pub struct PolyClient {
    client: ClobClient<AuthenticatedState<Normal>>,
    data_client: DataClient,
    signer: PrivateKeySigner,
    user_address: Address,
    rate_limiter: Arc<Semaphore>,
}

impl PolyClient {
    pub async fn new_from_env() -> anyhow::Result<Self> {
        let pk = std::env::var("PK").context("PK (private key) missing from environment")?;
        let chain_id: u64 = std::env::var("CHAIN_ID")
            .context("CHAIN_ID missing from environment")?
            .parse()
            .context("invalid CHAIN_ID")?;

        let signer = PrivateKeySigner::from_str(&pk)
            .context("failed to parse PK into LocalSigner")?
            .with_chain_id(Some(chain_id.into()));

        let funder_override = std::env::var("FUNDER_ADDRESS").ok();
        let user_address = if let Some(funder) = funder_override.as_deref() {
            funder.parse().context("invalid FUNDER_ADDRESS hex")?
        } else {
            signer.address()
        };

        let base_client =
            ClobClient::new(DEFAULT_CLOB_URL, ClobConfig::default()).context("init CLOB client")?;

        let mut auth_builder = base_client.authentication_builder(&signer);
        auth_builder = auth_builder.signature_type(SignatureType::Proxy);

        if let Some(funder) = funder_override {
            let addr: Address = funder
                .parse()
                .context("invalid FUNDER_ADDRESS hex")?;
            auth_builder = auth_builder.funder(addr);
        }

        let client = auth_builder
            .authenticate()
            .await
            .context("failed to authenticate Polymarket client")?;

        Ok(Self {
            client,
            data_client: DataClient::default(),
            signer,
            user_address,
            rate_limiter: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        })
    }

    fn map_side(side: OrderSide) -> ClobSide {
        match side {
            OrderSide::Buy => ClobSide::Buy,
            OrderSide::Sell => ClobSide::Sell,
        }
    }

    fn is_rate_limited(err: &anyhow::Error) -> bool {
        let msg = format!("{:#}", err);
        msg.contains("429") || msg.contains("Too Many Requests") || msg.contains("rate limit")
    }

    /// Build and submit a limit order with rate limiting and 429 backoff.
    pub async fn execute_snipe_order(
        &self,
        token_id: &str,
        side: OrderSide,
        price: f64,
        size: f64,
    ) -> anyhow::Result<String> {
        let _permit = self
            .rate_limiter
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("rate limiter closed"))?;

        let price_dec =
            Decimal::from_f64(price).context("invalid price, cannot represent as Decimal")?;
        let size_dec =
            Decimal::from_f64(size).context("invalid size, cannot represent as Decimal")?;

        let mut backoff_ms = 500u64;
        for attempt in 0..=MAX_429_RETRIES {
            let order = self
                .client
                .limit_order()
                .token_id(token_id)
                .size(size_dec)
                .price(price_dec)
                .side(Self::map_side(side))
                .build()
                .await
                .context("failed to build limit order")?;

            let signed = self
                .client
                .sign(&self.signer, order)
                .await
                .context("failed to sign order")?;

            match self.client.post_order(signed).await {
                Ok(resp) => return Ok(resp.order_id),
                Err(e) => {
                    let err = anyhow::Error::from(e).context("failed to post order");
                    if Self::is_rate_limited(&err) && attempt < MAX_429_RETRIES {
                        warn!(
                            "[PolyClient] 429 rate limited (attempt {}/{}), backing off {}ms",
                            attempt + 1,
                            MAX_429_RETRIES,
                            backoff_ms
                        );
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(5000);
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        anyhow::bail!("max 429 retries exceeded")
    }

    /// Fetch live position size (shares) for a token from Polymarket Data API.
    pub async fn sync_token_position(&self, token_id: &str) -> anyhow::Result<f64> {
        let _permit = self
            .rate_limiter
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("rate limiter closed"))?;

        let request = PositionsRequest::builder()
            .user(self.user_address)
            .build();
        let positions = self
            .data_client
            .positions(&request)
            .await
            .context("failed to fetch positions from Data API")?;
        let shares = positions
            .iter()
            .find(|p| p.asset == token_id)
            .and_then(|p| p.size.to_f64())
            .unwrap_or(0.0);
        Ok(shares.max(0.0))
    }

    /// Cancel all open orders for a specific token (asset_id). Best-effort.
    pub async fn cancel_all_orders(&self, token_id: &str) -> anyhow::Result<()> {
        let _permit = self
            .rate_limiter
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("rate limiter closed"))?;

        let req = CancelMarketOrderRequest::builder()
            .asset_id(token_id)
            .build();
        self.client
            .cancel_market_orders(&req)
            .await
            .context("failed to cancel market orders")?;
        Ok(())
    }
}
