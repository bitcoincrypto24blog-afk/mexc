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

// ── Internal parsed listing ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PendingListing {
    pub symbol: String,
    pub base: String,
    /// Unix ms of trading open — 0 means TBD
    pub open_time_ms: u64,
    pub discovered_at_ms: u64,
    pub title: String,
    pub article_url: String,
    pub warned: bool,
    pub ws_opened: bool,
}

/// Raw scraped announcement item
#[derive(Debug, Clone)]
struct ScrapedItem {
    title: String,
    article_url: String,
    /// "about 2 hours ago" / "1 day ago" etc.
    age_text: String,
}

const HOURS_24_MS: u64 = 24 * 60 * 60 * 1_000;

// ── Main loop ─────────────────────────────────────────────────────────────────

pub async fn run(
    config: Arc<Config>,
    tx: UnboundedSender<NewCoinEvent>,
    known: Arc<DashSet<String>>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                     AppleWebKit/537.36 (KHTML, like Gecko) \
                     Chrome/124.0.0.0 Safari/537.36")
        .build()?;

    info!("📢 Announcement monitor started (poll every 60s | window: last 24h)");

    let mut pending: HashMap<String, PendingListing> = HashMap::new();
    let mut seen_titles: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        let now = now_ms();

        match fetch_announcements(&client).await {
            Ok(items) => {
                for item in items {
                    // 24h filter based on age_text
                    if !is_within_24h(&item.age_text) {
                        continue;
                    }

                    if seen_titles.contains(&item.title) {
                        continue;
                    }

                    // Skip futures/perpetual/leverage
                    if is_futures_title(&item.title) {
                        seen_titles.insert(item.title.clone());
                        continue;
                    }

                    let symbol = match extract_symbol(&item.title) {
                        Some(s) => s,
                        None => {
                            seen_titles.insert(item.title.clone());
                            continue;
                        }
                    };

                    if pending.contains_key(&symbol) {
                        seen_titles.insert(item.title.clone());
                        continue;
                    }

                    seen_titles.insert(item.title.clone());

                    // Try title first, then fetch article body for datetime
                    let mut open_time_ms = parse_datetime_from_text(&item.title).unwrap_or(0);

                    if open_time_ms == 0 {
                        if let Ok(body) = fetch_text(&client, &item.article_url).await {
                            open_time_ms = parse_datetime_from_text(&body).unwrap_or(0);
                        }
                    }

                    let base = symbol.trim_end_matches("USDT").to_string();

                    let listing = PendingListing {
                        symbol: symbol.clone(),
                        base: base.clone(),
                        open_time_ms,
                        discovered_at_ms: now,
                        title: item.title.clone(),
                        article_url: item.article_url.clone(),
                        warned: false,
                        ws_opened: false,
                    };

                    info!(
                        "📢 New listing: {} | open: {}",
                        symbol,
                        if open_time_ms > 0 { format_time_ms(open_time_ms) } else { "TBD".to_string() }
                    );

                    send_announcement_alert(config.as_ref(), &listing).await;
                    known.insert(symbol.clone());
                    let ev = NewCoinEvent::new_exchange(&symbol, &base);
                    let _ = tx.send(ev);
                    pending.insert(symbol, listing);
                }
            }
            Err(e) => error!("Announcement fetch error: {e}"),
        }

        // Process pending
        let now = now_ms();

        for listing in pending.values_mut() {
            if listing.open_time_ms == 0 {
                continue;
            }

            let ms_left = listing.open_time_ms.saturating_sub(now);
            let mins_left = ms_left / 60_000;

            if !listing.warned && ms_left > 0 && mins_left <= 5 {
                listing.warned = true;
                send_5min_warning(config.as_ref(), listing).await;
            }

            if !listing.ws_opened && ms_left > 0 && ms_left <= 30_000 {
                listing.ws_opened = true;
                info!(
                    "⚡ Opening WS tracker for {} — {}s until open",
                    listing.symbol,
                    ms_left / 1000
                );
                spawn_price_tracker(listing.symbol.clone(), config.clone());
            }

            if ms_left == 0 && !listing.ws_opened {
                listing.ws_opened = true;
                warn!("⚠️  {} — open time passed, spawning tracker late", listing.symbol);
                spawn_price_tracker(listing.symbol.clone(), config.clone());
            }
        }

        // Cleanup
        pending.retain(|_, l| {
            if l.open_time_ms > 0 {
                now < l.open_time_ms + 7_200_000
            } else {
                now < l.discovered_at_ms + HOURS_24_MS
            }
        });

        sleep(Duration::from_secs(60)).await;
    }
}

