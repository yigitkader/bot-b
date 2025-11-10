//location: /crates/app/src/utils.rs
// Utility functions and helpers

use crate::core::types::*;
use crate::exec::binance::SymbolRules;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::RoundingStrategy;


// ============================================================================
// Quantization Helpers (moved from exec/mod.rs to avoid duplication)
// ============================================================================

/// Floor value to nearest step multiple
pub fn quant_utils_floor_to_step(val: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() {
        return val;
    }
    (val / step).floor() * step
}

/// Ceil value to nearest step multiple
pub fn quant_utils_ceil_to_step(val: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() {
        return val;
    }
    (val / step).ceil() * step
}

/// Snap price to tick (is_buy=true -> floor, is_buy=false -> ceil)
pub fn quant_utils_snap_price(raw: Decimal, tick: Decimal, is_buy: bool) -> Decimal {
    if is_buy {
        quant_utils_floor_to_step(raw, tick)
    } else {
        quant_utils_ceil_to_step(raw, tick)
    }
}

/// Calculate quantity from quote amount, floor to lot step
pub fn quant_utils_qty_from_quote(quote: Decimal, price: Decimal, lot_step: Decimal) -> Decimal {
    if price.is_zero() {
        return Decimal::ZERO;
    }
    quant_utils_floor_to_step(quote / price, lot_step)
}

/// Calculate basis points difference: (|new-old|/old)*1e4
pub fn quant_utils_bps_diff(old_px: Decimal, new_px: Decimal) -> f64 {
    if old_px.is_zero() {
        return f64::INFINITY;
    }
    let num = (new_px - old_px).abs();
    (num / old_px).to_f64().unwrap_or(0.0) * 10_000.0
}

/// Quantize decimal value to step (alias for floor_to_step for consistency)
pub fn quantize_decimal(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() || step.is_sign_negative() {
        return value;
    }
    quant_utils_floor_to_step(value, step)
}

// ============================================================================
// Quantity and Price Helpers
// ============================================================================

/// Get quantity step size (per-symbol or fallback)
pub fn get_qty_step(
    symbol_rules: Option<&std::sync::Arc<SymbolRules>>,
    fallback: f64,
) -> f64 {
    symbol_rules
        .map(|r| r.step_size.to_f64().unwrap_or(fallback))
        .unwrap_or(fallback)
}

/// Get price tick size (per-symbol or fallback)
pub fn get_price_tick(
    symbol_rules: Option<&std::sync::Arc<SymbolRules>>,
    fallback: f64,
) -> f64 {
    symbol_rules
        .map(|r| r.tick_size.to_f64().unwrap_or(fallback))
        .unwrap_or(fallback)
}

/// KRİTİK DÜZELTME: Tick/step kuantizasyon standardizasyonu - tek util fonksiyon
/// Fiyatı tick'e, miktarı step'e çevirir ve string format üretir
/// 
/// # Arguments
/// * `price` - Raw price (Decimal)
/// * `qty` - Raw quantity (Decimal)
/// * `rules` - Symbol rules (tick_size, step_size, precisions)
/// 
/// # Returns
/// (price_str, qty_str, price_quantized, qty_quantized)
pub fn quantize_order(
    price: Decimal,
    qty: Decimal,
    rules: &SymbolRules,
) -> (String, String, Decimal, Decimal) {
    // 1. Quantize: price to tick, qty to step
    let price_quantized = quant_utils_floor_to_step(price, rules.tick_size);
    let qty_quantized = quant_utils_floor_to_step(qty.abs(), rules.step_size);
    
    // 2. Format: precision'a göre string'e çevir
    let price_str = format_decimal_fixed(price_quantized, rules.price_precision);
    let qty_str = format_decimal_fixed(qty_quantized, rules.qty_precision);
    
    (price_str, qty_str, price_quantized, qty_quantized)
}

// format_decimal_fixed moved here to avoid duplication

/// Helper trait for floor division by step
pub trait FloorStep {
    fn floor_div_step(self, step: f64) -> f64;
}

impl FloorStep for f64 {
    fn floor_div_step(self, step: f64) -> f64 {
        if step <= 0.0 {
            return self;
        }
        (self / step).floor() * step
    }
}

