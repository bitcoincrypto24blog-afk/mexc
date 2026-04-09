use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    // MEXC credentials — OPTIONAL (only for deposit pre-listing monitor)
    pub mexc_api_key: Option<String>,
    pub mexc_secret_key: Option<String>,
    // Telegram
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    // CoinMarketCap
    pub cmc_api_key: String,
    // Deposit poll interval (ms) — only used if API keys are set
    pub deposit_poll_ms: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        Ok(Config {
            mexc_api_key: std::env::var("MEXC_API_KEY").ok(),
            mexc_secret_key: std::env::var("MEXC_SECRET_KEY").ok(),
            telegram_bot_token: std::env::var("TELEGRAM_BOT_TOKEN")
                .context("TELEGRAM_BOT_TOKEN not set")?,
            telegram_chat_id: std::env::var("TELEGRAM_CHAT_ID")
                .context("TELEGRAM_CHAT_ID not set")?,
            cmc_api_key: std::env::var("CMC_API_KEY")
                .context("CMC_API_KEY not set")?,
            deposit_poll_ms: std::env::var("DEPOSIT_POLL_MS")
                .unwrap_or_else(|_| "800".into())
                .parse()
                .unwrap_or(800),
        })
    }

    /// Returns true if private deposit monitoring can run
    pub fn deposit_monitor_enabled(&self) -> bool {
        self.mexc_api_key.is_some() && self.mexc_secret_key.is_some()
    }
}
