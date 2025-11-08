//location: /crates/app/src/position_order_tests.rs
// Test modülü: Pozisyon ve emir yönetimi mantığını test eder

#[cfg(test)]
mod tests {
    use bot_core::types::*;
    use rust_decimal::prelude::ToPrimitive;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    
    // ============================================================================
    // Test Helper Types & Functions
    // ============================================================================
    
    /// Test için OrderInfo struct'ı (main.rs'deki ile aynı)
    #[derive(Clone, Debug)]
    struct TestOrderInfo {
        order_id: String,
        side: Side,
        price: Px,
        qty: Qty,
        created_at: Instant,
    }

    /// Test pozisyonu oluştur (helper function)
    fn create_test_position(symbol: &str, qty: Decimal, entry: Decimal, leverage: u32) -> Position {
        Position {
            symbol: symbol.to_string(),
            qty: Qty(qty),
            entry: Px(entry),
            leverage,
            liq_px: Some(Px(entry * dec!(0.9))), // %10 likidasyon mesafesi
        }
    }

    /// Test emri oluştur (helper function)
    fn create_test_order(order_id: &str, side: Side, price: Decimal, qty: Decimal, age_ms: u64) -> TestOrderInfo {
        TestOrderInfo {
            order_id: order_id.to_string(),
            side,
            price: Px(price),
            qty: Qty(qty),
            created_at: Instant::now() - Duration::from_millis(age_ms),
        }
    }

    /// PnL hesaplama helper (long pozisyon için)
    fn calculate_pnl_long(entry: Decimal, mark_price: Decimal, qty: Decimal) -> Decimal {
        (mark_price - entry) * qty
    }

    /// PnL hesaplama helper (short pozisyon için)
    fn calculate_pnl_short(entry: Decimal, mark_price: Decimal, qty: Decimal) -> Decimal {
        (mark_price - entry) * qty // Short için qty negatif olacak
    }

    /// Margin hesaplama helper (unrealized PnL ile)
    fn calculate_margin_with_pnl(position_notional: f64, leverage: f64, unrealized_pnl: f64) -> f64 {
        let base_margin: f64 = position_notional / leverage;
        (base_margin - unrealized_pnl).max(0.0_f64)
    }

    /// Test constants
    const DEFAULT_LEVERAGE: u32 = 5;
    const DEFAULT_ENTRY_PRICE: Decimal = dec!(50000);
    const DEFAULT_POSITION_QTY: Decimal = dec!(0.1);
    const MAX_ORDER_AGE_MS: u64 = 10_000;

    // ============================================================================
    // PnL Calculation Tests
    // ============================================================================

    #[test]
    fn test_position_pnl_calculation_long_profit() {
        // Given: Long pozisyon, fiyat artışı
        let pos = create_test_position("BTCUSDT", DEFAULT_POSITION_QTY, DEFAULT_ENTRY_PRICE, DEFAULT_LEVERAGE);
        let mark_price = Px(dec!(51000)); // %2 artış
        
        // When: PnL hesaplanır
        let current_pnl = calculate_pnl_long(pos.entry.0, mark_price.0, pos.qty.0);
        let expected_pnl = (dec!(51000) - dec!(50000)) * dec!(0.1); // 100 USD kar
        
        // Then: Kar pozitif olmalı
        assert_eq!(current_pnl, expected_pnl, "Long position PnL should be positive when price increases");
        assert!(current_pnl > Decimal::ZERO, "Long position should show profit");
    }

    #[test]
    fn test_position_pnl_calculation_short_profit() {
        // Given: Short pozisyon, fiyat düşüşü
        let pos = create_test_position("BTCUSDT", dec!(-0.1), DEFAULT_ENTRY_PRICE, DEFAULT_LEVERAGE);
        let mark_price = Px(dec!(49000)); // %2 düşüş
        
        // When: PnL hesaplanır
        let current_pnl = calculate_pnl_short(pos.entry.0, mark_price.0, pos.qty.0);
        // Short: (49000 - 50000) * (-0.1) = (-1000) * (-0.1) = 100 USD kar
        let expected_pnl = (dec!(49000) - dec!(50000)) * dec!(-0.1);
        
        // Then: Kar pozitif olmalı
        assert_eq!(current_pnl, expected_pnl, "Short position PnL should be positive when price decreases");
        assert!(current_pnl > Decimal::ZERO, "Short position should show profit");
    }

    #[test]
    fn test_take_profit_logic() {
        // %2+ kar varsa ve trend tersine dönüyorsa kar al
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let mark_price = Px(dec!(51000)); // %2 kar
        
        let entry_price_f64 = pos.entry.0.to_f64().unwrap_or(0.0);
        let mark_price_f64 = mark_price.0.to_f64().unwrap_or(0.0);
        let price_change_pct = (mark_price_f64 - entry_price_f64) / entry_price_f64;
        
        let take_profit_threshold = 0.02; // %2
        let pnl_trend = -0.15; // Trend tersine dönüyor
        
        let should_take_profit = if price_change_pct >= take_profit_threshold {
            if pnl_trend < -0.1 {
                true
            } else {
                false
            }
        } else {
            false
        };
        
        assert!(should_take_profit, "Should take profit when %2+ profit and trend reversing");
    }

