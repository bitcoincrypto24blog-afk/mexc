use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{interval, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use url::Url;

use crate::mexc::types::{Deal, DealsPayload, MiniTicker};
use crate::monitor::NewCoinEvent;

const MEXC_WS_URL: &str = "wss://wbs.mexc.com/ws";
const PING_SECS: u64    = 20;
const RECONNECT_MS: u64 = 1_500;

// ─────────────────────────────────────────────────────────────────────────────
// All-tickers PUBLIC stream  (no API key needed)
// Subscribes to spot@public.miniTicker.v3.api@+@UTC+8
// Every message contains the full list of active symbols.
// When we see a symbol we have NOT seen before → NewCoinEvent.
// ─────────────────────────────────────────────────────────────────────────────

pub async fn stream_all_tickers(
    tx: UnboundedSender<NewCoinEvent>,
    known: crate::monitor::symbol::KnownSymbols,
) {
    let sub = serde_json::json!({
        "method": "SUBSCRIPTION",
        "params": ["spot@public.miniTicker.v3.api@+@UTC+8"]
    })
    .to_string();

    loop {
        info!("🌐 WS connecting — all-tickers stream");
        match run_ticker_loop(&sub, &tx, &known).await {
            Ok(())  => warn!("All-tickers WS closed — reconnecting…"),
            Err(e)  => error!("All-tickers WS error: {e} — reconnecting…"),
        }
        sleep(Duration::from_millis(RECONNECT_MS)).await;
    }
}

async fn run_ticker_loop(
    sub_msg: &str,
    tx: &UnboundedSender<NewCoinEvent>,
    known: &crate::monitor::symbol::KnownSymbols,
) -> Result<()> {
    let (ws, _) = connect_async(Url::parse(MEXC_WS_URL)?).await?;
    let (mut write, mut read) = ws.split();

    write.send(Message::Text(sub_msg.to_owned())).await?;
    info!("📡 Subscribed: spot@public.miniTicker.v3.api@+@UTC+8");

    let mut ping_tick = interval(Duration::from_secs(PING_SECS));

    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                let ping = serde_json::json!({"method":"PING"}).to_string();
                if write.send(Message::Text(ping)).await.is_err() { break; }
            }
            msg = read.next() => {
                match msg {
                    None | Some(Ok(Message::Close(_))) => break,
                    Some(Err(e)) => { warn!("ticker WS recv: {e}"); break; }
                    Some(Ok(Message::Text(txt))) => {
                        process_ticker_msg(&txt, tx, known);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn process_ticker_msg(
    text: &str,
    tx: &UnboundedSender<NewCoinEvent>,
    known: &crate::monitor::symbol::KnownSymbols,
) {
    if text.contains("PONG") || text.contains("\"code\"") { return; }

    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };

    let d = match v.get("d") {
        Some(d) => d,
        None => return,
    };

    let ticker: MiniTicker = match serde_json::from_value(d.clone()) {
        Ok(t) => t,
        Err(e) => { debug!("ticker parse: {e}"); return; }
    };

    // Spot USDT pairs only
    if !ticker.s.ends_with("USDT") { return; }

    if !known.contains(&ticker.s) {
        let base = ticker.s.trim_end_matches("USDT").to_string();
        info!("🆕 NEW symbol via WS: {}", ticker.s);
        known.insert(ticker.s.clone());
        let ev = NewCoinEvent::new_exchange(&ticker.s, &base);
        let _ = tx.send(ev);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-symbol deals stream  (price tracking after listing)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PriceEvent {
    pub symbol: String,
    pub deal: Deal,
}

/// Subscribe to real-time trades for one symbol. Auto-reconnects.
pub async fn stream_symbol_prices(symbol: String, tx: tokio::sync::mpsc::UnboundedSender<PriceEvent>) {
    let channel = format!("spot@public.deals.v3.api@{symbol}");
    let sub = serde_json::json!({
        "method": "SUBSCRIPTION",
        "params": [channel]
    })
    .to_string();

    loop {
        info!("🔌 WS price stream: {symbol}");
        match run_price_loop(&symbol, &sub, &tx).await {
            Ok(())  => warn!("Price WS closed for {symbol}, reconnecting…"),
            Err(e)  => error!("Price WS error for {symbol}: {e}"),
        }
        sleep(Duration::from_millis(RECONNECT_MS)).await;
    }
}

async fn run_price_loop(
    symbol: &str,
    sub_msg: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<PriceEvent>,
) -> Result<()> {
    let (ws, _) = connect_async(Url::parse(MEXC_WS_URL)?).await?;
    let (mut write, mut read) = ws.split();

    write.send(Message::Text(sub_msg.to_owned())).await?;
    info!("📡 Subscribed deals: {symbol}");

    let mut ping_tick = interval(Duration::from_secs(PING_SECS));

    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                let ping = serde_json::json!({"method":"PING"}).to_string();
                if write.send(Message::Text(ping)).await.is_err() { break; }
            }
            msg = read.next() => {
                match msg {
                    None | Some(Ok(Message::Close(_))) => break,
                    Some(Err(e)) => { warn!("price WS {symbol}: {e}"); break; }
                    Some(Ok(Message::Text(txt))) => {
                        if let Err(e) = process_price_msg(&txt, symbol, tx) {
                            debug!("price parse {symbol}: {e}");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn process_price_msg(text: &str, symbol: &str, tx: &tokio::sync::mpsc::UnboundedSender<PriceEvent>) -> Result<()> {
    if text.contains("PONG") || text.contains("\"code\"") { return Ok(()); }

    let v: serde_json::Value = serde_json::from_str(text)?;
    let deals_val = v
        .get("d")
        .and_then(|d| d.get("deals"))
        .ok_or_else(|| anyhow::anyhow!("no d.deals"))?;

    let payload: DealsPayload =
        serde_json::from_value(serde_json::json!({ "deals": deals_val }))?;

    for deal in payload.deals {
        if tx.send(PriceEvent { symbol: symbol.to_owned(), deal }).is_err() {
            break;
        }
    }
    Ok(())
}