// ── HTML scraper ──────────────────────────────────────────────────────────────

/// Scrape the public MEXC announcements page (HTML) — no API key required.
/// Falls back to the Spot-filtered page and the support portal.
async fn fetch_announcements(client: &reqwest::Client) -> Result<Vec<ScrapedItem>> {
    let pages = [
        "https://www.mexc.com/announcements/new-listings",
        "https://www.mexc.com/announcements/new-listings/spot-18",
        "https://www.mexc.com/announcements/all",
    ];

    for url in &pages {
        match client.get(*url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let html = resp.text().await.unwrap_or_default();
                let items = parse_html_announcements(&html);
                if !items.is_empty() {
                    info!("📡 Scraped {} announcements from {}", items.len(), url);
                    return Ok(items);
                }
            }
            _ => {}
        }
    }

    Ok(vec![])
}

/// Parse article links, titles and age from raw HTML.
/// MEXC pages embed articles as:
///   <a href="/announcements/article/SLUG" title="TITLE">...</a>
///   followed shortly by  "about X hours ago" | "X days ago"
fn parse_html_announcements(html: &str) -> Vec<ScrapedItem> {
    let mut items = Vec::new();

    // Find every occurrence of /announcements/article/ href
    let mut search = html;
    loop {
        let anchor = match search.find("/announcements/article/") {
            Some(i) => i,
            None => break,
        };

        // Walk back to find start of href="
        let prefix = &search[..anchor];
        let href_start = match prefix.rfind("href=\"") {
            Some(i) => i + 6, // skip href="
            None => { search = &search[anchor + 1..]; continue; }
        };

        let slug_str = &search[href_start..];
        let slug_end = match slug_str.find('"') {
            Some(i) => i,
            None => { search = &search[anchor + 1..]; continue; }
        };
        let relative_url = &slug_str[..slug_end];
        let article_url = format!("https://www.mexc.com{}", relative_url);

        // Extract title from title="..." attribute or inner text
        let rest = &search[anchor..];
        let title = extract_title_from_nearby_html(rest);

        // Extract age text — look for "ago" near this anchor
        let age_window = &rest[..rest.len().min(2000)];
        let age_text = extract_age_text(age_window);

        if !title.is_empty() {
            items.push(ScrapedItem { title, article_url, age_text });
        }

        // Advance past this anchor to avoid duplicates from the same article
        let advance = slug_end + anchor + href_start;
        if advance == 0 { break; }
        search = &search[advance..];
    }

    // Deduplicate by article URL
    let mut seen_urls = std::collections::HashSet::new();
    items.retain(|i| seen_urls.insert(i.article_url.clone()));

    items
}

/// Extract title from the HTML near an article link.
/// Tries title="..." attribute first, then <h2>/<h3> text.
fn extract_title_from_nearby_html(html: &str) -> String {
    // Try title="..." in first 500 chars
    let window = &html[..html.len().min(500)];

    if let Some(ti) = window.find("title=\"") {
        let after = &window[ti + 7..];
        if let Some(end) = after.find('"') {
            let t = after[..end].trim().to_string();
            if t.len() > 10 {
                return html_decode(&t);
            }
        }
    }

    // Try >TEXT< between tags (strip HTML tags)
    let stripped = strip_tags(window);
    let line = stripped
        .lines()
        .map(|l| l.trim())
        .find(|l| l.len() > 15 && !l.starts_with('#') && !l.is_empty())
        .unwrap_or("")
        .to_string();

    html_decode(&line)
}