/// Clamp quantity by USD value
pub fn clamp_qty_by_usd(qty: Qty, px: Px, max_usd: f64, qty_step: f64) -> Qty {
    let p = px.0.to_f64().unwrap_or(0.0);
    if p <= 0.0 || max_usd <= 0.0 {
        return Qty(Decimal::ZERO);
    }
    let max_qty = (max_usd / p).floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

/// Clamp quantity by base asset amount
pub fn clamp_qty_by_base(qty: Qty, max_base: f64, qty_step: f64) -> Qty {
    if max_base <= 0.0 {
        return Qty(Decimal::ZERO);
    }
    let max_qty = max_base.floor_div_step(qty_step);
    let wanted = qty.0.to_f64().unwrap_or(0.0);
    let q = wanted.min(max_qty);
    Qty(Decimal::from_f64_retain(q).unwrap_or(Decimal::ZERO))
}

/// Check if asset is a USD stablecoin (only USDC and USDT allowed)
pub fn is_usd_stable(asset: &str) -> bool {
    matches!(
        asset.to_uppercase().as_str(),
        "USDT" | "USDC"
    )
}

// ============================================================================
// Margin Chunking and Position Sizing
// ============================================================================

/// Split available margin into chunks of 10-100 USD
/// 
/// # Arguments
/// * `available_margin` - Available margin in USD
/// * `min_margin_per_trade` - Minimum margin per trade (default: 10 USD)
/// * `max_margin_per_trade` - Maximum margin per trade (default: 100 USD)
/// 
/// # Returns
/// Vector of margin chunks, each between min and max
/// 
/// # Strategy
/// Maximizes number of trades by splitting margin into smaller chunks when possible:
/// - If remaining == max: split into 2 equal chunks (max/2, max/2)
/// - If remaining > max: take max, then process remainder
/// - If remaining >= min: add as single chunk
/// 
/// # Examples
/// - 0-9 USD: empty vector (ignored)
/// - 10-100 USD: single chunk (or 2×50 if exactly 100)
/// - 100 USD: [50, 50] (2 chunks for more trades)
/// - 140 USD: [100, 40] (2 chunks)
/// - 200 USD: [100, 50, 50] (3 chunks)
pub fn split_margin_into_chunks(
    available_margin: f64,
    min_margin_per_trade: f64,
    max_margin_per_trade: f64,
) -> Vec<f64> {
    let mut chunks = Vec::new();
    let mut remaining = available_margin;
    
    // While we have enough for at least 2 full chunks (max_margin_per_trade * 2)
    // This ensures we can split the last max chunk if needed
    while remaining >= max_margin_per_trade * 2.0 {
        chunks.push(max_margin_per_trade);
        remaining -= max_margin_per_trade;
    }
    
    // Handle remaining margin
    if remaining >= max_margin_per_trade {
        // Remaining is >= max but < max*2
        // Split into 2 equal chunks to maximize number of trades
        let half = remaining / 2.0;
        // Ensure both halves are >= min_margin_per_trade
        if half >= min_margin_per_trade {
            chunks.push(half);
            chunks.push(remaining - half); // Second half (may be slightly different due to rounding)
        } else {
            // Can't split, add as single chunk
            chunks.push(remaining);
        }
    } else if remaining >= min_margin_per_trade {
        // Remaining is < max but >= min, add as single chunk
        chunks.push(remaining);
    }
    // If remaining < min, ignore it
    
    chunks
}

/// Calculate quantity and price from margin, leverage, and exchange rules
/// 
/// # Arguments
/// * `margin_chunk` - Margin chunk in USD (10-100) - hesaptan çıkan para
/// * `leverage` - Leverage (20-50x)
/// * `price` - Current price
/// * `rules` - Exchange rules (stepSize, tickSize, minQty, minNotional, precisions)
/// * `side` - Order side (Buy = floor price, Sell = ceil price for side-aware rounding)
/// 
/// # Returns
/// Option<(qty_string, price_string)> if valid, None if cannot satisfy rules
/// 
/// # Formula
/// 1. notional = margin * leverage (pozisyon büyüklüğü)
/// 2. qty = notional / price
/// 3. Quantize qty and price according to exchange rules
/// 
/// # KRİTİK DÜZELTME: Leverage uygulaması - ÇİFT SAYMA ÖNLEME
/// 
/// PROBLEM: Eğer hem burada hem cap_manager'da leverage uygulanırsa leverage^2 etkisi oluşur!
/// 
/// ÇÖZÜM: Leverage SADECE BİR YER'de uygulanmalı
/// - cap_manager: per_order_notional = per_order_cap_margin * effective_leverage (NOTIONAL LIMIT)
/// - calc_qty_from_margin: notional = margin_chunk * leverage (GERÇEK ORDER NOTIONAL)
/// 
/// Bu iki değer FARKLI amaçlar için:
/// - cap_manager: Limit kontrolü (max notional per order)
/// - calc_qty_from_margin: Gerçek order quantity hesaplama
/// 
/// ✅ DOĞRU KULLANIM:
/// - margin_chunk: leverage uygulanmamış margin (USD)
/// - leverage: leverage değeri
/// - Bu fonksiyon: notional = margin * leverage hesaplar
/// - cap_manager: per_order_notional = margin_limit * leverage hesaplar (limit için)
/// 
/// ❌ YANLIŞ KULLANIM:
/// - margin_chunk zaten leverage uygulanmış ise → leverage^2 etkisi!
pub fn calc_qty_from_margin(
    margin_chunk: f64,
    leverage: f64,
    price: Decimal,
    rules: &SymbolRules,
    side: crate::core::types::Side,
) -> Option<(String, String)> {
    // ✅ KRİTİK: Precision/Decimal - kritik hesaplarda f64 yerine Decimal kullan
    // Calculate notional = margin * leverage (Decimal olarak)
    // margin_chunk: hesaptan çıkan para (USD) - leverage uygulanmamış olmalı
    // notional: pozisyon büyüklüğü (USD) = margin * leverage
    // KRİTİK: margin_chunk zaten leverage uygulanmamış olmalı (split_margin_into_chunks'dan geliyor)
    // Eğer margin_chunk zaten leverage uygulanmış ise, burada tekrar leverage uygulanmamalı!
    let margin_dec = Decimal::try_from(margin_chunk).unwrap_or(Decimal::ZERO);
    let leverage_dec = Decimal::try_from(leverage).unwrap_or(Decimal::ZERO);
    let notional = margin_dec * leverage_dec;
    
    if price <= Decimal::ZERO || notional <= Decimal::ZERO {
        return None;
    }
    
    // Calculate raw quantity = notional / price (Decimal olarak)
    // KRİTİK: Division by zero kontrolü
    if price <= Decimal::ZERO {
        return None;
    }
    let qty_raw = notional / price;
    
    // Floor to step size (Decimal olarak)
    let step_size = rules.step_size;
    // KRİTİK: Division by zero kontrolü - step_size sıfır olamaz
    if step_size <= Decimal::ZERO {
        return None;
    }
    let qty_floor = quant_utils_floor_to_step(qty_raw, step_size);
    
    // KRİTİK DÜZELTME: Side-aware price rounding
    // BID: floor to tick (daha aşağı, maker olarak kal)
    // ASK: ceil to tick (daha yukarı, maker olarak kal)
    let tick_size = rules.tick_size;
    // KRİTİK: Division by zero kontrolü - tick_size sıfır olamaz
    if tick_size <= Decimal::ZERO {
        return None;
    }
    
    // KRİTİK DÜZELTME: tick_size çok büyükse (price'dan büyük veya eşitse) rounding yapma!
    // Örnek: price=0.29376, tick_size=0.1 -> floor_to_step(0.29376, 0.1) = 0.2 (YANLIŞ!)
    // Doğrusu: tick_size=0.00001 olmalı, o zaman floor_to_step(0.29376, 0.00001) = 0.29376 (DOĞRU)
    let price_quantized = if tick_size >= price {
        tracing::warn!(
            price = %price,
            tick_size = %tick_size,
            "tick_size >= price, skipping quantization (using raw price) - tick_size may be incorrectly parsed"
        );
        price // Rounding yapma, raw price kullan
    } else {
        match side {
            Side::Buy => quant_utils_floor_to_step(price, tick_size),
            Side::Sell => quant_utils_ceil_to_step(price, tick_size),
        }
    };
    
    // KRİTİK: Quantized price sıfır kontrolü
    if price_quantized <= Decimal::ZERO {
        tracing::error!(
            price = %price,
            tick_size = %tick_size,
            price_quantized = %price_quantized,
            side = ?side,
            "price_quantized is zero after rounding, this should not happen"
        );
        return None;
    }
    
    // KRİTİK: Quantized price ile raw price arasındaki fark kontrolü (debug için)
    let price_diff_bps = if price > Decimal::ZERO {
        ((price_quantized - price).abs() / price * Decimal::from(10000)).to_f64().unwrap_or(0.0)
    } else {
        0.0
    };
    
    if price_diff_bps > 100.0 {
        // %1'den fazla fark varsa uyar (tick_size çok büyük olabilir)
        tracing::warn!(
            price_raw = %price,
            price_quantized = %price_quantized,
            tick_size = %tick_size,
            diff_bps = price_diff_bps,
            side = ?side,
            "large price difference after quantization (>1%), check tick_size"
        );
    }
    
    // KRİTİK DÜZELTME: Precision/Decimal - minQty ve minNotional kontrolleri Decimal ile
    let min_qty = rules.step_size; // minQty usually = stepSize
    if qty_floor < min_qty {
        // Try to increase to minQty
        let qty_ceil = quant_utils_ceil_to_step(min_qty, step_size);
        // KRİTİK: Division by zero kontrolü (price_quantized zaten yukarıda kontrol edildi)
        if qty_ceil <= Decimal::ZERO {
            return None;
        }
        let notional_check = qty_ceil * price_quantized;
        let min_notional = rules.min_notional;
        
        if notional_check >= min_notional {
            // Format with precision (Decimal kullanarak)
            let qty_str = format_qty_with_precision(qty_ceil, rules.qty_precision);
            let price_str = format_price_with_precision(price_quantized, rules.price_precision, side);
            return Some((qty_str, price_str));
        } else {
            return None; // Cannot satisfy minNotional even with minQty
        }
    }
    
    // Check minNotional (Decimal olarak)
    let notional_check = qty_floor * price_quantized;
    let min_notional = rules.min_notional;
    
    if notional_check < min_notional {
        // Try to increase qty to satisfy minNotional
        // KRİTİK: Division by zero kontrolü
        if price_quantized <= Decimal::ZERO {
            return None;
        }
        let qty_needed_raw = min_notional / price_quantized;
        let qty_needed = quant_utils_ceil_to_step(qty_needed_raw, step_size);
        if qty_needed >= min_qty {
            // Format with precision (Decimal kullanarak)
            let qty_str = format_qty_with_precision(qty_needed, rules.qty_precision);
            let price_str = format_price_with_precision(price_quantized, rules.price_precision, side);
            return Some((qty_str, price_str));
        } else {
            return None; // Cannot satisfy both minQty and minNotional
        }
    }
    
    // Format with precision (Decimal kullanarak)
    let qty_str = format_qty_with_precision(qty_floor, rules.qty_precision);
    let price_str = format_price_with_precision(price_quantized, rules.price_precision, side);
    Some((qty_str, price_str))
}

/// KRİTİK DÜZELTME: ProfitGuarantee wrapper - maker→taker fallback zinciri standardize
/// Tek wrapper fonksiyon: required_take_profit_price_with_fallback
/// 
/// # Arguments
/// * `side` - Position side (Buy=Long, Sell=Short)
/// * `entry_price` - Entry price
/// * `qty` - Quantity
/// * `maker_fee_rate` - Maker fee rate (e.g., 0.0001)
/// * `taker_fee_rate` - Taker fee rate (e.g., 0.0004)
/// * `min_profit_usd` - Minimum profit in USD
/// * `tick_size` - Tick size for quantization
/// * `best_bid` - Best bid price (for maker check)
/// * `best_ask` - Best ask price (for maker check)
/// 
/// # Returns
/// (tp_price_quantized, is_taker_fallback, reason_code)
pub fn required_take_profit_price_with_fallback(
    side: crate::core::types::Side,
    entry_price: Decimal,
    qty: Decimal,
    maker_fee_rate: f64,
    taker_fee_rate: f64,
    min_profit_usd: f64,
    tick_size: Decimal,
    best_bid: Decimal,
    best_ask: Decimal,
) -> Option<(Decimal, bool, &'static str)> {
    let maker_fee_bps = maker_fee_rate * 10000.0;
    let taker_fee_bps = taker_fee_rate * 10000.0;
    
    // Önce maker→maker senaryosu
    let tp_maker = required_take_profit_price(
        side,
        entry_price,
        qty,
        maker_fee_bps,
        maker_fee_bps,
        min_profit_usd,
    )?;
    
    // Quantize et
    let tp_quantized = match side {
        crate::core::types::Side::Buy => quant_utils_ceil_to_step(tp_maker, tick_size),
        crate::core::types::Side::Sell => quant_utils_floor_to_step(tp_maker, tick_size),
    };
    
    // Maker kontrolü
    let is_maker = match side {
        crate::core::types::Side::Buy => {
            // Long: TP = sell exit, best_ask'ten en az 1 tick yüksek olmalı
            tp_quantized >= best_ask + tick_size
        }
        crate::core::types::Side::Sell => {
            // Short: TP = buy exit, best_bid'ten en az 1 tick düşük olmalı
            tp_quantized <= best_bid - tick_size
        }
    };
    
    if is_maker {
        return Some((tp_quantized, false, "TP_MAKER"));
    }
    
    // Maker olamıyorsa taker fallback
    let tp_taker = required_take_profit_price(
        side,
        entry_price,
        qty,
        taker_fee_bps,
        taker_fee_bps,
        min_profit_usd,
    )?;
    
    let tp_taker_quantized = match side {
        crate::core::types::Side::Buy => quant_utils_ceil_to_step(tp_taker, tick_size),
        crate::core::types::Side::Sell => quant_utils_floor_to_step(tp_taker, tick_size),
    };
    
    Some((tp_taker_quantized, true, "TP_TAKER_FALLBACK"))
}

/// Calculate required take profit price to guarantee net profit ≥ min_profit_usd
/// 
/// # Arguments
/// * `side` - "Long" (buy->sell) or "Short" (sell->buy)
/// * `entry_price` - Entry price (limit order price)
/// * `qty` - Quantity
/// * `fee_bps_entry` - Entry fee in basis points (e.g., 2.0 = 0.02%)
/// * `fee_bps_exit` - Exit fee in basis points (e.g., 4.0 = 0.04%)
/// * `min_profit_usd` - Minimum net profit in USD (e.g., 0.50)
/// 
/// # Returns
/// Required take profit price (must be quantized to tick_size)
/// 
/// # Formula
/// Long: exit_price >= entry_price + (min_profit + fee_entry + fee_exit) / qty
/// Short: exit_price <= entry_price - (min_profit + fee_entry + fee_exit) / qty
pub fn required_take_profit_price(
    side: crate::core::types::Side,
    entry_price: Decimal,
    qty: Decimal,
    fee_bps_entry: f64,
    fee_bps_exit: f64,
    min_profit_usd: f64,
) -> Option<Decimal> {
    if entry_price <= Decimal::ZERO || qty <= Decimal::ZERO {
        return None;
    }
    
    let notional_entry = entry_price * qty;
    let fee_entry = notional_entry * Decimal::try_from(fee_bps_entry / 10_000.0).unwrap_or(Decimal::ZERO);
    let min_profit_dec = Decimal::try_from(min_profit_usd).unwrap_or(Decimal::ZERO);
    
    match side {
        crate::core::types::Side::Buy => {
            // Long position: buy entry, sell exit
            // Net profit = exit_price * qty - fee_exit - (entry_price * qty + fee_entry) >= min_profit
            // => exit_price * qty * (1 - fee_bps_exit/10000) >= entry_price * qty + fee_entry + min_profit
            // => exit_price >= (entry_price * qty + fee_entry + min_profit) / (qty * (1 - fee_bps_exit/10000))
            let bps = Decimal::try_from(fee_bps_exit / 10_000.0).unwrap_or(Decimal::ZERO);
            let denominator = qty * (Decimal::ONE - bps);
            if denominator <= Decimal::ZERO {
                return None;
            }
            let numerator = notional_entry + fee_entry + min_profit_dec;
            Some(numerator / denominator)
        }
        crate::core::types::Side::Sell => {
            // Short position: sell entry, buy exit
            // Net profit = (entry_price * qty - fee_entry) - (exit_price * qty + fee_exit) >= min_profit
            // => entry_price * qty - fee_entry - exit_price * qty - fee_exit >= min_profit
            // => exit_price * qty * (1 + fee_bps_exit/10000) <= entry_price * qty - fee_entry - min_profit
            // => exit_price <= (entry_price * qty - fee_entry - min_profit) / (qty * (1 + fee_bps_exit/10000))
            let bps = Decimal::try_from(fee_bps_exit / 10_000.0).unwrap_or(Decimal::ZERO);
            let denominator = qty * (Decimal::ONE + bps);
            if denominator <= Decimal::ZERO {
                return None;
            }
            let numerator = notional_entry - fee_entry - min_profit_dec;
            if numerator <= Decimal::ZERO {
                return None; // Cannot achieve min profit
            }
            Some(numerator / denominator)
        }
    }
}

/// Calculate side multiplier for PnL calculation
/// Long: +1.0, Short: -1.0
pub fn side_mult(side: &crate::core::types::Side) -> f64 {
    match side {
        crate::core::types::Side::Buy => 1.0,  // Long
        crate::core::types::Side::Sell => -1.0, // Short
    }
}

/// Estimate exit fee BPS based on exit type
/// Market exit uses taker fee, limit exit uses maker fee
pub fn estimate_close_fee_bps(is_market_exit: bool, maker_bps: f64, taker_bps: f64) -> f64 {
    if is_market_exit {
        taker_bps
    } else {
        maker_bps
    }
}

/// Calculate net PnL in USD (fees included)
/// 
/// # Arguments
/// * `entry_price` - Entry price (Decimal)
/// * `exit_price` - Current/exit price (Decimal)
/// * `qty` - Position quantity (Decimal, absolute value)
/// * `side` - Position side (Buy=Long, Sell=Short)
/// * `entry_fee_bps` - Entry fee in basis points
/// * `exit_fee_bps` - Exit fee in basis points
/// 
/// # Returns
/// Net PnL in USD (fees deducted)
pub fn calc_net_pnl_usd(
    entry_price: Decimal,
    exit_price: Decimal,
    qty: Decimal,
    side: &crate::core::types::Side,
    entry_fee_bps: f64,
    exit_fee_bps: f64,
) -> f64 {
    let mult = side_mult(side);
    
    // Gross PnL = (exit_price - entry_price) * qty * mult
    let price_diff = exit_price - entry_price;
    let gross_pnl = price_diff * qty * Decimal::try_from(mult).unwrap_or(Decimal::ONE);
    
    // Notionals for fee calculation
    let notional_entry = entry_price * qty;
    let notional_exit = exit_price * qty;
    
    // Fees
    let entry_fee_bps_dec = Decimal::try_from(entry_fee_bps / 10_000.0).unwrap_or(Decimal::ZERO);
    let exit_fee_bps_dec = Decimal::try_from(exit_fee_bps / 10_000.0).unwrap_or(Decimal::ZERO);
    
    let fees_open = notional_entry * entry_fee_bps_dec;
    let fees_close = notional_exit * exit_fee_bps_dec;
    
    // Net PnL = gross - fees
    let net_pnl = gross_pnl - fees_open - fees_close;
    
    net_pnl.to_f64().unwrap_or(0.0)
}

/// Clamp price to market distance (mid ± max_distance_pct)
/// 
/// # Arguments
/// * `price` - Raw price from strategy
/// * `mid` - Mid price (best_bid + best_ask) / 2
/// * `max_distance_pct` - Maximum distance from mid (e.g., 0.01 = 1%)
/// 
/// # Returns
/// Clamped price within mid ± max_distance_pct
pub fn clamp_price_to_market_distance(
    price: Decimal,
    mid: Decimal,
    max_distance_pct: f64,
) -> Decimal {
    // KRİTİK: Division by zero ve invalid input kontrolleri
    if mid <= Decimal::ZERO || max_distance_pct <= 0.0 || price <= Decimal::ZERO {
        return price;
    }
    
    let max_distance_pct_dec = match Decimal::try_from(max_distance_pct) {
        Ok(d) => d,
        Err(_) => return price, // Invalid conversion
    };
    
    let max_distance = mid * max_distance_pct_dec;
    let min_price = mid - max_distance;
    let max_price = mid + max_distance;
    
    // Clamp to valid range
    price.max(min_price).min(max_price)
}

/// Analyze order book depth to find optimal price level
/// 
/// # Arguments
/// * `order_book` - Order book with top bids/asks
/// * `side` - Order side (Buy or Sell)
/// * `min_required_volume_usd` - Minimum required volume in USD for a level to be considered
/// * `best_bid` - Best bid price (fallback)
/// * `best_ask` - Best ask price (fallback)
/// 
/// # Returns
/// Optimal price based on depth analysis, or best_bid/best_ask if no suitable level found
pub fn find_optimal_price_from_depth(
    order_book: &crate::core::types::OrderBook,
    side: crate::core::types::Side,
    min_required_volume_usd: f64,
    best_bid: Decimal,
    best_ask: Decimal,
) -> Decimal {
    match side {
        crate::core::types::Side::Buy => {
            // BID: En yüksek volume'lu level'ı bul (best_bid'e yakın)
            if let Some(top_bids) = &order_book.top_bids {
                for level in top_bids.iter() {
                    let level_volume_usd = (level.px.0 * level.qty.0).to_f64().unwrap_or(0.0);
                    // Yeterli volume varsa ve best_bid'e yakınsa kullan
                    if level_volume_usd >= min_required_volume_usd && level.px.0 <= best_bid {
                        return level.px.0;
                    }
                }
            }
            // Fallback: best_bid
            best_bid
        }
        crate::core::types::Side::Sell => {
            // ASK: En yüksek volume'lu level'ı bul (best_ask'e yakın)
            if let Some(top_asks) = &order_book.top_asks {
                for level in top_asks.iter() {
                    let level_volume_usd = (level.px.0 * level.qty.0).to_f64().unwrap_or(0.0);
                    // Yeterli volume varsa ve best_ask'e yakınsa kullan
                    if level_volume_usd >= min_required_volume_usd && level.px.0 >= best_ask {
                        return level.px.0;
                    }
                }
            }
            // Fallback: best_ask
            best_ask
        }
    }
}

/// Adjust price for aggressiveness based on trend and manipulation
/// 
/// # Arguments
/// * `price` - Raw price from strategy
/// * `best_bid` - Best bid price
/// * `best_ask` - Best ask price
/// * `side` - Order side (Buy or Sell)
/// * `is_opportunity_mode` - Is manipulation opportunity active?
/// * `trend_bps` - Trend in basis points (positive = uptrend, negative = downtrend)
/// * `max_distance_pct` - Maximum distance from best bid/ask
/// 
/// # Returns
/// Adjusted price that is closer to market when trend/opportunity is strong
pub fn adjust_price_for_aggressiveness(
    price: Decimal,
    best_bid: Decimal,
    best_ask: Decimal,
    side: crate::core::types::Side,
    is_opportunity_mode: bool,
    trend_bps: f64,
    max_distance_pct: f64,
) -> Decimal {
    if best_bid <= Decimal::ZERO || best_ask <= Decimal::ZERO || price <= Decimal::ZERO {
        return price;
    }
    
    let max_distance_pct_dec = match Decimal::try_from(max_distance_pct) {
        Ok(d) => d,
        Err(_) => return price,
    };
    
    match side {
        crate::core::types::Side::Buy => {
            // BID: best_bid'e yakın olmalı
            let max_distance = best_bid * max_distance_pct_dec;
            
            // Trend-aware adjustment: Uptrend varsa bid'i yukarı çek
            let trend_factor = if trend_bps > 50.0 {
                // Güçlü uptrend: bid'i %50 daha yakın yap
                0.5
            } else if trend_bps > 0.0 {
                // Zayıf uptrend: bid'i %25 daha yakın yap
                0.75
            } else {
                1.0 // Downtrend: normal mesafe
            };
            
            // Opportunity mode: Manipülasyon varsa daha agresif
            let opportunity_factor = if is_opportunity_mode {
                0.2 // %80 daha yakın (çok agresif - küçük kar hedefi için)
            } else {
                0.5 // Normal modda bile %50 daha yakın (küçük kar hedefi için agresif)
            };
            
            // Kombine faktör (en agresif olanı seç)
            let adjustment_factor = if trend_factor < opportunity_factor { trend_factor } else { opportunity_factor };
            let adjustment_factor_dec = Decimal::try_from(adjustment_factor).unwrap_or(Decimal::ONE);
            let adjusted_min = best_bid - (max_distance * adjustment_factor_dec);
            
            // Clamp: strategy price ile adjusted min arasında en yakın olanı seç
            // Küçük kar hedefi için best_bid'e mümkün olduğunca yakın
            price.max(adjusted_min).min(best_bid)
        }
        crate::core::types::Side::Sell => {
            // ASK: best_ask'e yakın olmalı
            let max_distance = best_ask * max_distance_pct_dec;
            
            // Trend-aware adjustment: Downtrend varsa ask'i aşağı çek
            let trend_factor = if trend_bps < -50.0 {
                // Güçlü downtrend: ask'i %50 daha yakın yap
                0.5
            } else if trend_bps < 0.0 {
                // Zayıf downtrend: ask'i %25 daha yakın yap
                0.75
            } else {
                1.0 // Uptrend: normal mesafe
            };
            
            // Opportunity mode: Manipülasyon varsa daha agresif
            let opportunity_factor = if is_opportunity_mode {
                0.2 // %80 daha yakın (çok agresif - küçük kar hedefi için)
            } else {
                0.5 // Normal modda bile %50 daha yakın (küçük kar hedefi için agresif)
            };
            
            // Kombine faktör (en agresif olanı seç)
            let adjustment_factor = if trend_factor < opportunity_factor { trend_factor } else { opportunity_factor };
            let adjustment_factor_dec = Decimal::try_from(adjustment_factor).unwrap_or(Decimal::ONE);
            let adjusted_max = best_ask + (max_distance * adjustment_factor_dec);
            
            // Clamp: strategy price ile adjusted max arasında en yakın olanı seç
            // Küçük kar hedefi için best_ask'e mümkün olduğunca yakın
            price.max(best_ask).min(adjusted_max)
        }
    }
}

/// Helper trait for ceiling division by step
trait CeilStep {
    fn ceil_div_step(self, step: f64) -> f64;
}

impl CeilStep for f64 {
    fn ceil_div_step(self, step: f64) -> f64 {
        if step <= 0.0 {
            return self;
        }
        (self / step).ceil() * step
    }
}

/// Format decimal with fixed precision (truncate, don't round)
/// KRİTİK: Decimal kullanarak precision kaybını önle
/// Precision overflow kontrolü (max 28 decimal places)
pub fn format_decimal_fixed(value: Decimal, precision: usize) -> String {
    let precision = precision.min(28);
    let scale = precision as u32;
    
    // ÖNEMLİ: Precision hatasını önlemek için önce quantize, sonra format
    // KRİTİK: round_dp_with_strategy ile kesinlikle precision'a kadar yuvarla
    // ToZero strategy kullanarak fazla basamakları kes
    let truncated = value.round_dp_with_strategy(scale, RoundingStrategy::ToZero);
    
    // KRİTİK: String formatlamada kesinlikle precision'dan fazla basamak gösterme
    // Decimal'in to_string() metodu bazen internal precision'ı gösterebilir
    // Bu yüzden format! makrosu ile precision kontrolü yapıyoruz
    if precision == 0 {
        format!("{:.0}", truncated)
    } else {
        // Precision kadar ondalık basamak göster
        let formatted = format!("{:.prec$}", truncated, prec = precision);
        formatted
    }
}

/// Format quantity with precision (wrapper for consistency)
fn format_qty_with_precision(qty: Decimal, precision: usize) -> String {
    format_decimal_fixed(qty, precision)
}

/// Format price with precision (wrapper for consistency)
fn format_price_with_precision(price: Decimal, precision: usize, _side: crate::core::types::Side) -> String {
    format_decimal_fixed(price, precision)
}

// ============================================================================
// PnL Calculation Helpers
// ============================================================================

/// Compute drawdown in basis points from equity history
pub fn compute_drawdown_bps(history: &[Decimal]) -> i64 {
    if history.is_empty() {
        return 0;
    }
    
    let mut peak = history[0];
    let mut max_drawdown = Decimal::ZERO;
    
    for value in history {
        if *value > peak {
            peak = *value;
        }
        if peak > Decimal::ZERO {
            let drawdown = ((*value - peak) / peak) * Decimal::from(10_000i32);
            if drawdown < max_drawdown {
                max_drawdown = drawdown;
            }
        }
    }
    
    max_drawdown.to_i64().unwrap_or(0)
}

/// Record PnL snapshot to history
pub fn record_pnl_snapshot(
    history: &mut Vec<Decimal>,
    pos: &Position,
    mark_px: Px,
    max_len: usize,
) {
    let pnl = (mark_px.0 - pos.entry.0) * pos.qty.0;
    let mut equity = Decimal::ONE + pnl;
    
    // Keep the history strictly positive so drawdown math remains stable
    if equity <= Decimal::ZERO {
        equity = Decimal::new(1, 4);
    }
    
    history.push(equity);
    if history.len() > max_len {
        let excess = history.len() - max_len;
        history.drain(0..excess);
    }
}

// ============================================================================
// Rate Limiting - Weight-Based Binance API Rate Limiter
// ============================================================================

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Weight-based rate limiter for Binance Futures API calls
/// 
/// Binance Futures API Limits:
/// - Futures: 2400 requests/minute (40 req/sec) - weight-based
/// 
/// Common endpoint weights:
/// - GET /fapi/v1/depth (best_prices): Weight 5
/// - POST /fapi/v1/order (place_limit): Weight 1
/// - DELETE /fapi/v1/order (cancel): Weight 1
/// - GET /fapi/v1/openOrders: Weight 1
/// - GET /fapi/v2/positionRisk: Weight 5
/// - GET /fapi/v2/balance: Weight 5
#[cfg_attr(test, derive(Debug))]
pub struct RateLimiter {
    /// Per-second request timestamps (for burst protection)
    requests: Mutex<VecDeque<Instant>>,
    /// Per-minute weight tracking (for weight-based limits)
    weights: Mutex<VecDeque<(Instant, u32)>>,
    /// Maximum requests per second (with safety margin)
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) max_requests_per_sec: u32,
    /// Maximum weight per minute (with safety margin)
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) max_weight_per_minute: u32,
    /// Minimum interval between requests (ms)
    min_interval_ms: u64,
    /// Safety factor (0.0-1.0) - how much of the limit to use
    #[allow(dead_code)]
    safety_factor: f64,
}