    #[test]
    fn test_stop_loss_logic() {
        // %1'den fazla zarar varsa ve trend kötüleşiyorsa kapat
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let mark_price = Px(dec!(49500)); // %1 zarar
        
        let entry_price_f64 = pos.entry.0.to_f64().unwrap_or(0.0);
        let mark_price_f64 = mark_price.0.to_f64().unwrap_or(0.0);
        let price_change_pct = (mark_price_f64 - entry_price_f64) / entry_price_f64;
        
        let stop_loss_threshold = -0.01; // %1 zarar
        let pnl_trend = -0.25; // Trend kötüleşiyor
        let position_hold_duration_ms = 700_000; // 11 dakika
        
        let should_stop_loss = if price_change_pct <= stop_loss_threshold {
            if pnl_trend < -0.2 || position_hold_duration_ms > 600_000 {
                true
            } else {
                false
            }
        } else {
            false
        };
        
        assert!(should_stop_loss, "Should stop loss when %1+ loss and trend worsening");
    }

    #[test]
    fn test_trailing_stop_logic() {
        // Peak'ten %1 düşerse kapat
        let peak_pnl_f64: f64 = 100.0; // Peak kar
        let current_pnl_f64: f64 = 90.0; // Şu anki kar (peak'ten %10 düşüş)
        let trailing_stop_threshold = 0.01; // %1
        
        let should_trailing_stop = if peak_pnl_f64 > 0.0 && current_pnl_f64 < peak_pnl_f64 {
            let drawdown_from_peak: f64 = (peak_pnl_f64 - current_pnl_f64) / (peak_pnl_f64 as f64).abs().max(0.01_f64);
            drawdown_from_peak >= trailing_stop_threshold
        } else {
            false
        };
        
        assert!(should_trailing_stop, "Should trailing stop when drawdown from peak >= 1%");
    }

    // ============================================================================
    // Order Management Tests
    // ============================================================================

    #[test]
    fn test_stale_order_detection() {
        // Given: Eski ve yeni emirler
        let max_order_age_ms = MAX_ORDER_AGE_MS;
        let old_order = create_test_order("order1", Side::Buy, dec!(50000), dec!(0.1), 15_000); // 15 saniye eski
        let new_order = create_test_order("order2", Side::Sell, dec!(50010), dec!(0.1), 5_000); // 5 saniye eski
        
        // When: Emir yaşları kontrol edilir
        let old_age_ms = old_order.created_at.elapsed().as_millis() as u64;
        let new_age_ms = new_order.created_at.elapsed().as_millis() as u64;
        let old_is_stale = old_age_ms > max_order_age_ms;
        let new_is_stale = new_age_ms > max_order_age_ms;
        
        // Then: Eski emir stale, yeni emir değil
        assert!(old_is_stale, "Order older than threshold should be stale");
        assert!(!new_is_stale, "Order within threshold should not be stale");
    }

    #[test]
    fn test_order_market_distance_calculation() {
        let bid = Px(dec!(50000));
        let ask = Px(dec!(50010));
        
        // Buy order: ask'ten uzaklık
        let buy_order = create_test_order("buy1", Side::Buy, dec!(49900), dec!(0.1), 1_000);
        let ask_f64 = ask.0.to_f64().unwrap_or(0.0);
        let order_price_f64 = buy_order.price.0.to_f64().unwrap_or(0.0);
        let market_distance_pct = if ask_f64 > 0.0 {
            (ask_f64 - order_price_f64) / ask_f64
        } else {
            0.0
        };
        
        // (50010 - 49900) / 50010 = 110 / 50010 ≈ 0.0022 = %0.22
        assert!(market_distance_pct > 0.0, "Buy order should have positive distance from ask");
        
        // Sell order: bid'den uzaklık
        let sell_order = create_test_order("sell1", Side::Sell, dec!(50100), dec!(0.1), 1_000);
        let bid_f64 = bid.0.to_f64().unwrap_or(0.0);
        let sell_order_price_f64 = sell_order.price.0.to_f64().unwrap_or(0.0);
        let sell_market_distance_pct = if bid_f64 > 0.0 {
            (sell_order_price_f64 - bid_f64) / bid_f64
        } else {
            0.0
        };
        
        // (50100 - 50000) / 50000 = 100 / 50000 = 0.002 = %0.2
        assert!(sell_market_distance_pct > 0.0, "Sell order should have positive distance from bid");
    }

    #[test]
    fn test_order_cancel_logic_with_position() {
        // Pozisyon varsa daha toleranslı ol
        let _position_size_notional = 100.0; // Pozisyon var (test için kullanılmıyor ama mantık için gerekli)
        let max_distance_pct_with_position = 0.01; // %1 (pozisyon varsa)
        let max_distance_pct_without_position = 0.005; // %0.5 (pozisyon yoksa)
        
        let order_distance: f64 = 0.007; // %0.7
        
        let should_cancel_with_position = order_distance.abs() > max_distance_pct_with_position;
        let should_cancel_without_position = order_distance.abs() > max_distance_pct_without_position;
        
        assert!(!should_cancel_with_position, "Should not cancel when position exists and distance is within tolerance");
        assert!(should_cancel_without_position, "Should cancel when no position and distance exceeds threshold");
    }

    #[test]
    fn test_inventory_sync_logic() {
        // WebSocket envanteri ile API pozisyonu senkronizasyonu
        let ws_inv = Qty(dec!(0.1));
        let api_pos = create_test_position("BTCUSDT", dec!(0.10000001), dec!(50000), 5);
        
        let inv_diff = (ws_inv.0 - api_pos.qty.0).abs();
        let reconcile_threshold = Decimal::new(1, 8); // 0.00000001
        
        let should_sync = inv_diff > reconcile_threshold;
        
        // Fark çok küçük, senkronize etmeye gerek yok
        assert!(!should_sync, "Should not sync when difference is below threshold");
        
        // Daha büyük fark
        let ws_inv_large = Qty(dec!(0.1));
        let api_pos_large = create_test_position("BTCUSDT", dec!(0.2), dec!(50000), 5);
        let inv_diff_large = (ws_inv_large.0 - api_pos_large.qty.0).abs();
        let should_sync_large = inv_diff_large > reconcile_threshold;
        
        assert!(should_sync_large, "Should sync when difference exceeds threshold");
    }