fn extract_age_text(html: &str) -> String {
    // Find "ago" and walk back to get the full phrase
    let window = &html[..html.len().min(3000)];
    if let Some(ago_pos) = window.find(" ago") {
        let before = &window[..ago_pos];
        // Walk back up to 30 chars to find the number + unit
        let start = before.len().saturating_sub(30);
        let phrase = &before[start..];
        let stripped = strip_tags(phrase).trim().to_string();
        // Take last meaningful part
        let parts: Vec<&str> = stripped.split_whitespace().collect();
        if parts.len() >= 2 {
            let n = parts.len();
            return format!("{} {} ago", parts[n-2], parts[n-1]);
        }
        return "hours ago".to_string(); // default: treat as recent
    }
    // If no "ago" found, treat as within 24h (to be safe)
    String::new()
}

/// Rough check: return true if within the last ~24h based on text like
/// "about 2 hours ago", "about 1 hour ago", "1 day ago", "23 hours ago"
fn is_within_24h(age_text: &str) -> bool {
    if age_text.is_empty() {
        return true; // no time info → include
    }

    let lo = age_text.to_lowercase();

    if lo.contains("hour") {
        // "about 5 hours ago" → always within 24h
        return true;
    }

    if lo.contains("minute") || lo.contains("second") {
        return true;
    }

    if lo.contains("day") {
        // Extract number of days
        let n: u64 = lo
            .split_whitespace()
            .find_map(|w| w.parse::<u64>().ok())
            .unwrap_or(2);
        return n <= 1; // "1 day ago" ≈ borderline, keep it
    }

    // "just now" etc.
    true
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn html_decode(s: &str) -> String {
    s.replace("&amp;", "&")
     .replace("&lt;", "<")
     .replace("&gt;", ">")
     .replace("&quot;", "\"")
     .replace("&#39;", "'")
     .replace("&nbsp;", " ")
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client.get(url).send().await?;
    Ok(resp.text().await?)
}

// ── Symbol extractor ──────────────────────────────────────────────────────────

fn extract_symbol(title: &str) -> Option<String> {
    let candidates = extract_between(title, '(', ')');

    // Priority 1: (XYZUSDT)
    for c in &candidates {
        let s = c.trim();
        if s.ends_with("USDT")
            && s.len() > 6
            && s.len() <= 20
            && !s.contains(' ')
            && s.chars().all(|ch| ch.is_ascii_alphanumeric())
        {
            return Some(s.to_string());
        }
    }

    // Priority 2: (ENM) → ENMUSDT
    for c in &candidates {
        let s = c.trim();
        if s.len() >= 2
            && s.len() <= 10
            && !s.contains(' ')
            && s.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
            && s.chars().next().map(|ch| ch.is_ascii_uppercase()).unwrap_or(false)
            && s != "UTC"
            && !s.chars().all(|ch| ch.is_ascii_digit())
        {
            return Some(format!("{}USDT", s));
        }
    }

    // Priority 3: XXX/USDT anywhere in title
    for word in title.split_whitespace() {
        let w = word.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '/');
        if let Some(ticker) = w.strip_suffix("/USDT") {
            if ticker.len() >= 2
                && ticker.len() <= 10
                && ticker.chars().all(|ch| ch.is_ascii_alphanumeric())
            {
                return Some(format!("{}USDT", ticker));
            }
        }
    }

    None
}

// ── Futures detector ──────────────────────────────────────────────────────────

fn is_futures_title(title: &str) -> bool {
    let lo = title.to_lowercase();
    lo.contains("futures")
        || lo.contains("perpetual")
        || lo.contains("leverage")
        || lo.contains("usdt-m")
        || title.contains("آجلة")
        || title.contains("رافعة")
        || title.contains("عقود مستقبلية")
        || title.contains("دائم")
}

// ── Datetime parsers ──────────────────────────────────────────────────────────

