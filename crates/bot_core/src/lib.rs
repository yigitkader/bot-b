//location: /crates/bot_core/src/lib.rs
pub mod types {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Copy, Debug, Serialize, Deserialize)]
    pub struct Px(pub Decimal);
    #[derive(Clone, Copy, Debug, Serialize, Deserialize)]
    pub struct Qty(pub Decimal);

    #[derive(Clone, Copy, Debug, Serialize, Deserialize)]
    pub struct BookLevel {
        pub px: Px,
        pub qty: Qty,
    }

    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    pub struct OrderBook {
        pub best_bid: Option<BookLevel>,
        pub best_ask: Option<BookLevel>,
        // KRİTİK İYİLEŞTİRME: Top-K levels for more reliable imbalance calculation
        // When available, imbalance uses sum of top-K levels instead of just best bid/ask
        pub top_bids: Option<Vec<BookLevel>>, // Top-K bid levels (sorted by price descending)
        pub top_asks: Option<Vec<BookLevel>>, // Top-K ask levels (sorted by price ascending)
    }

    #[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
    pub enum Side {
        Buy,
        Sell,
    }

    #[derive(Clone, Copy, Debug, Serialize, Deserialize)]
    pub enum Tif {
        Gtc,
        Ioc,
        PostOnly,
    }

    #[derive(Clone, Copy, Debug, Serialize, Deserialize)]
    pub struct Quote {
        pub bid: Px,
        pub ask: Px,
        pub size: Qty,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Position {
        pub symbol: String,
        pub qty: Qty,
        pub entry: Px,
        pub leverage: u32,
        pub liq_px: Option<Px>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Fill {
        pub id: i64,
        pub symbol: String,
        pub side: Side,
        pub qty: Qty,
        pub price: Px,
        pub timestamp: u64,
    }

    #[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
    pub struct Quotes {
        pub bid: Option<(Px, Qty)>,
        pub ask: Option<(Px, Qty)>,
    }
}

#[cfg(test)]
mod tests {
    use super::types::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    #[test]
    fn test_orderbook_creation() {
        let ob = OrderBook {
            best_bid: Some(BookLevel {
                px: Px(dec!(50000)),
                qty: Qty(dec!(0.1)),
            }),
            best_ask: Some(BookLevel {
                px: Px(dec!(50010)),
                qty: Qty(dec!(0.1)),
            }),
            top_bids: None,
            top_asks: None,
        };

        assert!(ob.best_bid.is_some());
        assert!(ob.best_ask.is_some());
        assert_eq!(ob.best_bid.unwrap().px.0, dec!(50000));
        assert_eq!(ob.best_ask.unwrap().px.0, dec!(50010));
    }

    #[test]
    fn test_orderbook_default() {
        let ob = OrderBook::default();
        assert!(ob.best_bid.is_none());
        assert!(ob.best_ask.is_none());
    }

    #[test]
    fn test_quotes_creation() {
        let quotes = Quotes {
            bid: Some((Px(dec!(50000)), Qty(dec!(0.1)))),
            ask: Some((Px(dec!(50010)), Qty(dec!(0.1)))),
        };

        assert!(quotes.bid.is_some());
        assert!(quotes.ask.is_some());
    }

    #[test]
    fn test_quotes_default() {
        let quotes = Quotes::default();
        assert!(quotes.bid.is_none());
        assert!(quotes.ask.is_none());
    }

    #[test]
    fn test_position_creation() {
        let pos = Position {
            symbol: "BTCUSDT".to_string(),
            qty: Qty(dec!(0.1)),
            entry: Px(dec!(50000)),
            leverage: 5,
            liq_px: Some(Px(dec!(45000))),
        };

        assert_eq!(pos.symbol, "BTCUSDT");
        assert_eq!(pos.qty.0, dec!(0.1));
        assert_eq!(pos.entry.0, dec!(50000));
        assert_eq!(pos.leverage, 5);
        assert!(pos.liq_px.is_some());
    }

    #[test]
    fn test_side_equality() {
        assert_eq!(Side::Buy, Side::Buy);
        assert_eq!(Side::Sell, Side::Sell);
        assert_ne!(Side::Buy, Side::Sell);
    }

    #[test]
    fn test_serialization() {
        let pos = Position {
            symbol: "BTCUSDT".to_string(),
            qty: Qty(dec!(0.1)),
            entry: Px(dec!(50000)),
            leverage: 5,
            liq_px: Some(Px(dec!(45000))),
        };

        // Test that serialization works
        let json = serde_json::to_string(&pos).unwrap();
        let deserialized: Position = serde_json::from_str(&json).unwrap();

        assert_eq!(pos.symbol, deserialized.symbol);
        assert_eq!(pos.qty.0, deserialized.qty.0);
        assert_eq!(pos.entry.0, deserialized.entry.0);
        assert_eq!(pos.leverage, deserialized.leverage);
    }
}