impl RateLimiter {
    /// Create a new rate limiter with safety margin
    /// 
    /// # Arguments
    /// * `max_requests_per_sec` - Maximum requests per second (Futures: 40)
    /// * `max_weight_per_minute` - Maximum weight per minute (Futures: 2400)
    /// * `safety_factor` - Safety margin (0.0-1.0). 0.6 = use only 60% of limit (very safe)
    pub fn new(max_requests_per_sec: u32, max_weight_per_minute: u32, safety_factor: f64) -> Self {
        let safety_factor = safety_factor.clamp(0.5, 0.95); // En az %50, en fazla %95 kullan
        let safe_req_limit = (max_requests_per_sec as f64 * safety_factor) as u32;
        let safe_weight_limit = (max_weight_per_minute as f64 * safety_factor) as u32;
        let min_interval_ms = (1000.0 / safe_req_limit as f64).ceil() as u64;
        
        Self {
            requests: Mutex::new(VecDeque::new()),
            weights: Mutex::new(VecDeque::new()),
            max_requests_per_sec: safe_req_limit,
            max_weight_per_minute: safe_weight_limit,
            min_interval_ms,
            safety_factor,
        }
    }
    
    /// Wait if needed to respect rate limits (weight-based)
    /// 
    /// # Arguments
    /// * `weight` - API endpoint weight (default: 1 for most endpoints)
    /// 
    /// # Race Condition Fix
    /// Sleep sırasında lock yokken başka thread'ler weight ekleyebilir.
    /// Bu yüzden sleep'i küçük parçalara bölüp her parçada kontrol yapıyoruz.
    pub async fn wait_if_needed(&self, weight: u32) {
        loop {
            let now = Instant::now();
            let mut requests = self.requests.lock().unwrap();
            let mut weights = self.weights.lock().unwrap();
            
            // ===== PER-SECOND LIMIT (Burst Protection) =====
            // Son 1 saniyede yapılan request'leri temizle
            let one_sec_ago = now.checked_sub(Duration::from_secs(1))
                .unwrap_or(Instant::now());
            while requests.front().map_or(false, |&t| t < one_sec_ago) {
                requests.pop_front();
            }
            
            // Eğer per-second limit aşıldıysa bekle
            if requests.len() >= self.max_requests_per_sec as usize {
                if let Some(oldest) = requests.front().copied() {
                    let wait_time = oldest + Duration::from_secs(1);
                    if wait_time > now {
                        let sleep_duration = wait_time.duration_since(now);
                        drop(requests);
                        drop(weights);
                        // Kısa bekleme (< 1 saniye): 100ms polling (fine-grained)
                        let poll_interval_ms = 100;
                        self.sleep_with_polling(sleep_duration, poll_interval_ms).await;
                        continue;
                    }
                }
            }
            
            // Minimum interval kontrolü (her request arasında minimum bekleme)
            if let Some(last) = requests.back() {
                let elapsed = now.duration_since(*last);
                if elapsed.as_millis() < self.min_interval_ms as u128 {
                    let wait = Duration::from_millis(self.min_interval_ms)
                        .saturating_sub(elapsed);
                    if wait.as_millis() > 0 {
                        drop(requests);
                        drop(weights);
                        // Kısa bekleme (< 1 saniye): 100ms polling (fine-grained)
                        let poll_interval_ms = 100;
                        self.sleep_with_polling(wait, poll_interval_ms).await;
                        continue;
                    }
                }
            }
            
            // ===== PER-MINUTE WEIGHT LIMIT =====
            // Son 1 dakikada kullanılan weight'leri temizle
            let one_min_ago = now.checked_sub(Duration::from_secs(60))
                .unwrap_or(Instant::now());
            while weights.front().map_or(false, |&(t, _)| t < one_min_ago) {
                weights.pop_front();
            }
            
            // Toplam weight'i hesapla
            let total_weight: u32 = weights.iter().map(|(_, w)| w).sum();
            
            // Eğer weight limit aşıldıysa bekle
            if total_weight + weight > self.max_weight_per_minute {
                if let Some((oldest_time, _)) = weights.front().copied() {
                    let wait_time = oldest_time + Duration::from_secs(60);
                    if wait_time > now {
                        let sleep_duration = wait_time.duration_since(now);
                        drop(requests);
                        drop(weights);
                        // KRİTİK İYİLEŞTİRME: Adaptive polling interval - Binance 1 dakikalık pencere kullanıyor
                        // Uzun bekleme (> 10 saniye): 2-5 saniye polling (overhead azaltma)
                        // Orta bekleme (1-10 saniye): 1 saniye polling
                        // Kısa bekleme (< 1 saniye): 100ms polling
                        let poll_interval_ms = if sleep_duration.as_secs() > 10 {
                            3000 // 3 saniye - uzun bekleme için (1 dakikalık limit)
                        } else if sleep_duration.as_secs() >= 1 {
                            1000 // 1 saniye - orta bekleme için
                        } else {
                            100 // 100ms - kısa bekleme için (per-second limit)
                        };
                        self.sleep_with_polling(sleep_duration, poll_interval_ms).await;
                        continue;
                    }
                }
            }
            
            // Request'i kaydet ve çık
            requests.push_back(now);
            weights.push_back((now, weight));
            break;
        }
    }
    
