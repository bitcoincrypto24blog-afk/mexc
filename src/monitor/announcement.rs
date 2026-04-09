use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use dashmap::DashSet;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::monitor::{now_ms, NewCoinEvent};
use crate::monitor::price::spawn_price_tracker;
use crate::telegram;

// ── MEXC Announcement API response types ─────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AnnResponse {
    data: Option<AnnData>,
}

#[derive(Debug, Deserialize)]
struct AnnData {
    items: Option<Vec<AnnItem>>,
}

#[derive(Debug, Deserialize, Clone)]
struct AnnItem {
    /// Article title — e.g. "MEXC Will List XYZ (XYZUSDT) 2024-03-14 10:00 (UTC)"
    title: String,
    /// Article URL
    #[serde(rename = "articleId")]
    article_id: Option<String>,
    /// Unix timestamp ms when announcement was published
    #[serde(rename = "publishTime")]
    publish_time: Option<u64>,
}

// ── Internal parsed listing ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PendingListing {
    pub symbol: String,
    pub base: String,
    pub open_time_ms: u64,
    pub title: String,
    pub article_url: String,
    /// Whether we already sent the 5-min warning
    pub warned: bool,
    /// Whether we already opened the WS price tracker
    pub ws_opened: bool,
}

// ── Main loop ─────────────────────────────────────────────────────────────────

/// Polls MEXC announcement API every 60 s.
/// - Sends Telegram alert on new listing announcement with countdown
/// - Sends 5-min warning before trading opens
/// - Opens WS price tracker at T-30s so it's ready exactly at open
pub async fn run(
    config: Arc<Config>,
    tx: UnboundedSender<NewCoinEvent>,
    known: Arc<DashSet<String>>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("Mozilla/5.0")
        .build()?;

    info!("📢 Announcement monitor started (poll every 60s)");

    // symbol → PendingListing
    let mut pending: HashMap<String, PendingListing> = HashMap::new();
    let mut seen_titles: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        // ── Fetch announcements ───────────────────────────────────────────────
        match fetch_announcements(&client).await {
            Ok(items) => {
                for item in items {
                    if seen_titles.contains(&item.title) {
                        continue;
                    }

                    // Try to parse symbol + open time from title
                    if let Some(listing) = parse_listing(&item) {
                        if !pending.contains_key(&listing.symbol) {
                            seen_titles.insert(item.title.clone());
                            info!(
                                "📢 New listing announcement: {} opens at {}",
                                listing.symbol,
                                format_time_ms(listing.open_time_ms)
                            );

                            // Send immediate announcement alert
                            send_announcement_alert(config.as_ref(), &listing).await;

                            // Pre-register in known so symbol monitor doesn't double-fire
                            known.insert(listing.symbol.clone());

                            // Emit NewCoinEvent for CMC lookup
                            let ev = NewCoinEvent::new_exchange(&listing.symbol, &listing.base);
                            let _ = tx.send(ev);

                            pending.insert(listing.symbol.clone(), listing);
                        }
                    }
                }
            }
            Err(e) => error!("Announcement fetch error: {e}"),
        }

        // ── Process pending listings ──────────────────────────────────────────
        let now = now_ms();

        for listing in pending.values_mut() {
            let ms_left = listing.open_time_ms.saturating_sub(now);
            let mins_left = ms_left / 60_000;

            // 5-minute warning
            if !listing.warned && ms_left > 0 && mins_left <= 5 {
                listing.warned = true;
                send_5min_warning(config.as_ref(), listing).await;
            }

            // Open WS at T-30s so it's live exactly when trading starts
            if !listing.ws_opened && ms_left > 0 && ms_left <= 30_000 {
                listing.ws_opened = true;
                info!(
                    "⚡ Opening WS price tracker for {} — {}s until open",
                    listing.symbol,
                    ms_left / 1000
                );
                spawn_price_tracker(listing.symbol.clone(), config.clone());
            }

            // Trading time passed but WS was never opened (bot was down?)
            if ms_left == 0 && !listing.ws_opened {
                listing.ws_opened = true;
                warn!(
                    "⚠️  {} — open time passed, spawning price tracker late",
                    listing.symbol
                );
                spawn_price_tracker(listing.symbol.clone(), config.clone());
            }
        }

        // Remove listings older than 2 hours past open time
        pending.retain(|_, l| now < l.open_time_ms + 7_200_000);

        sleep(Duration::from_secs(60)).await;
    }
}

