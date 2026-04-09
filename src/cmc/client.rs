use anyhow::{bail, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing::debug;

const CMC_BASE: &str = "https://pro-api.coinmarketcap.com";

/// Result of a successful CMC lookup
#[derive(Debug, Clone)]
pub struct CmcResult {
    pub price_usd: Option<f64>,
    pub total_supply: Option<f64>,
    pub exchanges: Vec<String>,
    pub cmc_url: String,
}

// ── Internal response types ───────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
struct CmcQuoteResp {
    data: Option<std::collections::HashMap<String, Vec<CmcCoin>>>,
    status: CmcStatus,
}

#[derive(Deserialize, Debug)]
struct CmcStatus {
    error_code: i64,
    error_message: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct CmcCoin {
    slug: String,
    total_supply: Option<f64>,
    quote: Option<std::collections::HashMap<String, CmcQuote>>,
}

#[derive(Deserialize, Debug, Clone)]
struct CmcQuote {
    price: Option<f64>,
}

#[derive(Deserialize, Debug)]
struct CmcPairsResp {
    data: Option<CmcPairsData>,
}

#[derive(Deserialize, Debug)]
struct CmcPairsData {
    market_pairs: Option<Vec<CmcPair>>,
}

#[derive(Deserialize, Debug)]
struct CmcPair {
    exchange: CmcExchange,
}

#[derive(Deserialize, Debug)]
struct CmcExchange {
    name: String,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns Some(CmcResult) if the symbol exists in CMC, None if it doesn't, Err on hard failure.
pub async fn lookup_coin(client: &Client, api_key: &str, symbol: &str) -> Result<Option<CmcResult>> {
    // Step 1: quotes/latest
    let url = format!(
        "{CMC_BASE}/v2/cryptocurrency/quotes/latest?symbol={symbol}&convert=USD&aux=total_supply"
    );
    debug!("CMC request: {url}");

    let resp = client
        .get(&url)
        .header("X-CMC_PRO_API_KEY", api_key)
        .header("Accept", "application/json")
        .send()
        .await?;

    let status_code = resp.status();
    let body = resp.text().await?;
    debug!("CMC response ({status_code}): {body}");

    let parsed: CmcQuoteResp = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => bail!("CMC JSON parse error: {e} — body: {body}"),
    };

    if parsed.status.error_code != 0 {
        // error_code 400 = symbol not found
        if parsed.status.error_code == 400 {
            return Ok(None);
        }
        bail!(
            "CMC API error {}: {}",
            parsed.status.error_code,
            parsed.status.error_message.unwrap_or_default()
        );
    }

    let coins = match &parsed.data {
        Some(d) => d.get(symbol).cloned().unwrap_or_default(),
        None => return Ok(None),
    };

    if coins.is_empty() {
        return Ok(None);
    }

    let coin = &coins[0];
    let slug = &coin.slug;
    let price_usd = coin
        .quote
        .as_ref()
        .and_then(|q| q.get("USD"))
        .and_then(|u| u.price);

    let total_supply = coin.total_supply;
    let cmc_url = format!("https://coinmarketcap.com/currencies/{slug}/");

    // Step 2: market-pairs (top 5 exchanges)
    let exchanges = fetch_top_exchanges(client, api_key, symbol)
        .await
        .unwrap_or_default();

    Ok(Some(CmcResult {
        price_usd,
        total_supply,
        exchanges,
        cmc_url,
    }))
}

async fn fetch_top_exchanges(client: &Client, api_key: &str, symbol: &str) -> Result<Vec<String>> {
    let url = format!(
        "{CMC_BASE}/v1/cryptocurrency/market-pairs/latest?symbol={symbol}&limit=5&convert=USD"
    );

    let resp = client
        .get(&url)
        .header("X-CMC_PRO_API_KEY", api_key)
        .header("Accept", "application/json")
        .send()
        .await?;

    let body = resp.text().await?;
    let parsed: CmcPairsResp = serde_json::from_str(&body)?;

    let exchanges: Vec<String> = parsed
        .data
        .and_then(|d| d.market_pairs)
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.exchange.name)
        .collect();

    // Deduplicate while preserving order
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<String> = exchanges
        .into_iter()
        .filter(|e| seen.insert(e.clone()))
        .collect();

    Ok(unique)
}