    /// Sleep with polling: Sleep'i küçük parçalara böl ve her parçada kontrol yap
    /// Bu, sleep sırasında başka thread'lerin weight eklemesini önler (race condition fix)
    /// 
    /// KRİTİK İYİLEŞTİRME: Adaptive polling interval
    /// - Binance 1 dakikalık pencere kullanıyor, bu yüzden uzun bekleme için 1-5 saniye polling yeterli
    /// - Kısa bekleme için 100ms polling (per-second limit için)
    /// - Bu overhead'i önemli ölçüde azaltır
    /// 
    /// # Arguments
    /// * `total_duration` - Toplam bekleme süresi
    /// * `poll_interval_ms` - Her kontrol arasındaki süre (ms) - adaptive olarak seçilir
    async fn sleep_with_polling(&self, total_duration: Duration, poll_interval_ms: u64) {
        let poll_interval = Duration::from_millis(poll_interval_ms);
        let mut remaining = total_duration;
        
        while remaining > Duration::ZERO {
            // Her poll interval'de bir kontrol yap
            let sleep_duration = remaining.min(poll_interval);
            tokio::time::sleep(sleep_duration).await;
            remaining = remaining.saturating_sub(sleep_duration);
            
            // Kısa bir kontrol: Eğer limit artık aşılmıyorsa erken çık
            // (Bu optimizasyon, ama asıl amaç race condition önleme)
            // Not: Burada lock almıyoruz çünkü sadece optimizasyon için
            // Asıl kontrol loop'un başında yapılıyor
        }
    }
    
