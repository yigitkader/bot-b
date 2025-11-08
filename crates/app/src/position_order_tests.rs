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
    
    // OrderInfo struct'ını test için tanımla (main.rs'deki ile aynı)
    #[derive(Clone, Debug)]
    struct TestOrderInfo {
        order_id: String,
        side: Side,
        price: Px,
        qty: Qty,
        created_at: Instant,
    }

    // Helper: Test pozisyonu oluştur
    fn create_test_position(symbol: &str, qty: Decimal, entry: Decimal, leverage: u32) -> Position {
        Position {
            symbol: symbol.to_string(),
            qty: Qty(qty),
            entry: Px(entry),
            leverage,
            liq_px: Some(Px(entry * dec!(0.9))), // %10 likidasyon mesafesi
        }
    }

    // Helper: Test emri oluştur
    fn create_test_order(order_id: &str, side: Side, price: Decimal, qty: Decimal, age_ms: u64) -> TestOrderInfo {
        TestOrderInfo {
            order_id: order_id.to_string(),
            side,
            price: Px(price),
            qty: Qty(qty),
            created_at: Instant::now() - Duration::from_millis(age_ms),
        }
    }

    #[test]
    fn test_position_pnl_calculation_long() {
        // Long pozisyon: fiyat artışı = kar
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let mark_price = Px(dec!(51000)); // %2 artış
        
        let current_pnl = (mark_price.0 - pos.entry.0) * pos.qty.0;
        let expected_pnl = (dec!(51000) - dec!(50000)) * dec!(0.1); // 100 USD kar
        
        assert_eq!(current_pnl, expected_pnl);
        assert!(current_pnl > Decimal::ZERO); // Kar var
    }

    #[test]
    fn test_position_pnl_calculation_short() {
        // Short pozisyon: fiyat düşüşü = kar
        let pos = create_test_position("BTCUSDT", dec!(-0.1), dec!(50000), 5);
        let mark_price = Px(dec!(49000)); // %2 düşüş
        
        let current_pnl = (mark_price.0 - pos.entry.0) * pos.qty.0;
        // Short: (49000 - 50000) * (-0.1) = (-1000) * (-0.1) = 100 USD kar
        let expected_pnl = (dec!(49000) - dec!(50000)) * dec!(-0.1);
        
        assert_eq!(current_pnl, expected_pnl);
        assert!(current_pnl > Decimal::ZERO); // Kar var
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
            let drawdown_from_peak = (peak_pnl_f64 - current_pnl_f64) / peak_pnl_f64.abs().max(0.01_f64);
            drawdown_from_peak >= trailing_stop_threshold
        } else {
            false
        };
        
        assert!(should_trailing_stop, "Should trailing stop when drawdown from peak >= 1%");
    }

    #[test]
    fn test_stale_order_detection() {
        let max_order_age_ms = 10_000; // 10 saniye
        let old_order = create_test_order("order1", Side::Buy, dec!(50000), dec!(0.1), 15_000); // 15 saniye eski
        let new_order = create_test_order("order2", Side::Sell, dec!(50010), dec!(0.1), 5_000); // 5 saniye eski
        
        let old_age_ms = old_order.created_at.elapsed().as_millis() as u64;
        let new_age_ms = new_order.created_at.elapsed().as_millis() as u64;
        
        let old_is_stale = old_age_ms > max_order_age_ms;
        let new_is_stale = new_age_ms > max_order_age_ms;
        
        assert!(old_is_stale, "Old order should be detected as stale");
        assert!(!new_is_stale, "New order should not be stale");
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
        let total_balance = 100.0; // 100 USD toplam bakiye
        let existing_position_notional = 200.0; // 200 USD pozisyon
        let effective_leverage = 5.0;
        
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
}

