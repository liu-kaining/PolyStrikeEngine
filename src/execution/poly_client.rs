use std::str::FromStr;

use anyhow::Context;
use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk::auth::{state::Authenticated as AuthenticatedState, Normal, Signer};
use polymarket_client_sdk::clob::{client::Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{Side as ClobSide, SignatureType};
use polymarket_client_sdk::types::{Address, Decimal};
use rust_decimal::prelude::FromPrimitive;

use crate::models::types::OrderSide;

const DEFAULT_CLOB_URL: &str = "https://clob.polymarket.com";

/// Thin wrapper around an authenticated Polymarket CLOB client.
/// Signer is cached so the hot path never touches env or parses PK.
pub struct PolyClient {
    client: ClobClient<AuthenticatedState<Normal>>,
    signer: PrivateKeySigner,
}

impl PolyClient {
    /// Initialize an authenticated Polymarket CLOB client using EIP-712 proxy wallet signing.
    ///
    /// Expected environment variables (loaded by the caller via `dotenvy`):
    /// - `PK`: hex-encoded private key
    /// - `CHAIN_ID`: numeric chain id (e.g. 137 for Polygon)
    /// - `FUNDER_ADDRESS`: optional override for proxy wallet address
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

        let base_client =
            ClobClient::new(DEFAULT_CLOB_URL, ClobConfig::default()).context("init CLOB client")?;

        let mut auth_builder = base_client.authentication_builder(&signer);

        // SignatureType::Proxy corresponds to the proxy wallet flow (signature_type = 2).
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

        Ok(Self { client, signer })
    }

    fn map_side(side: OrderSide) -> ClobSide {
        match side {
            OrderSide::Buy => ClobSide::Buy,
            OrderSide::Sell => ClobSide::Sell,
        }
    }

    /// Build and submit a limit order that crosses the book (sniper-style).
    ///
    /// Returns the Polymarket order id on success.
    pub async fn execute_snipe_order(
        &self,
        token_id: &str,
        side: OrderSide,
        price: f64,
        size: f64,
    ) -> anyhow::Result<String> {
        let price_dec =
            Decimal::from_f64(price).context("invalid price, cannot represent as Decimal")?;
        let size_dec =
            Decimal::from_f64(size).context("invalid size, cannot represent as Decimal")?;

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

        let resp = self
            .client
            .post_order(signed)
            .await
            .context("failed to post order")?;

        // Return the CLOB order id.
        Ok(resp.order_id)
    }
}