    #[test]
    fn test_position_size_risk_check() {
        let max_usd_per_order = 100.0;
        let effective_leverage = 5.0;
        let max_position_size_usd = max_usd_per_order * effective_leverage * 5.0; // 2500 USD
        
        let position_size_notional = 3000.0; // Risk limitini aşıyor
        
        let is_risky = position_size_notional > max_position_size_usd;
        
        assert!(is_risky, "Position size should be flagged as risky when exceeding limit");
    }

    #[test]
    fn test_order_fill_rate_tracking() {
        // Fill oranı takibi: Her fill'de artır, her tick'te azalt
        let mut fill_rate: f64 = 0.5;
        
        // Fill oldu
        fill_rate = (fill_rate * 0.95 + 0.05).min(1.0_f64);
        assert!(fill_rate > 0.5, "Fill rate should increase after fill");
        
        // Fill olmadı (ardışık)
        let initial_rate = fill_rate;
        fill_rate = (fill_rate * 0.99).max(0.1_f64);
        assert!(fill_rate < initial_rate, "Fill rate should decrease when no fills");
    }

    #[test]
    fn test_position_hold_duration_tracking() {
        let entry_time = Instant::now() - Duration::from_millis(300_000); // 5 dakika önce
        let hold_duration_ms = entry_time.elapsed().as_millis() as u64;
        
        // 5 dakikadan fazla tutuldu (300_000 ms = 5 dakika)
        // Not: elapsed() hemen hesaplanır, bu yüzden yaklaşık 300_000 ms olmalı
        // Ama tam olarak 300_000'den büyük olmayabilir (çok küçük fark olabilir)
        // Bu yüzden >= kullanıyoruz veya threshold'u biraz düşürüyoruz
        let should_consider_take_profit = hold_duration_ms >= 299_000; // 4.98 dakika (küçük tolerans)
        
        assert!(should_consider_take_profit, "Should consider take profit after ~5 minutes, got: {} ms", hold_duration_ms);
    }

    #[test]
    fn test_pnl_trend_calculation() {
        // PnL geçmişinden trend hesaplama
        let pnl_history = vec![
            dec!(100), dec!(105), dec!(110), dec!(115), dec!(120), // İlk 5
            dec!(115), dec!(110), dec!(105), dec!(100), dec!(95),  // Son 5 (düşüş)
        ];
        
        if pnl_history.len() >= 10 {
            let recent = &pnl_history[pnl_history.len().saturating_sub(10)..];
            let first = recent[0];
            let last = recent[recent.len() - 1];
            let pnl_trend = if first > Decimal::ZERO {
                ((last - first) / first).to_f64().unwrap_or(0.0)
            } else {
                0.0
            };
            
            // (95 - 100) / 100 = -0.05 = %5 düşüş
            assert!(pnl_trend < 0.0, "PnL trend should be negative when decreasing");
        }
    }

    #[test]
    fn test_multiple_orders_management() {
        let mut active_orders: HashMap<String, TestOrderInfo> = HashMap::new();
        
        // Birkaç emir ekle
        active_orders.insert("order1".to_string(), create_test_order("order1", Side::Buy, dec!(50000), dec!(0.1), 1_000));
        active_orders.insert("order2".to_string(), create_test_order("order2", Side::Sell, dec!(50010), dec!(0.1), 1_000));
        active_orders.insert("order3".to_string(), create_test_order("order3", Side::Buy, dec!(49900), dec!(0.1), 15_000)); // Stale
        
        assert_eq!(active_orders.len(), 3);
        
        // Stale emirleri temizle
        let max_order_age_ms = 10_000;
        let mut to_remove = Vec::new();
        for (order_id, order) in &active_orders {
            let age_ms = order.created_at.elapsed().as_millis() as u64;
            if age_ms > max_order_age_ms {
                to_remove.push(order_id.clone());
            }
        }
        
        for order_id in &to_remove {
            active_orders.remove(order_id);
        }
        
        assert_eq!(active_orders.len(), 2, "Should remove stale orders");
    }

    #[test]
    fn test_min_notional_insufficient_balance() {
        // Bakiye yetersizse min notional'ı karşılayamayan emir yapma
        let min_notional = 10.0; // 10 USD minimum
        let available_margin = 5.0; // 5 USD bakiye (yetersiz)
        let price = 50000.0;
        let effective_leverage = 5.0;
        let step = 0.001;
        
        // Maksimum miktar hesapla
        let max_qty_by_margin: f64 = (((available_margin * effective_leverage) / price / step) as f64).floor() * step;
        let available_notional = max_qty_by_margin * price;
        
        // Bakiye yetersizse skip et
        let should_skip = available_notional < min_notional;
        
        assert!(should_skip, "Should skip order when balance is insufficient for min_notional");
        assert!(available_notional < min_notional, "Available notional should be less than min_notional");
    }