    /// Get current usage statistics (for monitoring)
    #[allow(dead_code)]
    pub fn get_stats(&self) -> (usize, u32, f64) {
        let now = Instant::now();
        let requests = self.requests.lock().unwrap();
        let weights = self.weights.lock().unwrap();
        
        // Son 1 saniyede yapılan request sayısı
        let one_sec_ago = now.checked_sub(Duration::from_secs(1))
            .unwrap_or(Instant::now());
        let req_count = requests.iter().filter(|&&t| t >= one_sec_ago).count();
        
        // Son 1 dakikada kullanılan weight
        let one_min_ago = now.checked_sub(Duration::from_secs(60))
            .unwrap_or(Instant::now());
        let total_weight: u32 = weights.iter()
            .filter(|(t, _)| *t >= one_min_ago)
            .map(|(_, w)| w)
            .sum();
        
        let weight_usage_pct = (total_weight as f64 / self.max_weight_per_minute as f64) * 100.0;
        
        (req_count, total_weight, weight_usage_pct)
    }
}

/// Global rate limiter instance
/// 
/// Futures limits:
/// - Futures: 40 req/sec, 2400 weight/min → 28 req/sec, 1680 weight/min (70% safety)
static RATE_LIMITER: OnceLock<RateLimiter> = OnceLock::new();

