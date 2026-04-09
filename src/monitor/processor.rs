use std::sync::Arc;
use std::time::Duration;

use dashmap::DashSet;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{error, info};

use crate::cmc::client::{lookup_coin, CmcResult};
use crate::config::Config;
use crate::monitor::{DiscoverySource, NewCoinEvent};
use crate::monitor::price::spawn_price_tracker;
use crate::telegram;

pub async fn run(
    config: Arc<Config>,
    mut rx: UnboundedReceiver<NewCoinEvent>,
    already_processed: Arc<DashSet<String>>,
) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("HTTP client");

    info!("⚙️  Event processor running — waiting for new coins…");

    while let Some(event) = rx.recv().await {
        if already_processed.contains(&event.base) {
            continue;
        }
        already_processed.insert(event.base.clone());

        info!("🔔 Processing: {} (source: {:?})", event.base, event.source);

        let cfg  = config.clone();
        let http = http.clone();

        tokio::spawn(async move {
            handle_coin(event, cfg, http).await;
        });
    }
}

async fn handle_coin(event: NewCoinEvent, config: Arc<Config>, http: reqwest::Client) {
    // ── 1. CMC lookup ────────────────────────────────────────────────────────
    let cmc = lookup_coin(&http, &config.cmc_api_key, &event.base).await;

    match cmc {
        Ok(Some(info)) => send_listed_alert(config.as_ref(), &event, &info).await,
        Ok(None)       => send_new_coin_alert(config.as_ref(), &event).await,
        Err(e) => {
            error!("CMC lookup failed for {}: {e}", event.base);
            send_new_coin_alert(config.as_ref(), &event).await;
        }
    }

    // ── 2. WS price tracker ──────────────────────────────────────────────────
    // Announcement source: announcement.rs handles WS timing (T-30s)
    // DepositOpen / ExchangeInfo: open WS immediately
    match event.source {
        DiscoverySource::Announcement => {
            // announcement.rs will call spawn_price_tracker at T-30s
            info!("📅 {} — WS timing handled by announcement monitor", event.symbol);
        }
        _ => {
            // Open WS immediately — waits silently until first trade
            info!("📡 {} — WS مفتوح وينتظر أول صفقة…", event.symbol);
            spawn_price_tracker(event.symbol.clone(), config.clone());
        }
    }
}

// ── Telegram builders ─────────────────────────────────────────────────────────

fn source_label(source: &DiscoverySource) -> &'static str {
    match source {
        DiscoverySource::ExchangeInfo => "📈 إدراج مباشر — التداول مفتوح الآن",
        DiscoverySource::DepositOpen  => "💰 إيداع مفتوح — الإدراج قريب",
        DiscoverySource::Announcement => "📢 إعلان رسمي — الإدراج مجدول",
    }
}

async fn send_listed_alert(config: &Config, event: &NewCoinEvent, cmc: &CmcResult) {
    let exchanges_str = if cmc.exchanges.is_empty() {
        "غير متاح".to_string()
    } else {
        cmc.exchanges.join(", ")
    };

    let supply_str = match cmc.total_supply {
        Some(s) if s > 1_000_000_000.0 => format!("{:.2}B", s / 1_000_000_000.0),
        Some(s) if s > 1_000_000.0     => format!("{:.2}M", s / 1_000_000.0),
        Some(s)                         => format!("{s:.0}"),
        None                            => "N/A".to_string(),
    };

    let price_str = cmc
        .price_usd
        .map(|p| format!("${p:.8}"))
        .unwrap_or_else(|| "N/A".to_string());

    let msg = format!(
        "✅ <b>{base}</b> — مُدرجة على منصات أخرى!\n\
         ══════════════════════════\n\
         🏷  الرمز         : <code>{base}</code>\n\
         💲 السعر (CMC)   : <b>{price_str}</b>\n\
         🏦 المنصات       : {exchanges_str}\n\
         📦 إجمالي العرض  : {supply_str}\n\
         {src}\n\
         ══════════════════════════\n\
         🔗 CoinMarketCap : {cmc_url}\n\
         🔗 MEXC          : https://www.mexc.com/exchange/{symbol}",
        base   = event.base,
        symbol = event.symbol,
        src    = source_label(&event.source),
        cmc_url = cmc.cmc_url,
    );

    telegram::send_html(config, &msg).await;
}

async fn send_new_coin_alert(config: &Config, event: &NewCoinEvent) {
    let msg = format!(
        "🆕 <b>{base}</b> — عملة جديدة!\n\
         ══════════════════════════\n\
         🔎 غير مدرجة على CoinMarketCap بعد\n\
         {src}\n\
         ══════════════════════════\n\
         👁  جاري مراقبة العملة…\n\
         🔗 MEXC : https://www.mexc.com/exchange/{symbol}",
        base   = event.base,
        symbol = event.symbol,
        src    = source_label(&event.source),
    );

    telegram::send_html(config, &msg).await;
}