fn parse_datetime_from_text(text: &str) -> Option<u64> {
    // Pattern 1: ISO "YYYY-MM-DD HH:MM"
    let bytes = text.as_bytes();
    for i in 0..bytes.len().saturating_sub(15) {
        if text.is_char_boundary(i) {
            if let Some(ms) = try_parse_iso(&text[i..]) {
                return Some(ms);
            }
        }
    }
    // Pattern 2: "11 أبريل 2026، الساعة 11:00"
    if let Some(ms) = try_parse_day_first(text) {
        return Some(ms);
    }
    // Pattern 3: "Apr 11, 2026, 11:00" / "April 11, 2026 11:00"
    if let Some(ms) = try_parse_month_first(text) {
        return Some(ms);
    }
    None
}

fn try_parse_iso(s: &str) -> Option<u64> {
    if s.len() < 16 { return None; }
    let year: u32  = s.get(0..4)?.parse().ok()?;
    if s.as_bytes().get(4)? != &b'-' { return None; }
    let month: u32 = s.get(5..7)?.parse().ok()?;
    if s.as_bytes().get(7)? != &b'-' { return None; }
    let day: u32   = s.get(8..10)?.parse().ok()?;
    let sep = *s.as_bytes().get(10)?;
    if sep != b' ' && sep != b'T' { return None; }
    let hour: u32  = s.get(11..13)?.parse().ok()?;
    if s.as_bytes().get(13)? != &b':' { return None; }
    let min: u32   = s.get(14..16)?.parse().ok()?;
    if year < 2024 || month == 0 || month > 12 || day == 0 || day > 31
        || hour > 23 || min > 59 { return None; }
    Some(datetime_to_unix_ms(year, month, day, hour, min))
}

fn try_parse_day_first(text: &str) -> Option<u64> {
    let tokens: Vec<&str> = text
        .split(|c: char| c == ' ' || c == ',' || c == '،' || c == '\n' || c == '\t')
        .filter(|t| !t.is_empty())
        .collect();
    let len = tokens.len();
    if len < 4 { return None; }
    for i in 0..len.saturating_sub(3) {
        let day: u32 = match tokens[i].trim_matches(|c: char| !c.is_ascii_digit()).parse::<u32>() {
            Ok(d) if d >= 1 && d <= 31 => d,
            _ => continue,
        };
        let month_num = match month_name_to_num(tokens[i + 1]) {
            Some(m) => m,
            None => continue,
        };
        let year: u32 = match tokens[i + 2].trim_matches(|c: char| !c.is_ascii_digit()).parse::<u32>() {
            Ok(y) if y >= 2024 => y,
            _ => continue,
        };
        for j in (i + 3)..len {
            let t = tokens[j].trim_matches(|c: char| c == '(' || c == ')');
            if let Some(ms) = try_parse_hhmm_token(t, year, month_num, day) {
                return Some(ms);
            }
        }
    }
    None
}

fn try_parse_month_first(text: &str) -> Option<u64> {
    let tokens: Vec<&str> = text
        .split(|c: char| c == ' ' || c == ',' || c == '،' || c == '\n' || c == '\t')
        .filter(|t| !t.is_empty())
        .collect();
    let len = tokens.len();
    if len < 4 { return None; }
    for i in 0..len.saturating_sub(3) {
        let month_num = match month_name_to_num(tokens[i]) {
            Some(m) => m,
            None => continue,
        };
        let day: u32 = match tokens[i + 1].trim_matches(|c: char| !c.is_ascii_digit()).parse::<u32>() {
            Ok(d) if d >= 1 && d <= 31 => d,
            _ => continue,
        };
        let year: u32 = match tokens[i + 2].trim_matches(|c: char| !c.is_ascii_digit()).parse::<u32>() {
            Ok(y) if y >= 2024 => y,
            _ => continue,
        };
        for j in (i + 3)..len {
            let t = tokens[j].trim_matches(|c: char| c == '(' || c == ')');
            if let Some(ms) = try_parse_hhmm_token(t, year, month_num, day) {
                return Some(ms);
            }
        }
    }
    None
}

fn try_parse_hhmm_token(token: &str, year: u32, month: u32, day: u32) -> Option<u64> {
    if token.len() != 5 { return None; }
    if token.as_bytes().get(2)? != &b':' { return None; }
    let h: u32 = token.get(0..2)?.parse().ok()?;
    let m: u32 = token.get(3..5)?.parse().ok()?;
    if h > 23 || m > 59 { return None; }
    Some(datetime_to_unix_ms(year, month, day, h, m))
}

