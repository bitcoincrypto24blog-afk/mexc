use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use dashmap::DashSet;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::mexc::rest::fetch_coin_configs;
use crate::monitor::NewCoinEvent;

/// Poll capital/config/getall (private, requires API key) and fire events
/// when a new coin has deposit enabled — signals an IMMINENT listing.
///
/// This monitor is OPTIONAL. It only runs if MEXC_API_KEY + MEXC_SECRET_KEY
/// are set in the environment.
pub async fn run(
    config: Arc<Config>,
    tx: UnboundedSender<NewCoinEvent>,
    known_deposits: Arc<DashSet<String>>,
) -> Result<()> {
    let api_key = match &config.mexc_api_key {
        Some(k) => k.clone(),
        None => {
            warn!("⚠️  Deposit monitor disabled — MEXC_API_KEY not set");
            return Ok(());
        }
    };
    let secret = match &config.mexc_secret_key {
        Some(s) => s.clone(),
        None => {
            warn!("⚠️  Deposit monitor disabled — MEXC_SECRET_KEY not set");
            return Ok(());
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;

    info!(
        "🏦 Deposit monitor started (poll every {}ms) — early pre-listing signal",
        config.deposit_poll_ms
    );

    let mut seeded = false;

    loop {
        match fetch_coin_configs(&client, &api_key, &secret).await {
            Ok(configs) => {
                if !seeded {
                    for c in &configs {
                        known_deposits.insert(c.coin.clone());
                    }
                    info!("✅ Deposit monitor seeded ({} coins)", known_deposits.len());
                    seeded = true;
                } else {
                    for c in configs {
                        if c.deposit_enabled() && !known_deposits.contains(&c.coin) {
                            info!("💰 NEW deposit enabled: {}", c.coin);
                            known_deposits.insert(c.coin.clone());
                            let ev = NewCoinEvent::new_deposit(&c.coin);
                            if tx.send(ev).is_err() {
                                warn!("Event channel closed — stopping deposit monitor");
                                return Ok(());
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Deposit monitor error: {e}");
                sleep(Duration::from_secs(2)).await;
            }
        }
        sleep(Duration::from_millis(config.deposit_poll_ms)).await;
    }
}
