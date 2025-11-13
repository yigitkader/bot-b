// Library for trading bot - makes tests easily discoverable
// Consolidated modules for minimal, clean structure

pub mod app_init;
pub mod config;
pub mod constants;
pub mod exchange; // NEW: consolidates binance_exec/rest/ws
pub mod exec;
pub mod monitor;
pub mod order;
pub mod position_manager;
pub mod processor; // NEW: consolidates quote_gen/symbol_proc/discovery
pub mod qmel;
pub mod risk;
pub mod strategy; // Now includes direction_selector
pub mod types;
pub mod utils;

// Additional modules
pub mod logger;

// Backward compatibility - old module names (deprecated, use exchange instead)
pub use self::exchange as binance_exec;
pub use self::exchange as binance_rest;
pub use self::exchange as binance_ws;

// Tests are now embedded in modules via #[path] includes
// No separate test files needed

// Re-export key types
pub use config::*;
pub use constants::*;
pub use types::*;