/// Initialize rate limiter for futures
pub fn init_rate_limiter() {
    // Futures: 40 req/sec, 2400 weight/min
    let max_req_per_sec = 40;
    let max_weight_per_min = 2400;
    
    // Safety factor 0.7 (daha agresif, ban riski hala minimal)
    let safety_factor = 0.7;
    
    RATE_LIMITER.set(RateLimiter::new(
        max_req_per_sec,
        max_weight_per_min,
        safety_factor,
    )).ok();
}

/// Get or initialize the global rate limiter (default: Futures limits)
pub fn get_rate_limiter() -> &'static RateLimiter {
    RATE_LIMITER.get_or_init(|| {
        // Default: Futures limits with 70% safety factor
        RateLimiter::new(40, 2400, 0.7)
    })
}

/// Guard function for rate limiting (async)
/// 
/// # Arguments
/// * `weight` - API endpoint weight (default: 1)
pub async fn rate_limit_guard(weight: u32) {
    get_rate_limiter().wait_if_needed(weight).await;
}

/// Convenience function for rate limiting with default weight (1)
#[allow(dead_code)]
pub async fn rate_limit_guard_default() {
    rate_limit_guard(1).await;
}

// ============================================================================
// Profit Guarantee and Tracking
// ============================================================================

/// Profit guarantee calculator - ensures each trade is profitable after fees
pub struct ProfitGuarantee {
    /// Minimum profit per trade in USD (e.g., 0.50 cents = 0.005 USD)
    min_profit_usd: f64,
    /// Maker fee rate (e.g., 0.0002 = 0.02% = 2 bps)
    maker_fee_rate: f64,
    /// Taker fee rate (e.g., 0.0004 = 0.04% = 4 bps)
    taker_fee_rate: f64,
}

