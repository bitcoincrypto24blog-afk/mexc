use anyhow::{bail, Result};
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::mexc::types::{CoinConfig, ExchangeInfo, SymbolInfo};

const BASE: &str = "https://api.mexc.com";

// ── helpers ──────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn sign(secret: &str, payload: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC init");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

// ── public endpoints ─────────────────────────────────────────────────────────

/// Fetch all SPOT symbols from exchangeInfo (public, no auth needed)
pub async fn fetch_exchange_info(client: &Client) -> Result<Vec<SymbolInfo>> {
    let resp = client
        .get(format!("{BASE}/api/v3/exchangeInfo"))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("exchangeInfo HTTP {status}: {body}");
    }

    let info: ExchangeInfo = resp.json().await?;
    // Keep only SPOT trading pairs (USDT quote, ENABLED)
    let symbols = info
        .symbols
        .into_iter()
        .filter(|s| s.quote_asset == "USDT" && s.status == "1")
        .collect();
    Ok(symbols)
}

// ── private / signed endpoints ────────────────────────────────────────────────

/// Fetch coin config list (requires API key + signature) — used for deposit monitoring
pub async fn fetch_coin_configs(
    client: &Client,
    api_key: &str,
    secret: &str,
) -> Result<Vec<CoinConfig>> {
    let ts = now_ms();
    let qs = format!("timestamp={ts}");
    let sig = sign(secret, &qs);

    let url = format!("{BASE}/api/v3/capital/config/getall?{qs}&signature={sig}");

    let resp = client
        .get(&url)
        .header("X-MEXC-APIKEY", api_key)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("capital/config HTTP {status}: {body}");
    }

    let configs: Vec<CoinConfig> = resp.json().await?;
    Ok(configs)
}

/// Fetch the latest price for a single symbol (lightweight ticker)
pub async fn fetch_ticker_price(client: &Client, symbol: &str) -> Result<f64> {
    #[derive(serde::Deserialize)]
    struct Ticker {
        price: String,
    }

    let url = format!("{BASE}/api/v3/ticker/price?symbol={symbol}");
    let resp = client.get(&url).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("ticker/price HTTP {status}: {body}");
    }

    let t: Ticker = resp.json().await?;
    let price: f64 = t.price.parse()?;
    Ok(price)
}
