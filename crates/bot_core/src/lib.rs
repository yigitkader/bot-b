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

    #[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
    pub struct OrderBook {
        pub best_bid: Option<BookLevel>,
        pub best_ask: Option<BookLevel>,
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
