use std::sync::Arc;

use dashmap::DashSet;
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;

use crate::monitor::NewCoinEvent;
use crate::mexc::websocket::stream_all_tickers;

/// Shared set of known symbols — written by WS ticker, read by deposit monitor
pub type KnownSymbols = Arc<DashSet<String>>;

/// Runs forever: connects to the public all-tickers WebSocket and fires
/// `NewCoinEvent` for every symbol seen for the first time.
/// This replaces the old REST polling approach entirely.
pub async fn run(
    tx: UnboundedSender<NewCoinEvent>,
    known: KnownSymbols,
) -> anyhow::Result<()> {
    info!("📋 Symbol monitor started — using PUBLIC WebSocket all-tickers stream");
    // stream_all_tickers loops forever + auto-reconnects internally
    stream_all_tickers(tx, known).await;
    Ok(())
}