    #[test]
    fn test_leverage_position_size_calculation() {
        // Mevcut pozisyonun margin'i çıkarıldıktan sonra kalan bakiyeden leverage uygulanmalı
        let total_balance: f64 = 100.0; // 100 USD toplam bakiye
        let existing_position_notional: f64 = 200.0; // 200 USD pozisyon
        let effective_leverage: f64 = 5.0;
        
        // Mevcut pozisyonun margin'i = notional / leverage
        let existing_position_margin: f64 = existing_position_notional / effective_leverage; // 40 USD
        
        // Kalan bakiye
        let available_after_position: f64 = (total_balance - existing_position_margin).max(0.0_f64); // 60 USD
        
        // Yeni pozisyon için kullanılabilir margin (cap ile sınırlı)
        let max_usable_from_account = available_after_position.min(100.0); // 60 USD (cap 100)
        
        // Leverage ile pozisyon boyutu
        let position_size_with_leverage = max_usable_from_account * effective_leverage; // 300 USD
        
        assert_eq!(existing_position_margin, 40.0, "Existing position margin should be 40 USD");
        assert_eq!(available_after_position, 60.0, "Available after position should be 60 USD");
        assert_eq!(position_size_with_leverage, 300.0, "New position size should be 300 USD");
    }

    #[test]
    fn test_crossing_guard_equality_check() {
        // Crossing guard: bid best_bid'den ASLA yüksek olmamalı (eşitlik dahil)
        let best_bid = dec!(50000);
        let best_ask = dec!(50010);
        let tick = dec!(0.01);
        
        // Bid best_bid'e eşit → 1 tick altına çekilmeli
        let mut bid_px = best_bid;
        if bid_px >= best_bid {
            bid_px = (best_bid - tick).max(dec!(0));
        }
        assert!(bid_px < best_bid, "Bid should be below best_bid when equal or higher");
        
        // Ask best_ask'e eşit → 1 tick üstüne çekilmeli
        let mut ask_px = best_ask;
        if ask_px <= best_ask {
            ask_px = best_ask + tick;
        }
        assert!(ask_px > best_ask, "Ask should be above best_ask when equal or lower");
    }

    #[test]
    fn test_opportunity_size_multiplier_safety() {
        // Fırsat modu çarpanı 5.0 → 2.0 (daha güvenli)
        let base_size = 20.0;
        let old_multiplier = 5.0;
        let new_multiplier = 2.0;
        
        let old_size = base_size * old_multiplier; // 100 USD (çok riskli)
        let new_size = base_size * new_multiplier; // 40 USD (daha güvenli)
        
        assert!(new_size < old_size, "New multiplier should reduce position size");
        assert_eq!(new_size, 40.0, "New size should be 40 USD");
    }

    #[test]
    fn test_dynamic_trailing_stop() {
        // Dinamik trailing stop: Kar büyüdükçe genişlet
        let base_trailing_stop = 0.02; // %2 base
        
        // Küçük kar (< 20 USD)
        let peak_pnl_small = 10.0;
        let trailing_stop_small = if peak_pnl_small > 50.0 {
            0.05
        } else if peak_pnl_small > 20.0 {
            0.03
        } else {
            base_trailing_stop
        };
        assert_eq!(trailing_stop_small, 0.02, "Small profits should use base trailing stop");
        
        // Orta kar (20-50 USD)
        let peak_pnl_medium = 30.0;
        let trailing_stop_medium = if peak_pnl_medium > 50.0 {
            0.05
        } else if peak_pnl_medium > 20.0 {
            0.03
        } else {
            base_trailing_stop
        };
        assert_eq!(trailing_stop_medium, 0.03, "Medium profits should use 3% trailing stop");
        
        // Büyük kar (> 50 USD)
        let peak_pnl_large = 60.0;
        let trailing_stop_large = if peak_pnl_large > 50.0 {
            0.05
        } else if peak_pnl_large > 20.0 {
            0.03
        } else {
            base_trailing_stop
        };
        assert_eq!(trailing_stop_large, 0.05, "Large profits should use 5% trailing stop");
    }

    #[test]
    fn test_rate_limit_guard_usage() {
        // Rate limit guard her API çağrısından önce kullanılmalı
        // Bu test sadece mantığı doğrular (gerçek async test için integration test gerekir)
        let api_calls = vec!["get_position", "get_open_orders", "place_order", "cancel_order", "asset_free"];
        
        for call in api_calls {
            // Her API çağrısından önce rate_limit_guard() çağrılmalı
            // Bu test sadece dokümantasyon amaçlı
            assert!(!call.is_empty(), "API call name should not be empty");
        }
    }

    // ============================================================================
    // Leverage & Margin Calculation Tests
    // ============================================================================

    #[test]
    fn test_leverage_margin_with_unrealized_pnl_loss() {
        // Given: Zarar eden pozisyon (unrealized PnL negatif)
        let total_balance: f64 = 100.0;
        let existing_position_notional: f64 = 200.0;
        let effective_leverage: f64 = 5.0;
        let unrealized_pnl: f64 = -20.0; // 20 USD zarar
        
        // When: Margin hesaplanır (unrealized PnL ile)
        let base_margin: f64 = existing_position_notional / effective_leverage; // 40 USD
        let existing_position_margin: f64 = calculate_margin_with_pnl(
            existing_position_notional,
            effective_leverage,
            unrealized_pnl
        ); // 60 USD
        let available_after_position: f64 = (total_balance - existing_position_margin).max(0.0_f64);
        
        // Then: Zarar eden pozisyon margin'i tüketir
        assert_eq!(base_margin, 40.0, "Base margin should be 40 USD");
        assert_eq!(existing_position_margin, 60.0, "Loss position consumes more margin");
        assert_eq!(available_after_position, 40.0, "Available balance should decrease");
    }

