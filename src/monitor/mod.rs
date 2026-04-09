pub mod announcement;
pub mod deposit;
pub mod price;
pub mod processor;
pub mod symbol;

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct NewCoinEvent {
    pub symbol: String,
    pub base: String,
    pub source: DiscoverySource,
    pub discovered_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiscoverySource {
    /// Live — trading already open (from WS miniTicker)
    ExchangeInfo,
    /// Pre-listing — deposit just enabled (from REST capital/config)
    DepositOpen,
    /// Pre-listing — official announcement with known open time
    Announcement,
}

impl NewCoinEvent {
    pub fn new_exchange(symbol: &str, base: &str) -> Self {
        NewCoinEvent {
            symbol: symbol.to_owned(),
            base: base.to_owned(),
            source: DiscoverySource::ExchangeInfo,
            discovered_at_ms: now_ms(),
        }
    }

    pub fn new_deposit(coin: &str) -> Self {
        NewCoinEvent {
            symbol: format!("{coin}USDT"),
            base: coin.to_owned(),
            source: DiscoverySource::DepositOpen,
            discovered_at_ms: now_ms(),
        }
    }

    pub fn new_announcement(symbol: &str, base: &str) -> Self {
        NewCoinEvent {
            symbol: symbol.to_owned(),
            base: base.to_owned(),
            source: DiscoverySource::Announcement,
            discovered_at_ms: now_ms(),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