// ── Fetcher ───────────────────────────────────────────────────────────────────

async fn fetch_announcements(client: &reqwest::Client) -> Result<Vec<AnnItem>> {
    // Primary: MEXC announcement list API
    let url = "https://www.mexc.com/api/platform/announce/list\
               ?pageNum=1&pageSize=20&annType=coin_listings";

    let resp = client.get(url).send().await?;

    if !resp.status().is_success() {
        // Fallback URL
        let url2 = "https://api.mexc.com/api/v1/announcement\
                    ?pageNum=1&pageSize=20&type=new_listings";
        let resp2 = client.get(url2).send().await?;
        let body = resp2.text().await?;
        let parsed: AnnResponse = serde_json::from_str(&body)?;
        return Ok(parsed.data.and_then(|d| d.items).unwrap_or_default());
    }

    let body = resp.text().await?;
    let parsed: AnnResponse = serde_json::from_str(&body)?;
    Ok(parsed.data.and_then(|d| d.items).unwrap_or_default())
}

// ── Title parser ──────────────────────────────────────────────────────────────

/// Parses titles like:
///   "MEXC Will List XYZ (XYZUSDT) 2024-03-14 10:00 (UTC)"
///   "MEXC Futures Will List XYZ Perpetual Contract ..."   ← skip (futures)
fn parse_listing(item: &AnnItem) -> Option<PendingListing> {
    let title = &item.title;

    // Skip futures / perpetual
    if title.to_lowercase().contains("futures")
        || title.to_lowercase().contains("perpetual")
        || title.to_lowercase().contains("leverage")
    {
        return None;
    }

    // Extract symbol like (XYZUSDT) — must end with USDT
    let symbol = extract_between(title, '(', ')')
        .into_iter()
        .find(|s| s.ends_with("USDT") && s.len() > 4 && !s.contains(' '))?;

    let base = symbol.trim_end_matches("USDT").to_string();

    // Extract datetime: YYYY-MM-DD HH:MM
    let open_time_ms = parse_datetime_from_title(title)?;

    let article_url = item
        .article_id
        .as_ref()
        .map(|id| format!("https://www.mexc.com/support/articles/{id}"))
        .unwrap_or_else(|| "https://www.mexc.com/support/categories/announcements".to_string());

    Some(PendingListing {
        symbol: symbol.to_string(),
        base,
        open_time_ms,
        title: title.clone(),
        article_url,
        warned: false,
        ws_opened: false,
    })
}

fn extract_between(s: &str, open: char, close: char) -> Vec<String> {
    let mut results = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in s.chars() {
        if c == open {
            depth += 1;
            if depth > 1 { current.push(c); }
        } else if c == close {
            if depth == 1 {
                results.push(current.clone());
                current.clear();
            } else {
                current.push(c);
            }
            depth = depth.saturating_sub(1);
        } else if depth > 0 {
            current.push(c);
        }
    }
    results
}

/// Tries to find "YYYY-MM-DD HH:MM" in the title and convert to UTC ms
fn parse_datetime_from_title(title: &str) -> Option<u64> {
    // Match pattern: 4digits-2digits-2digits space 2digits:2digits
    let re_str = r"(\d{4})-(\d{2})-(\d{2})\s+(\d{2}):(\d{2})";
    // Manual parse without regex crate to keep deps minimal
    let bytes = title.as_bytes();
    for i in 0..bytes.len().saturating_sub(15) {
        let slice = &title[i..];
        if let Some(dt) = try_parse_dt(slice) {
            return Some(dt);
        }
    }
    None
}

