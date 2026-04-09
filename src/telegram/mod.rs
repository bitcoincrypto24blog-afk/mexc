use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::Serialize;
use tokio::time::sleep;
use tracing::{error, warn};

use crate::config::Config;

const TG_BASE: &str = "https://api.telegram.org";
const MAX_RETRIES: u32 = 3;

#[derive(Serialize)]
struct SendMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
    parse_mode: &'a str,
    disable_web_page_preview: bool,
}

/// Send an HTML-formatted Telegram message. Non-blocking — retries up to 3×.
pub async fn send_html(config: &Config, text: &str) {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("TG client build");

    let url = format!(
        "{TG_BASE}/bot{}/sendMessage",
        config.telegram_bot_token
    );

    let body = SendMessage {
        chat_id: &config.telegram_chat_id,
        text,
        parse_mode: "HTML",
        disable_web_page_preview: true,
    };

    for attempt in 1..=MAX_RETRIES {
        match client.post(&url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => {
                let status = resp.status();
                let err_body = resp.text().await.unwrap_or_default();
                warn!("Telegram send failed ({status}): {err_body} [attempt {attempt}]");
            }
            Err(e) => {
                error!("Telegram HTTP error: {e} [attempt {attempt}]");
            }
        }
        sleep(Duration::from_millis(500 * attempt as u64)).await;
    }
}

/// Send the startup status message when the bot boots.
pub async fn send_startup(config: &Config) {
    let msg = format!(
        "🚀 <b>MEXC Sniper Bot</b> — تشغيل\n\
         ════════════════════════\n\
         ✅ مراقبة الرموز الجديدة  : <b>نشطة</b>\n\
         ✅ مراقبة الإيداعات       : <b>نشطة</b>\n\
         ✅ WebSocket السعر         : <b>جاهز</b>\n\
         ✅ CoinMarketCap           : <b>متصل</b>\n\
         ✅ إشعارات تيليجرام        : <b>تعمل ✓</b>\n\
         ════════════════════════\n\
         ⚡ السوق: <b>Spot USDT فقط</b>\n\
         📡 الوضع: <b>مراقبة مستمرة بالميلي ثانية</b>"
    );
    send_html(config, &msg).await;
}