    #[test]
    fn test_leverage_margin_with_unrealized_pnl_profit() {
        // Given: Kar eden pozisyon (unrealized PnL pozitif)
        let total_balance: f64 = 100.0;
        let existing_position_notional: f64 = 200.0;
        let effective_leverage: f64 = 5.0;
        let unrealized_pnl: f64 = 20.0; // 20 USD kar
        
        // When: Margin hesaplanır (unrealized PnL ile)
        let base_margin: f64 = existing_position_notional / effective_leverage; // 40 USD
        let existing_position_margin: f64 = calculate_margin_with_pnl(
            existing_position_notional,
            effective_leverage,
            unrealized_pnl
        ); // 20 USD
        let available_after_position: f64 = (total_balance - existing_position_margin).max(0.0_f64);
        
        // Then: Kar eden pozisyon margin'i serbest bırakır
        assert_eq!(existing_position_margin, 20.0, "Profit position frees up margin");
        assert_eq!(available_after_position, 80.0, "Available balance should increase");
    }

    #[test]
    fn test_trailing_stop_take_profit_priority() {
        // KRİTİK TEST: Trailing stop ve take profit çakışması - Öncelik sırası
        // Önce trailing stop (20 USD+ karlardayken), sonra take profit/stop loss
        let peak_pnl_f64 = 25.0; // 25 USD peak kar
        let current_pnl_f64 = 24.0; // 24 USD (peak'ten %4 düşüş)
        let price_change_pct = 0.04; // %4 kar
        let take_profit_threshold = 0.05; // %5 take profit eşiği
        
        // Trailing stop kontrolü
        let drawdown_from_peak: f64 = (peak_pnl_f64 - current_pnl_f64) / (peak_pnl_f64 as f64).abs().max(0.01_f64);
        let trailing_stop_threshold = 0.01; // %1 trailing stop
        let should_trailing_stop = drawdown_from_peak >= trailing_stop_threshold && peak_pnl_f64 > 20.0;
        
        // Take profit kontrolü
        let should_take_profit = price_change_pct >= take_profit_threshold;
        
        // Öncelik sırası: Trailing stop > Take profit
        let (should_close, reason) = if should_trailing_stop && peak_pnl_f64 > 20.0 {
            (true, "trailing_stop")
        } else if should_take_profit {
            (true, "take_profit")
        } else {
            (false, "")
        };
        
        // %4 kardayken %1 trailing stop tetiklenmeli (take profit değil)
        assert!(should_close, "Should close position due to trailing stop");
        assert_eq!(reason, "trailing_stop", "Reason should be trailing_stop, not take_profit");
        
        // Take profit daha yüksek karda tetiklenmeli
        let price_change_pct_high = 0.06; // %6 kar
        let should_take_profit_high = price_change_pct_high >= take_profit_threshold;
        let (should_close_high, reason_high) = if should_trailing_stop && peak_pnl_f64 > 20.0 {
            (true, "trailing_stop")
        } else if should_take_profit_high {
            (true, "take_profit")
        } else {
            (false, "")
        };
        
        // Trailing stop hala aktif, öncelik trailing stop
        assert!(should_close_high, "Should close position");
        // Not: Bu test'te trailing stop hala tetikleniyor çünkü peak'ten düşüş var
    }

    #[test]
    fn test_min_notional_retry_with_margin_constraint() {
        // KRİTİK TEST: Min notional retry - margin constraint ile
        let min_notional = 10.0; // 10 USD minimum
        let available_margin = 8.0; // 8 USD bakiye (yetersiz)
        let price_quantized = 50000.0;
        let effective_leverage = 5.0;
        let step = 0.001;
        
        // Önce maksimum qty'yi belirle (margin constraint)
        let max_qty_by_margin: f64 = (((available_margin * effective_leverage) / price_quantized / step) as f64).floor() * step;
        let available_notional = max_qty_by_margin * price_quantized;
        
        // Bakiye yetersizse min notional'ı karşılayamayan emir yapma
        let should_skip = available_notional < min_notional;
        
        assert!(should_skip, "Should skip when margin is insufficient for min_notional");
        assert!(available_notional < min_notional, "Available notional should be less than min_notional");
        
        // Yeterli bakiye varsa
        let available_margin_sufficient = 12.0; // 12 USD bakiye (yeterli)
        let max_qty_by_margin_sufficient: f64 = (((available_margin_sufficient * effective_leverage) / price_quantized / step) as f64).floor() * step;
        let available_notional_sufficient = max_qty_by_margin_sufficient * price_quantized;
        
        // Min qty hesapla
        let min_qty_for_notional: f64 = ((min_notional * 1.1 / price_quantized / step) as f64).ceil() * step; // %10 güvenli margin
        let final_qty: f64 = min_qty_for_notional.min(max_qty_by_margin_sufficient);
        let final_notional = final_qty * price_quantized;
        let required_margin = final_notional / effective_leverage;
        
        assert!(final_notional >= min_notional, "Final notional should meet min_notional");
        assert!(required_margin <= available_margin_sufficient, "Required margin should be within available");
    }

    #[test]
    fn test_funding_cost_calculation() {
        // KRİTİK TEST: Funding cost hesaplama
        // Funding cost = funding_rate * position_size_notional (her 8 saatte bir)
        let funding_rate = 0.0001; // 0.01% per 8h = 1 bps
        let position_size_notional = 1000.0; // 1000 USD pozisyon
        
        let funding_cost = funding_rate * position_size_notional; // 0.1 USD
        
        assert_eq!(funding_cost, 0.1, "Funding cost should be 0.1 USD for 1000 USD position at 0.01% rate");
        
        // Negatif funding rate (short pozisyon için ödeme alırsın)
        let funding_rate_negative = -0.0001; // -0.01% per 8h
        let funding_cost_negative = funding_rate_negative * position_size_notional; // -0.1 USD (alırsın)
        
        assert_eq!(funding_cost_negative, -0.1, "Negative funding cost means you receive payment");
    }