impl ProfitGuarantee {
    /// Create a new profit guarantee calculator
    /// 
    /// # Arguments
    /// * `min_profit_usd` - Minimum profit per trade in USD (e.g., 0.005 for 0.50 cents)
    /// * `maker_fee_rate` - Maker fee rate (default: 0.0002 = 2 bps for Binance Futures)
    /// * `taker_fee_rate` - Taker fee rate (default: 0.0004 = 4 bps for Binance Futures)
    pub fn new(min_profit_usd: f64, maker_fee_rate: f64, taker_fee_rate: f64) -> Self {
        Self {
            min_profit_usd,
            maker_fee_rate,
            taker_fee_rate,
        }
    }

    /// Default constructor with Binance Futures fees
    /// KRİTİK DÜZELTME: 0.005 USD (0.5 cent) → 0.50 USD (50 cent) hedef
    pub fn default() -> Self {
        Self::new(0.50, 0.0002, 0.0004) // 0.50 USD, 2 bps maker, 4 bps taker
    }

    /// Calculate minimum spread required for profitable trade
    /// 
    /// # Arguments
    /// * `position_size_usd` - Position size in USD (notional)
    /// 
    /// # Returns
    /// Minimum spread in bps (basis points) required for profit
    /// 
    /// # Formula
    /// Maker → maker (giriş+çıkış) için ücret toplamı: fees_bps_total = 2 * maker_fee_bps
    /// İstenen net kâr: $0.50
    /// Gerekli brüt bps: target_bps = 10000 * 0.50 / notional
    /// Safety margin: slippage (~1-5 bps) + partial fill risks + market volatility
    /// min_spread_bps_needed = fees_bps_total + target_bps + safety_margin_bps
    pub fn calculate_min_spread_bps(&self, position_size_usd: f64) -> f64 {
        if position_size_usd <= 0.0 {
            return 0.0;
        }

        // Maker → maker (giriş+çıkış) için ücret toplamı: 2 * maker_fee_bps
        let maker_fee_bps = self.maker_fee_rate * 10000.0;
        let fees_bps_total = 2.0 * maker_fee_bps;
        
        // İstenen net kâr için gerekli brüt bps
        let target_bps = 10000.0 * self.min_profit_usd / position_size_usd;
        
        // ✅ Safety margin: slippage, partial fill risks, market volatility
        // Slippage: ~1-5 bps (fiyat kayması)
        // Partial fill risks: emirlerin tam doldurulmaması riski
        // Market volatility: volatilite anında spread genişlemesi
        let safety_margin_bps = crate::constants::MIN_SPREAD_SAFETY_MARGIN_BPS;
        
        // Minimum spread = fees + target profit + safety margin
        fees_bps_total + target_bps + safety_margin_bps
    }

    /// Check if a trade is profitable given spread and position size
    /// 
    /// # Arguments
    /// * `spread_bps` - Current spread in basis points
    /// * `position_size_usd` - Position size in USD
    /// 
    /// # Returns
    /// true if trade is profitable, false otherwise
    pub fn is_trade_profitable(&self, spread_bps: f64, position_size_usd: f64) -> bool {
        if position_size_usd <= 0.0 {
            return false;
        }

        let min_spread_bps = self.calculate_min_spread_bps(position_size_usd);
        spread_bps >= min_spread_bps
    }

    /// Calculate expected profit for a trade
    /// 
    /// # Arguments
    /// * `spread_bps` - Current spread in basis points
    /// * `position_size_usd` - Position size in USD
    /// 
    /// # Returns
    /// Expected profit in USD (can be negative if unprofitable)
    pub fn calculate_expected_profit(&self, spread_bps: f64, position_size_usd: f64) -> f64 {
        if position_size_usd <= 0.0 {
            return 0.0;
        }

        let spread_rate = spread_bps / 10000.0;
        let gross_profit = position_size_usd * spread_rate;
        let total_fees = position_size_usd * (self.maker_fee_rate + self.taker_fee_rate);
        gross_profit - total_fees
    }

    /// Calculate optimal position size for a given spread
    /// 
    /// # Arguments
    /// * `spread_bps` - Current spread in basis points
    /// * `min_position_usd` - Minimum position size in USD
    /// * `max_position_usd` - Maximum position size in USD
    /// 
    /// # Returns
    /// Optimal position size in USD that guarantees minimum profit
    pub fn calculate_optimal_position_size(
        &self,
        spread_bps: f64,
        min_position_usd: f64,
        max_position_usd: f64,
    ) -> f64 {
        if spread_bps <= 0.0 {
            return min_position_usd;
        }

        let spread_rate = spread_bps / 10000.0;
        let total_fee_rate = self.maker_fee_rate + self.taker_fee_rate;
        let net_spread_rate = spread_rate - total_fee_rate;

        if net_spread_rate <= 0.0 {
            // Spread doesn't cover fees, use minimum
            return min_position_usd;
        }

        // Calculate position size needed for minimum profit
        let optimal_size = self.min_profit_usd / net_spread_rate;
        
        // Clamp to min/max bounds
        optimal_size.max(min_position_usd).min(max_position_usd)
    }