fn month_name_to_num(s: &str) -> Option<u32> {
    match s.trim() {
        "يناير"  | "January"   | "Jan" => Some(1),
        "فبراير" | "February"  | "Feb" => Some(2),
        "مارس"   | "March"     | "Mar" => Some(3),
        "أبريل"  | "ابريل"    | "April" | "Apr" => Some(4),
        "مايو"   | "May"                => Some(5),
        "يونيو"  | "June"      | "Jun" => Some(6),
        "يوليو"  | "July"      | "Jul" => Some(7),
        "أغسطس"  | "اغسطس"   | "August"  | "Aug" => Some(8),
        "سبتمبر" | "September" | "Sep" => Some(9),
        "أكتوبر" | "اكتوبر"  | "October"  | "Oct" => Some(10),
        "نوفمبر" | "November"  | "Nov" => Some(11),
        "ديسمبر" | "December"  | "Dec" => Some(12),
        _ => None,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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

fn datetime_to_unix_ms(year: u32, month: u32, day: u32, hour: u32, min: u32) -> u64 {
    let y = year as u64;
    let m = month as u64;
    let d = day as u64;
    let leap = |yr: u64| (yr % 4 == 0 && yr % 100 != 0) || yr % 400 == 0;
    let days_in_month = |yr: u64, mo: u64| -> u64 {
        match mo {
            1|3|5|7|8|10|12 => 31,
            4|6|9|11        => 30,
            2 => if leap(yr) { 29 } else { 28 },
            _ => 0,
        }
    };
    let mut days: u64 = 0;
    for yr in 1970..y { days += if leap(yr) { 366 } else { 365 }; }
    for mo in 1..m    { days += days_in_month(y, mo); }
    days += d - 1;
    (days * 86_400 + hour as u64 * 3_600 + min as u64 * 60) * 1_000
}

fn format_time_ms(ms: u64) -> String {
    let secs  = ms / 1_000;
    let mins  = (secs / 60) % 60;
    let hours = (secs / 3_600) % 24;
    let days  = secs / 86_400;
    if days > 0 { format!("{days}d {hours:02}:{mins:02} UTC") }
    else        { format!("{hours:02}:{mins:02} UTC") }
}

// ── Telegram alerts ───────────────────────────────────────────────────────────

async fn send_announcement_alert(config: &Config, listing: &PendingListing) {
    let now = now_ms();
    let (open_time_str, countdown_str) = if listing.open_time_ms > 0 {
        let ms_left    = listing.open_time_ms.saturating_sub(now);
        let hours_left = ms_left / 3_600_000;
        let mins_left  = (ms_left % 3_600_000) / 60_000;
        let cd = if hours_left > 0 {
            format!("{hours_left} ساعة و{mins_left} دقيقة")
        } else {
            format!("{mins_left} دقيقة")
        };
        (format_time_ms(listing.open_time_ms), cd)
    } else {
        ("TBD".to_string(), "غير محدد بعد".to_string())
    };

    let msg = format!(
        "📢 <b>{sym}</b> — إعلان إدراج رسمي على MEXC!\n\
         ══════════════════════════\n\
         🏷  الرمز          : <code>{sym}</code>\n\
         📅 وقت فتح التداول : <b>{open_time_str}</b>\n\
         ⏳ الوقت المتبقي   : <b>{countdown_str}</b>\n\
         ══════════════════════════\n\
         📌 {title}\n\
         ══════════════════════════\n\
         🔗 الإعلان : {article_url}\n\
         🔗 MEXC    : https://www.mexc.com/exchange/{sym}\n\
         ──────────────────────────\n\
         ⚡ البوت سيفتح WS قبل 30 ثانية من الفتح تلقائياً",
        sym         = listing.symbol,
        title       = listing.title,
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
        sym       = listing.symbol,
        open_time = format_time_ms(listing.open_time_ms),
    );
    telegram::send_html(config, &msg).await;
}