fn try_parse_dt(s: &str) -> Option<u64> {
    // Expect: YYYY-MM-DD HH:MM
    if s.len() < 16 { return None; }
    let year:  u32 = s[0..4].parse().ok()?;
    if s.as_bytes().get(4)? != &b'-' { return None; }
    let month: u32 = s[5..7].parse().ok()?;
    if s.as_bytes().get(7)? != &b'-' { return None; }
    let day:   u32 = s[8..10].parse().ok()?;
    let sep = s.as_bytes().get(10)?;
    if *sep != b' ' && *sep != b'T' { return None; }
    let hour:  u32 = s[11..13].parse().ok()?;
    if s.as_bytes().get(13)? != &b':' { return None; }
    let min:   u32 = s[14..16].parse().ok()?;

    // Basic sanity
    if year < 2024 || month == 0 || month > 12 || day == 0 || day > 31
        || hour > 23 || min > 59 { return None; }

    // Convert to Unix ms (UTC) using chrono-free math
    let ts = datetime_to_unix_ms(year, month, day, hour, min);
    Some(ts)
}

/// Simple UTC datetime → Unix ms (no external crate needed)
fn datetime_to_unix_ms(year: u32, month: u32, day: u32, hour: u32, min: u32) -> u64 {
    // Days from 1970-01-01 to year-01-01
    let y = year as u64;
    let m = month as u64;
    let d = day as u64;

    let leap = |yr: u64| (yr % 4 == 0 && yr % 100 != 0) || yr % 400 == 0;
    let days_in_month = |yr: u64, mo: u64| -> u64 {
        match mo {
            1|3|5|7|8|10|12 => 31,
            4|6|9|11 => 30,
            2 => if leap(yr) { 29 } else { 28 },
            _ => 0,
        }
    };

    let mut days: u64 = 0;
    for yr in 1970..y {
        days += if leap(yr) { 366 } else { 365 };
    }
    for mo in 1..m {
        days += days_in_month(y, mo);
    }
    days += d - 1;

    let secs = days * 86_400 + hour as u64 * 3_600 + min as u64 * 60;
    secs * 1_000
}

fn format_time_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let mins = (secs / 60) % 60;
    let hours = (secs / 3600) % 24;
    let days = secs / 86400;
    if days > 0 {
        format!("{days}d {hours:02}:{mins:02} UTC")
    } else {
        format!("{hours:02}:{mins:02} UTC")
    }
}

// ── Telegram alerts ───────────────────────────────────────────────────────────

async fn send_announcement_alert(config: &Config, listing: &PendingListing) {
    let now = now_ms();
    let ms_left = listing.open_time_ms.saturating_sub(now);
    let hours_left   = ms_left / 3_600_000;
    let mins_left    = (ms_left % 3_600_000) / 60_000;

    let countdown = if hours_left > 0 {
        format!("{hours_left} ساعة و{mins_left} دقيقة")
    } else {
        format!("{mins_left} دقيقة")
    };

    let open_time_str = format_time_ms(listing.open_time_ms);

    let msg = format!(
        "📢 <b>{sym}</b> — إعلان إدراج رسمي على MEXC!\n\
         ══════════════════════════\n\
         🏷  الرمز          : <code>{sym}</code>\n\
         📅 وقت فتح التداول : <b>{open_time_str}</b>\n\
         ⏳ الوقت المتبقي   : <b>{countdown}</b>\n\
         ══════════════════════════\n\
         📌 {title}\n\
         ══════════════════════════\n\
         🔗 الإعلان : {article_url}\n\
         🔗 MEXC    : https://www.mexc.com/exchange/{sym}\n\
         ──────────────────────────\n\
         ⚡ البوت سيفتح WS قبل 30 ثانية من الفتح تلقائياً",
        sym = listing.symbol,
        title = listing.title,
        article_url = listing.article_url,
    );

    telegram::send_html(config, &msg).await;
}

async fn send_5min_warning(config: &Config, listing: &PendingListing) {
    let msg = format!(
        "⚡ <b>{sym}</b> — يفتح التداول بعد 5 دقائق!\n\
         ══════════════════════════\n\
         ⏰ وقت الفتح : <b>{open_time}</b>\n\
         📡 WS جاهز وسيتصل خلال 30 ثانية…\n\
         🔗 https://www.mexc.com/exchange/{sym}",
        sym = listing.symbol,
        open_time = format_time_ms(listing.open_time_ms),
    );
    telegram::send_html(config, &msg).await;
}
