use serde::{Deserialize, Serialize};

// ── REST: exchangeInfo ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SymbolInfo {
    pub symbol: String,
    #[serde(rename = "baseAsset")]
    pub base_asset: String,
    #[serde(rename = "quoteAsset")]
    pub quote_asset: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct ExchangeInfo {
    pub symbols: Vec<SymbolInfo>,
}

// ── REST: capital/config/getall ─────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct CoinConfig {
    pub coin: String,
    pub name: Option<String>,
    #[serde(rename = "networkList")]
    pub network_list: Vec<NetworkInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkInfo {
    pub coin: String,
    pub network: String,
    #[serde(rename = "depositEnable")]
    pub deposit_enable: bool,
    #[serde(rename = "withdrawEnable")]
    pub withdraw_enable: bool,
}

impl CoinConfig {
    /// Returns true if at least one network has deposit enabled
    pub fn deposit_enabled(&self) -> bool {
        self.network_list.iter().any(|n| n.deposit_enable)
    }
}

// ── WebSocket: Mini Ticker (all symbols stream) ─────────────────────────────

/// Payload inside `spot@public.miniTicker.v3.api@{SYMBOL}@UTC+8`
#[derive(Debug, Clone, Deserialize)]
pub struct MiniTicker {
    pub s: String,  // symbol  e.g. "BTCUSDT"
    #[serde(rename = "c")]
    pub last_price: String,  // last / close price
    pub v: String,  // base volume
    pub q: String,  // quote volume
}


/// Generic WS envelope from MEXC
#[derive(Debug, Deserialize)]
pub struct WsEnvelope {
    pub c: Option<String>,              // channel
    pub d: Option<serde_json::Value>,   // data payload
    pub s: Option<String>,              // symbol (sometimes top-level)
    pub t: Option<u64>,                 // timestamp ms
    // control messages
    pub msg: Option<String>,
    pub code: Option<i64>,
}

/// Individual trade / deal
#[derive(Debug, Clone, Deserialize)]
pub struct Deal {
    pub p: String,  // price
    pub v: String,  // volume
    pub t: u64,     // timestamp ms
    #[serde(rename = "S")]
    pub side: u8,   // 1 = buy, 2 = sell
}

#[derive(Debug, Deserialize)]
pub struct DealsPayload {
    pub deals: Vec<Deal>,
}

// ── Internal price snapshot ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PriceSnap {
    pub symbol: String,
    pub price: f64,
    pub ts_ms: u64,
}