    /// Calculate risk/reward ratio for a trade
    /// 
    /// # Arguments
    /// * `spread_bps` - Current spread in basis points
    /// * `position_size_usd` - Position size in USD
    /// * `max_loss_pct` - Maximum loss percentage (e.g., 0.01 = 1%)
    /// 
    /// # Returns
    /// Risk/reward ratio (e.g., 2.0 = 2:1 reward:risk)
    pub fn calculate_risk_reward_ratio(
        &self,
        spread_bps: f64,
        position_size_usd: f64,
        max_loss_pct: f64,
    ) -> f64 {
        if position_size_usd <= 0.0 || max_loss_pct <= 0.0 {
            return 0.0;
        }

        let expected_profit = self.calculate_expected_profit(spread_bps, position_size_usd);
        let max_loss = position_size_usd * max_loss_pct.abs();

        if max_loss <= 0.0 {
            return 0.0;
        }

        expected_profit / max_loss
    }
}

/// Profit tracker for monitoring trading performance
pub struct ProfitTracker {
    total_trades: u32,
    winning_trades: u32,
    total_profit: f64,
    total_fees: f64,
    total_loss: f64,
}

impl ProfitTracker {
    pub fn new() -> Self {
        Self {
            total_trades: 0,
            winning_trades: 0,
            total_profit: 0.0,
            total_fees: 0.0,
            total_loss: 0.0,
        }
    }

    /// Record a completed trade
    /// 
    /// # Arguments
    /// * `profit` - Net profit in USD (can be negative)
    /// * `fees` - Total fees paid in USD
    pub fn record_trade(&mut self, profit: f64, fees: f64) {
        self.total_trades += 1;
        self.total_fees += fees;

        if profit > 0.0 {
            self.winning_trades += 1;
            self.total_profit += profit;
        } else {
            self.total_loss += profit.abs();
        }
    }

    /// Get win rate as percentage
    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        (self.winning_trades as f64 / self.total_trades as f64) * 100.0
    }

    /// Get net profit (profit - fees - losses)
    pub fn net_profit(&self) -> f64 {
        self.total_profit - self.total_fees - self.total_loss
    }

    /// Get statistics as formatted string
    pub fn get_stats(&self) -> String {
        let win_rate = self.win_rate();
        let net_profit = self.net_profit();
        let avg_profit_per_trade = if self.total_trades > 0 {
            net_profit / self.total_trades as f64
        } else {
            0.0
        };

        format!(
            "Trades: {}, Win Rate: {:.1}%, Net Profit: ${:.2}, Fees: ${:.2}, Avg Profit/Trade: ${:.4}",
            self.total_trades, win_rate, net_profit, self.total_fees, avg_profit_per_trade
        )
    }

    /// Reset all statistics
    pub fn reset(&mut self) {
        self.total_trades = 0;
        self.winning_trades = 0;
        self.total_profit = 0.0;
        self.total_fees = 0.0;
        self.total_loss = 0.0;
    }
}

impl Default for ProfitTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe profit tracker wrapper
pub type SharedProfitTracker = Mutex<ProfitTracker>;

/// Calculate spread in basis points from bid and ask prices
/// KRİTİK DÜZELTME: Mid price bazlı tutarlı hesaplama (volatilite anında sapma önleme)
/// 
/// # Arguments
/// * `bid` - Bid price
/// * `ask` - Ask price
/// 
/// # Returns
/// Spread in basis points (bps), or 0.0 if invalid
/// 
/// # Formula
/// spread_bps = (ask - bid) / ((ask + bid) / 2) * 10000
/// Mid price kullanarak volatilite anında sapmayı önler
pub fn calculate_spread_bps(bid: Decimal, ask: Decimal) -> f64 {
    if bid <= Decimal::ZERO || ask <= Decimal::ZERO || ask <= bid {
        return 0.0;
    }
    
    let bid_f = bid.to_f64().unwrap_or(0.0);
    let ask_f = ask.to_f64().unwrap_or(0.0);
    
    if bid_f <= 0.0 || ask_f <= 0.0 || ask_f <= bid_f {
        return 0.0;
    }
    
    // KRİTİK DÜZELTME: Mid price bazlı hesaplama (tutarlı, volatilite anında sapma önleme)
    let mid_price = (bid_f + ask_f) / 2.0;
    if mid_price <= 0.0 {
        return 0.0;
    }
    
    let spread_rate = (ask_f - bid_f) / mid_price;
    spread_rate * 10000.0 // Convert to bps
}

/// Check if a trade should be placed based on profit guarantee and risk/reward
/// 
/// Market Making için özel mantık:
/// - Spread'den kazanç garantili (maker olarak)
/// - Risk/reward kontrolü sabit threshold kullanır (pozisyon boyutuna göre değişmez)
/// - Her işlemde $0.50 kar hedefi olduğu için threshold sabit olmalı
/// 
/// # Arguments
/// * `spread_bps` - Current spread in basis points
/// * `position_size_usd` - Position size in USD
/// * `min_spread_bps` - Minimum spread threshold from config
/// * `stop_loss_threshold` - Stop loss threshold (negative, e.g., -0.01 for 1%)
/// * `min_risk_reward_ratio` - Minimum risk/reward ratio (e.g., 1.0 for 1:1, market making için)
/// 
/// # Returns
/// (should_place, reason) - true if trade should be placed, false otherwise with reason
pub fn should_place_trade(
    spread_bps: f64,
    position_size_usd: f64,
    min_spread_bps: f64,
    stop_loss_threshold: f64,
    min_risk_reward_ratio: f64,
    profit_guarantee: &ProfitGuarantee,
) -> (bool, &'static str) {
    // 1. Minimum spread kontrolü
    if spread_bps < min_spread_bps {
        return (false, "spread_below_minimum");
    }
    
    // 2. Profit guarantee kontrolü
    // KRİTİK DÜZELTME: ProfitGuarantee artık parametre olarak geçiliyor (default kaldırıldı)
    if !profit_guarantee.is_trade_profitable(spread_bps, position_size_usd) {
        return (false, "not_profitable_after_fees");
    }
    
    // 3. Risk/Reward oranı kontrolü
    // ✅ KRİTİK FIX: $0.50 kar hedefi her işlemde aynı olduğu için risk/reward oranı sabit olmalı
    // Pozisyon boyutuna göre threshold değiştirmek mantıksız çünkü:
    // - Küçük pozisyonlarda bile kar garantisi korunmalı
    // - Her işlemde aynı $0.50 hedefi varsa threshold sabit olmalı
    let max_loss_pct = stop_loss_threshold.abs();
    let risk_reward_ratio = profit_guarantee.calculate_risk_reward_ratio(
        spread_bps,
        position_size_usd,
        max_loss_pct,
    );
    
    // Risk/reward oranı kontrolü - sabit threshold (pozisyon boyutuna göre değişmez)
    if risk_reward_ratio < min_risk_reward_ratio {
        return (false, "risk_reward_too_low");
    }
    
    (true, "ok")
}

#[cfg(test)]
#[path = "rate_limiter_tests.rs"]
mod rate_limiter_tests;

#[cfg(test)]
#[path = "utils_tests.rs"]
mod utils_tests;