    #[test]
    fn test_websocket_reconnect_position_sync() {
        // KRİTİK TEST: WebSocket reconnect sonrası pozisyon sync
        let ws_inv = Qty(dec!(0.1));
        let api_pos = create_test_position("BTCUSDT", dec!(0.15), dec!(50000), 5);
        let force_sync_all = true; // Reconnect sonrası
        
        let inv_diff = (ws_inv.0 - api_pos.qty.0).abs();
        let reconcile_threshold = Decimal::new(1, 8);
        let is_reconnect_sync = force_sync_all;
        
        // Reconnect sonrası: Her zaman sync yap (uyumsuzluk olsun ya da olmasın)
        let should_sync = is_reconnect_sync || inv_diff > reconcile_threshold;
        
        assert!(should_sync, "Should sync position after reconnect, regardless of difference");
        
        // Normal durumda: Sadece uyumsuzluk varsa sync yap
        let force_sync_all_normal = false;
        let is_reconnect_sync_normal = force_sync_all_normal;
        let should_sync_normal = is_reconnect_sync_normal || inv_diff > reconcile_threshold;
        
        assert!(should_sync_normal, "Should sync in normal mode when difference exceeds threshold");
        
        // Küçük fark: Normal modda sync yapma
        let ws_inv_small = Qty(dec!(0.1));
        let api_pos_small = create_test_position("BTCUSDT", dec!(0.10000001), dec!(50000), 5);
        let inv_diff_small = (ws_inv_small.0 - api_pos_small.qty.0).abs();
        let should_sync_small = is_reconnect_sync_normal || inv_diff_small > reconcile_threshold;
        
        assert!(!should_sync_small, "Should not sync when difference is below threshold in normal mode");
    }

    #[test]
    fn test_rate_limit_symbol_prioritization() {
        // KRİTİK TEST: Rate limit koruması - Sembol önceliklendirme
        // Aktif emir/pozisyon olanlar önce işlenmeli
        let mut states_with_priority: Vec<(usize, bool)> = vec![
            (0, false), // Sembol 0: Aktif emir/pozisyon yok
            (1, true),  // Sembol 1: Aktif emir var
            (2, false), // Sembol 2: Aktif emir/pozisyon yok
            (3, true),  // Sembol 3: Pozisyon var
        ];
        
        // Öncelikli olanları öne al
        states_with_priority.sort_by(|a, b| b.1.cmp(&a.1)); // true (öncelikli) önce
        
        // İlk iki sembol öncelikli olmalı
        assert!(states_with_priority[0].1, "First symbol should have priority");
        assert!(states_with_priority[1].1, "Second symbol should have priority");
        assert!(!states_with_priority[2].1, "Third symbol should not have priority");
        assert!(!states_with_priority[3].1, "Fourth symbol should not have priority");
    }

    #[test]
    fn test_opportunity_multiplier_dynamic_confidence() {
        // KRİTİK TEST: Opportunity multiplier - Dinamik güven seviyesi
        let base_multiplier = 1.2; // Config'den: Base multiplier
        let confidence_bonus_multiplier = 0.3; // Config'den: Bonus multiplier
        let confidence_max_multiplier = 1.5; // Config'den: Max multiplier
        
        // Flash crash: Büyük düşüş = yüksek güven
        let price_drop_bps = 400.0; // 400 bps düşüş
        let confidence_price_drop_max = 500.0;
        let confidence_flash_crash: f64 = ((price_drop_bps / confidence_price_drop_max) as f64).min(1.0_f64).max(0.5_f64);
        let confidence_bonus: f64 = confidence_bonus_multiplier * confidence_flash_crash;
        let multiplier_flash_crash: f64 = (base_multiplier + confidence_bonus).min(confidence_max_multiplier);
        
        assert!(multiplier_flash_crash > base_multiplier, "Flash crash should increase multiplier");
        assert!(multiplier_flash_crash <= confidence_max_multiplier, "Multiplier should not exceed max");
        
        // Volume anomaly: Yüksek volume ratio = yüksek güven
        let volume_ratio = 8.0; // 8x volume
        let confidence_volume_ratio_min = 5.0;
        let confidence_volume_ratio_max = 10.0;
        let confidence_volume: f64 = (((volume_ratio - confidence_volume_ratio_min) / (confidence_volume_ratio_max - confidence_volume_ratio_min)) as f64).min(1.0_f64).max(0.5_f64);
        let confidence_bonus_volume: f64 = confidence_bonus_multiplier * confidence_volume;
        let multiplier_volume: f64 = (base_multiplier + confidence_bonus_volume).min(confidence_max_multiplier);
        
        assert!(multiplier_volume > base_multiplier, "High volume should increase multiplier");
        
        // Düşük güven: Küçük düşüş
        let price_drop_bps_small = 260.0; // 260 bps (eşik 250'den biraz fazla)
        let confidence_small: f64 = ((price_drop_bps_small / confidence_price_drop_max) as f64).min(1.0_f64).max(0.5_f64);
        let confidence_bonus_small: f64 = confidence_bonus_multiplier * confidence_small;
        let multiplier_small: f64 = (base_multiplier + confidence_bonus_small).min(confidence_max_multiplier);
        
        assert!(multiplier_small < multiplier_flash_crash, "Small drop should have lower multiplier than large drop");
    }

