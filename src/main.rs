use std::sync::Arc;

use anyhow::Result;
use dashmap::DashSet;
use tokio::sync::mpsc;
use tracing::{error, info};

mod cmc;
mod config;
mod mexc;
mod monitor;
mod telegram;

use config::Config;
use monitor::symbol::KnownSymbols;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mexc_sniper=info,warn".parse().unwrap()),
        )
        .with_target(false)
        .compact()
        .init();

    info!("════════════════════════════════════");
    info!("   MEXC Sniper Bot — Starting");
    info!("════════════════════════════════════");

    let config = Arc::new(Config::from_env()?);

    // ── Shared state ──────────────────────────────────────────────────────────
    let known_symbols:   KnownSymbols            = Arc::new(DashSet::new());
    let known_deposits:  Arc<DashSet<String>>    = Arc::new(DashSet::new());
    let processed_bases: Arc<DashSet<String>>    = Arc::new(DashSet::new());

    let (event_tx, event_rx) = mpsc::unbounded_channel();

    // ── Startup Telegram ─────────────────────────────────────────────────────
    let dep_status = if config.deposit_monitor_enabled() {
        "✅ نشطة (API keys موجودة)"
    } else {
        "⚠️  معطّلة (بدون MEXC API key)"
    };

    let msg = format!(
        "🚀 <b>MEXC Sniper Bot</b> — تشغيل\n\
         ════════════════════════\n\
         ✅ مراقبة الإعلانات (pre-listing) : <b>نشطة</b>\n\
         {dep} مراقبة الإيداعات (pre-listing) : <b>{dep_status}</b>\n\
         ✅ مراقبة الرموز — WebSocket عام   : <b>نشطة</b>\n\
         ✅ تتبع السعر — WS مباشر           : <b>جاهز</b>\n\
         ✅ CoinMarketCap                    : <b>متصل</b>\n\
         ✅ إشعارات تيليجرام                : <b>تعمل ✓</b>\n\
         ════════════════════════\n\
         ⚡ السوق : <b>Spot USDT فقط</b>",
        dep = if config.deposit_monitor_enabled() { "✅" } else { "⚠️" },
        dep_status = dep_status,
    );
    telegram::send_html(config.as_ref(), &msg).await;
    info!("✅ Startup notification sent");

    // ── Spawn: Announcement Monitor ───────────────────────────────────────────
    {
        let cfg   = config.clone();
        let tx    = event_tx.clone();
        let known = known_symbols.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = monitor::announcement::run(cfg.clone(), tx.clone(), known.clone()).await {
                    error!("Announcement monitor crashed: {e} — restarting in 30s");
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
            }
        });
    }

    // ── Spawn: Symbol Monitor (WebSocket public all-tickers) ──────────────────
    {
        let tx    = event_tx.clone();
        let known = known_symbols.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = monitor::symbol::run(tx.clone(), known.clone()).await {
                    error!("Symbol monitor crashed: {e} — restarting in 2s");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        });
    }

    // ── Spawn: Deposit Monitor (optional, requires MEXC API keys) ─────────────
    if config.deposit_monitor_enabled() {
        let cfg = config.clone();
        let tx  = event_tx.clone();
        let kd  = known_deposits.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = monitor::deposit::run(cfg.clone(), tx.clone(), kd.clone()).await {
                    error!("Deposit monitor crashed: {e} — restarting in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        });
    }

    // ── Event Processor ───────────────────────────────────────────────────────
    monitor::processor::run(config, event_rx, processed_bases).await;

    Ok(())
}
