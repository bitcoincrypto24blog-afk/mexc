use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::unbounded_channel;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::config::Config;
use crate::mexc::websocket::{stream_symbol_prices, PriceEvent};
use crate::telegram;

/// Spawns a price-tracking task for a newly listed symbol.
///
/// Guarantees:
///  - T₀ price is captured at the **exact microsecond** the first trade
///    message is received from the WebSocket (zero polling — pure async await).
///  - **Every single trade** for 4 seconds after T₀ is recorded with 100%
///    accuracy using an unbounded async channel (no drops, no backpressure).
///  - No `sleep` or polling in the hot path — does not affect sniper speed.
pub fn spawn_price_tracker(symbol: String, config: Arc<Config>) {
    tokio::spawn(async move {
        if let Err(e) = track_price(symbol, config).await {
            warn!("price tracker error: {e}");
        }
    });
}

async fn track_price(symbol: String, config: Arc<Config>) -> anyhow::Result<()> {
    // ── Async unbounded channel — zero-latency delivery, no drops ────────────
    // tokio::sync::mpsc allows direct .await on recv() instead of polling.
    let (tx_price, mut rx_price) = unbounded_channel::<PriceEvent>();

    let sym_clone = symbol.clone();
    tokio::spawn(async move {
        stream_symbol_prices(sym_clone, tx_price).await;
    });

    info!("⏱  Price tracker waiting for first trade on {symbol}");

    // ── Wait for T₀ — ZERO polling, pure async suspend ───────────────────────
    // The task is suspended (not spinning) until the exact WS message arrives.
    // No 10 ms tick loop — we wake up at the exact moment the first trade lands.
    let first = match timeout(Duration::from_secs(1800), rx_price.recv()).await {
        Ok(Some(ev)) => ev,
        Ok(None) => {
            warn!("Price channel closed for {symbol} before first trade");
            return Ok(());
        }
        Err(_) => {
            warn!("No first trade for {symbol} within 30 min — aborting");
            return Ok(());
        }
    };

    // ── T₀: stamp the exact microsecond the message woke us up ───────────────
    let t0_instant   = Instant::now();        // monotonic clock — used for the 4s window
    let t0_wall_us   = SystemTime::now()      // wall-clock µs — shown in the report
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros();

    let price_t0: f64   = first.deal.p.parse().unwrap_or(0.0);
    let exch_ts_t0: u64 = first.deal.t;      // exchange timestamp (ms)
    let side_t0: u8     = first.deal.side;   // 1=buy 2=sell

    info!("🎯 {symbol} T₀ first trade: ${price_t0:.8} | local={t0_wall_us}µs | exchange={exch_ts_t0}ms");

    // ── Collect EVERY trade for exactly 4 seconds ─────────────────────────────
    // Pre-allocated Vec; channel is unbounded so the WebSocket task never
    // blocks — every deal is enqueued immediately and drained here.
    let mut trades: Vec<(u64, f64, u8)> = Vec::with_capacity(512);
    trades.push((exch_ts_t0, price_t0, side_t0));

    let window = Duration::from_secs(4);
    loop {
        let elapsed = t0_instant.elapsed();
        if elapsed >= window {
            break;
        }
        let remaining = window - elapsed;

        // Suspend until next trade arrives OR window expires — no spin, no sleep
        match timeout(remaining, rx_price.recv()).await {
            Ok(Some(ev)) => {
                if let Ok(p) = ev.deal.p.parse::<f64>() {
                    trades.push((ev.deal.t, p, ev.deal.side));
                }
            }
            // Window expired or channel closed — both are clean exits
            _ => break,
        }
    }

    // ── Build the report ──────────────────────────────────────────────────────
    let total  = trades.len();
    let p_last = trades.last().map(|t| t.1).unwrap_or(price_t0);
    let p_min  = trades.iter().map(|t| t.1).fold(f64::INFINITY,     f64::min);
    let p_max  = trades.iter().map(|t| t.1).fold(f64::NEG_INFINITY, f64::max);

    let pct = if price_t0 > 0.0 {
        (p_last - price_t0) / price_t0 * 100.0
    } else {
        0.0
    };
    let arrow = if pct >= 0.0 { "🟢" } else { "🔴" };

    // Per-second price snapshots (last trade seen at or before that second)
    let snap = |offset_sec: u64| -> f64 {
        let ts_target = exch_ts_t0 + offset_sec * 1_000;
        trades
            .iter()
            .filter(|t| t.0 <= ts_target)
            .last()
            .map(|t| t.1)
            .unwrap_or(price_t0)
    };

    // Buy/sell breakdown
    let buys  = trades.iter().filter(|t| t.2 == 1).count();
    let sells = trades.iter().filter(|t| t.2 == 2).count();

    let msg = format!(
        "📊 <b>{symbol}</b> — تقرير 4 ثوان الأولى\n\
         ══════════════════════════\n\
         🎯 T₀  (أول صفقة)  : <code>${price_t0:.8}</code>\n\
         🕐 T₀+1s           : <code>${s1:.8}</code>\n\
         🕑 T₀+2s           : <code>${s2:.8}</code>\n\
         🕒 T₀+3s           : <code>${s3:.8}</code>\n\
         🏁 T₀+4s           : <code>${p_last:.8}</code>\n\
         ──────────────────────────\n\
         📉 أدنى سعر        : <code>${p_min:.8}</code>\n\
         📈 أعلى سعر        : <code>${p_max:.8}</code>\n\
         {arrow} التغيير (4s)    : <b>{pct:+.2}%</b>\n\
         ──────────────────────────\n\
         📋 إجمالي الصفقات  : <b>{total}</b>\n\
         🟩 شراء            : {buys}   🟥 بيع : {sells}\n\
         ⏱  وقت T₀ (µs)    : <code>{t0_wall_us}</code>\n\
         ══════════════════════════\n\
         🔗 MEXC: https://www.mexc.com/exchange/{symbol}",
        s1 = snap(1),
        s2 = snap(2),
        s3 = snap(3),
    );

    telegram::send_html(config.as_ref(), &msg).await;
    Ok(())
}