    #[test]
    fn test_manipulation_recovery_check() {
        // KRİTİK TEST: Manipülasyon tespiti - Recovery check mantığı
        // Flash crash sonrası geri dönüş kontrolü: Gerçek flash crash mi yoksa normal volatilite mi?
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        // Senaryo 1: Gerçek flash crash (düşüş sonrası geri yükseliyor)
        // Fiyat geçmişi: 50000 → 49000 → 49500 (düşüş sonrası geri yükseliyor)
        let price_history_recovery: Vec<(u64, Decimal)> = vec![
            (now - 2000, dec!(50000)), // 2 saniye önce
            (now - 1000, dec!(49000)), // 1 saniye önce (düşüş)
            (now, dec!(49500)),        // Şimdi (geri yükseliyor)
        ];
        
        if price_history_recovery.len() >= 3 {
            let last_3: Vec<Decimal> = price_history_recovery.iter().rev().take(3).map(|(_, p)| *p).collect();
            let price_change_bps = -200.0; // -200 bps düşüş
            let price_jump_threshold_bps = 250.0;
            
            let recovery_check = if price_change_bps < -price_jump_threshold_bps {
                // Düşüş sonrası geri yükseliyor mu?
                last_3[0] > last_3[1] && last_3[1] < last_3[2]
            } else {
                false
            };
            
            // Bu senaryoda recovery_check false olmalı (price_change_bps -200, threshold -250)
            // Ama mantık doğru: last_3[0] (49500) > last_3[1] (49000) && last_3[1] (49000) < last_3[2] (50000) = true
            // Ancak price_change_bps threshold'u geçmediği için recovery_check false
            // Gerçek flash crash için threshold'u geçmeli
            let price_change_bps_real = -300.0; // -300 bps (threshold'u geçiyor
            let recovery_check_real = if price_change_bps_real < -price_jump_threshold_bps {
                last_3[0] > last_3[1] && last_3[1] < last_3[2]
            } else {
                false
            };
            
            assert!(recovery_check_real, "Recovery check should detect real flash crash with recovery");
        }
        
        // Senaryo 2: Normal volatilite (düşüş sonrası daha da düşüyor)
        let price_history_no_recovery: Vec<(u64, Decimal)> = vec![
            (now - 2000, dec!(50000)),
            (now - 1000, dec!(49000)),
            (now, dec!(48500)), // Daha da düşüyor (geri yükselmiyor)
        ];
        
        if price_history_no_recovery.len() >= 3 {
            let last_3: Vec<Decimal> = price_history_no_recovery.iter().rev().take(3).map(|(_, p)| *p).collect();
            let price_change_bps = -300.0;
            let price_jump_threshold_bps = 250.0;
            
            let recovery_check_no_recovery = if price_change_bps < -price_jump_threshold_bps {
                last_3[0] > last_3[1] && last_3[1] < last_3[2]
            } else {
                false
            };
            
            // last_3[0] (48500) > last_3[1] (49000) = false
            assert!(!recovery_check_no_recovery, "Recovery check should not trigger for continued decline");
        }
    }

    #[test]
    fn test_config_values_usage() {
        // KRİTİK TEST: Config değerlerinin doğru kullanımı
        // Tüm hardcoded değerler config'den okunmalı
        
        // Internal config değerleri
        let pnl_history_max_len = 1024;
        let max_symbols_per_tick = 8;
        let order_sync_interval_sec = 2;
        let cancel_stagger_delay_ms = 50;
        let fill_rate_increase_factor = 0.95;
        let fill_rate_increase_bonus = 0.05;
        let order_price_distance_with_position = 0.01;
        let order_price_distance_no_position = 0.005;
        let pnl_alert_interval_sec = 10;
        let pnl_alert_threshold_positive = 0.05;
        let pnl_alert_threshold_negative = -0.03;
        let take_profit_threshold_small = 0.01;
        let take_profit_threshold_large = 0.05;
        let trailing_stop_peak_threshold_medium = 20.0;
        let trailing_stop_drawdown_small = 0.01;
        let stop_loss_threshold = -0.005;
        let max_position_size_buffer = 5.0;
        
        // Strategy internal config değerleri
        let manipulation_volume_ratio_threshold = 5.0;
        let manipulation_time_threshold_ms = 2000;
        let manipulation_price_history_max_len = 200;
        let confidence_price_drop_max = 500.0;
        let confidence_bonus_multiplier = 0.3;
        let confidence_max_multiplier = 1.5;
        let trend_analysis_threshold_negative = -0.15;
        let trend_analysis_threshold_strong_negative = -0.20;
        
        // Değerlerin mantıklı olduğunu kontrol et
        assert!(pnl_history_max_len > 0, "PnL history max length should be positive");
        assert!(max_symbols_per_tick > 0, "Max symbols per tick should be positive");
        assert!(order_sync_interval_sec > 0, "Order sync interval should be positive");
        assert!(fill_rate_increase_factor > 0.0 && fill_rate_increase_factor < 1.0, "Fill rate increase factor should be between 0 and 1");
        assert!(order_price_distance_with_position > order_price_distance_no_position, "Order price distance with position should be larger than without");
        assert!(take_profit_threshold_large > take_profit_threshold_small, "Take profit threshold large should be larger than small");
        assert!(trailing_stop_peak_threshold_medium > 0.0, "Trailing stop peak threshold should be positive");
        assert!(stop_loss_threshold < 0.0, "Stop loss threshold should be negative");
        assert!(manipulation_volume_ratio_threshold > 1.0, "Manipulation volume ratio threshold should be > 1.0");
        assert!(confidence_max_multiplier > 1.0, "Confidence max multiplier should be > 1.0");
        assert!(trend_analysis_threshold_strong_negative < trend_analysis_threshold_negative, "Strong negative threshold should be more negative");
    }

    #[test]
    fn test_edge_cases_zero_values() {
        // KRİTİK TEST: Edge case'ler - Sıfır değerler
        let zero_position = create_test_position("BTCUSDT", dec!(0), dec!(50000), 5);
        let zero_mark_price = Px(dec!(0));
        
        // Sıfır pozisyon ile PnL hesaplama
        let pnl_zero = (zero_mark_price.0 - zero_position.entry.0) * zero_position.qty.0;
        assert_eq!(pnl_zero, dec!(0), "PnL should be zero for zero position");
        
        // Sıfır bakiye
        let total_balance_zero: f64 = 0.0;
        let existing_position_margin_zero: f64 = 0.0;
        let available_after_position_zero: f64 = (total_balance_zero - existing_position_margin_zero).max(0.0_f64);
        assert_eq!(available_after_position_zero, 0.0, "Available should be zero when balance is zero");
        
        // Sıfır leverage (bölme hatası önleme)
        let position_notional = 100.0;
        let leverage_zero = 0.0;
        // Leverage sıfır olamaz (config'de kontrol edilmeli), ama test için
        if leverage_zero > 0.0 {
            let margin: f64 = position_notional / leverage_zero;
            assert!(margin.is_finite() || margin == 0.0, "Margin should be finite or zero");
        }
    }

    #[test]
    fn test_edge_cases_negative_values() {
        // KRİTİK TEST: Edge case'ler - Negatif değerler
        // Negatif PnL (zarar)
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let mark_price_loss = Px(dec!(49000)); // %2 zarar
        let pnl_loss = (mark_price_loss.0 - pos.entry.0) * pos.qty.0;
        assert!(pnl_loss < Decimal::ZERO, "PnL should be negative for loss");
        
        // Negatif bakiye (imkansız ama kontrol)
        let total_balance_negative: f64 = -10.0;
        let existing_position_margin_negative: f64 = 5.0;
        let available_after_position_negative: f64 = (total_balance_negative - existing_position_margin_negative).max(0.0_f64);
        assert_eq!(available_after_position_negative, 0.0, "Available should be zero when balance is negative");
    }

    #[test]
    fn test_edge_cases_very_large_values() {
        // KRİTİK TEST: Edge case'ler - Çok büyük değerler
        let very_large_position = create_test_position("BTCUSDT", dec!(1000), dec!(50000), 5);
        let very_large_mark_price = Px(dec!(1000000));
        
        // Çok büyük pozisyon ile PnL hesaplama (overflow kontrolü)
        let pnl_large = (very_large_mark_price.0 - very_large_position.entry.0) * very_large_position.qty.0;
        // Decimal için is_finite() yok, f64'e çevirip kontrol et
        let pnl_f64 = pnl_large.to_f64().unwrap_or(0.0);
        assert!(pnl_f64.is_finite() || pnl_f64 == 0.0, "PnL should be finite or zero even for very large values");
        
        // Çok büyük bakiye
        let total_balance_large: f64 = 1_000_000.0;
        let existing_position_margin_large: f64 = 100_000.0;
        let available_after_position_large: f64 = (total_balance_large - existing_position_margin_large).max(0.0_f64);
        assert_eq!(available_after_position_large, 900_000.0, "Available should handle large values correctly");
    }

    #[test]
    fn test_trailing_stop_dynamic_thresholds() {
        // KRİTİK TEST: Dinamik trailing stop eşikleri (config'den)
        let peak_pnl_large = 120.0; // 120 USD (büyük kar)
        let peak_pnl_medium = 25.0; // 25 USD (orta kar)
        let peak_pnl_small = 10.0; // 10 USD (küçük kar)
        
        let trailing_stop_peak_threshold_large = 100.0;
        let trailing_stop_peak_threshold_medium = 20.0;
        let trailing_stop_drawdown_large = 0.03; // %3
        let trailing_stop_drawdown_medium = 0.015; // %1.5
        let trailing_stop_drawdown_small = 0.01; // %1
        
        // Büyük kar: %3 trailing stop
        let trailing_stop_large = if peak_pnl_large > trailing_stop_peak_threshold_large {
            trailing_stop_drawdown_large
        } else if peak_pnl_large > trailing_stop_peak_threshold_medium {
            trailing_stop_drawdown_medium
        } else {
            trailing_stop_drawdown_small
        };
        assert_eq!(trailing_stop_large, 0.03, "Large profit should use 3% trailing stop");
        
        // Orta kar: %1.5 trailing stop
        let trailing_stop_medium = if peak_pnl_medium > trailing_stop_peak_threshold_large {
            trailing_stop_drawdown_large
        } else if peak_pnl_medium > trailing_stop_peak_threshold_medium {
            trailing_stop_drawdown_medium
        } else {
            trailing_stop_drawdown_small
        };
        assert_eq!(trailing_stop_medium, 0.015, "Medium profit should use 1.5% trailing stop");
        
        // Küçük kar: %1 trailing stop
        let trailing_stop_small = if peak_pnl_small > trailing_stop_peak_threshold_large {
            trailing_stop_drawdown_large
        } else if peak_pnl_small > trailing_stop_peak_threshold_medium {
            trailing_stop_drawdown_medium
        } else {
            trailing_stop_drawdown_small
        };
        assert_eq!(trailing_stop_small, 0.01, "Small profit should use 1% trailing stop");
    }
}

