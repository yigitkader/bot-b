//location: /crates/risk/src/lib.rs
use bot_core::types::*;

pub struct RiskLimits {
    pub inv_cap: Qty,
    pub min_liq_gap_bps: f64,
    pub dd_limit_bps: i64,
    pub max_leverage: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskAction {
    Ok,
    Widen,
    Reduce,
    Halt,
}

pub fn check_risk(
    pos: &Position,
    inv: Qty,
    liq_gap_bps: f64,
    dd_bps: i64,
    lim: &RiskLimits,
) -> RiskAction {
    if dd_bps <= -lim.dd_limit_bps {
        return RiskAction::Halt;
    }
    if inv.0.abs() > lim.inv_cap.0 {
        return RiskAction::Reduce;
    }
    // Pozisyon yoksa liquidation gap kontrolü yapma (liq_px None ise zaten 9999.0 olarak ayarlanmış)
    // Ama pozisyon varsa kontrol et
    let has_position = pos.qty.0.abs() > rust_decimal::Decimal::new(1, 8);
    if has_position && liq_gap_bps < lim.min_liq_gap_bps {
        return RiskAction::Reduce;
    }
    // Pozisyon yoksa leverage kontrolü yapma (pozisyon yokken leverage anlamsız)
    if has_position && pos.leverage > lim.max_leverage {
        return RiskAction::Reduce;
    }
    RiskAction::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use bot_core::types::*;

    fn create_test_position(symbol: &str, qty: Decimal, entry: Decimal, leverage: u32) -> Position {
        Position {
            symbol: symbol.to_string(),
            qty: Qty(qty),
            entry: Px(entry),
            leverage,
            liq_px: None,
        }
    }

    fn create_test_limits(inv_cap: Decimal, min_liq_gap: f64, dd_limit: i64, max_leverage: u32) -> RiskLimits {
        RiskLimits {
            inv_cap: Qty(inv_cap),
            min_liq_gap_bps: min_liq_gap,
            dd_limit_bps: dd_limit,
            max_leverage,
        }
    }

    #[test]
    fn test_risk_ok() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);
        
        let action = check_risk(&pos, inv, 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Ok);
    }

    #[test]
    fn test_risk_halt_on_drawdown() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);
        
        // Drawdown -2500 bps (limit -2000'den kötü)
        let action = check_risk(&pos, inv, 500.0, -2500, &limits);
        assert_eq!(action, RiskAction::Halt);
    }

    #[test]
    fn test_risk_reduce_on_inventory_exceeded() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let limits = create_test_limits(dec!(0.5), 300.0, 2000, 10);
        
        // Inventory 0.6, limit 0.5'ten fazla
        let inv = Qty(dec!(0.6));
        let action = check_risk(&pos, inv, 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }

    #[test]
    fn test_risk_reduce_on_low_liquidation_gap() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);
        
        // Liquidation gap 200 bps, minimum 300 bps
        let action = check_risk(&pos, inv, 200.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }

    #[test]
    fn test_risk_reduce_on_leverage_exceeded() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 15);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);
        
        // Position leverage 15, max 10
        let action = check_risk(&pos, inv, 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }

    #[test]
    fn test_risk_negative_inventory() {
        let pos = create_test_position("BTCUSDT", dec!(-0.1), dec!(50000), 5);
        let limits = create_test_limits(dec!(0.5), 300.0, 2000, 10);
        
        // Negative inventory -0.6, abs > 0.5
        let inv = Qty(dec!(-0.6));
        let action = check_risk(&pos, inv, 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }

    #[test]
    fn test_risk_edge_cases() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);
        
        // Exactly at drawdown limit
        let action = check_risk(&pos, Qty(dec!(0.05)), 500.0, -2000, &limits);
        assert_eq!(action, RiskAction::Halt);
        
        // Just below drawdown limit
        let action = check_risk(&pos, Qty(dec!(0.05)), 500.0, -1999, &limits);
        assert_eq!(action, RiskAction::Ok);
        
        // Exactly at inventory limit
        let action = check_risk(&pos, Qty(dec!(1.0)), 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Ok);
        
        // Just above inventory limit
        let action = check_risk(&pos, Qty(dec!(1.0001)), 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
        
        // Exactly at liquidation gap limit
        let action = check_risk(&pos, Qty(dec!(0.05)), 300.0, -100, &limits);
        assert_eq!(action, RiskAction::Ok);
        
        // Just below liquidation gap limit
        let action = check_risk(&pos, Qty(dec!(0.05)), 299.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
        
        // Exactly at leverage limit
        let pos_max_leverage = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 10);
        let action = check_risk(&pos_max_leverage, Qty(dec!(0.05)), 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Ok);
        
        // Just above leverage limit
        let pos_over_leverage = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 11);
        let action = check_risk(&pos_over_leverage, Qty(dec!(0.05)), 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }

    #[test]
    fn test_liq_gap_bps_long_position() {
        // Long pozisyon: liq_px < mark_px
        // liq_gap_bps = ((mark - liq) / mark) * 10000
        // Örnek: mark=50000, liq=48500 → gap = (1500/50000)*10000 = 300 bps
        let pos = Position {
            symbol: "BTCUSDT".to_string(),
            qty: Qty(dec!(0.1)), // Long
            entry: Px(dec!(50000)),
            leverage: 5,
            liq_px: Some(Px(dec!(48500))), // Liquidation price
        };
        let mark_px = Px(dec!(50000));
        let liq_gap_bps = if let Some(liq_px) = pos.liq_px {
            let mark = mark_px.0.to_f64().unwrap_or(0.0);
            let liq = liq_px.0.to_f64().unwrap_or(0.0);
            if mark > 0.0 {
                ((mark - liq).abs() / mark) * 10_000.0
            } else {
                9_999.0
            }
        } else {
            9_999.0
        };
        // Long: mark=50000, liq=48500 → gap = 300 bps
        assert!((liq_gap_bps - 300.0).abs() < 1.0, "Long liq gap should be ~300 bps");
        
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);
        let action = check_risk(&pos, Qty(dec!(0.05)), liq_gap_bps, -100, &limits);
        assert_eq!(action, RiskAction::Ok, "300 bps gap should be OK (>= 300)");
        
        // Düşük gap: 200 bps
        let liq_gap_low = 200.0;
        let action_low = check_risk(&pos, Qty(dec!(0.05)), liq_gap_low, -100, &limits);
        assert_eq!(action_low, RiskAction::Reduce, "200 bps gap should trigger Reduce (< 300)");
    }

    #[test]
    fn test_liq_gap_bps_short_position() {
        // Short pozisyon: liq_px > mark_px
        // liq_gap_bps = ((liq - mark) / mark) * 10000
        // Örnek: mark=50000, liq=51500 → gap = (1500/50000)*10000 = 300 bps
        let pos = Position {
            symbol: "BTCUSDT".to_string(),
            qty: Qty(dec!(-0.1)), // Short
            entry: Px(dec!(50000)),
            leverage: 5,
            liq_px: Some(Px(dec!(51500))), // Liquidation price (short için yukarıda)
        };
        let mark_px = Px(dec!(50000));
        let liq_gap_bps = if let Some(liq_px) = pos.liq_px {
            let mark = mark_px.0.to_f64().unwrap_or(0.0);
            let liq = liq_px.0.to_f64().unwrap_or(0.0);
            if mark > 0.0 {
                ((mark - liq).abs() / mark) * 10_000.0
            } else {
                9_999.0
            }
        } else {
            9_999.0
        };
        // Short: mark=50000, liq=51500 → gap = 300 bps
        assert!((liq_gap_bps - 300.0).abs() < 1.0, "Short liq gap should be ~300 bps");
        
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);
        let action = check_risk(&pos, Qty(dec!(-0.05)), liq_gap_bps, -100, &limits);
        assert_eq!(action, RiskAction::Ok, "300 bps gap should be OK (>= 300)");
    }

    #[test]
    fn test_risk_action_priority() {
        // Risk eylem önceliği: HALT > REDUCE > WIDEN > OK
        // HALT en yüksek öncelik, her zaman önce kontrol edilmeli
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);
        
        // HALT: Drawdown çok kötü
        let action_halt = check_risk(&pos, Qty(dec!(0.05)), 500.0, -2500, &limits);
        assert_eq!(action_halt, RiskAction::Halt, "HALT should have highest priority");
        
        // REDUCE: Inventory exceeded (HALT değil, çünkü drawdown OK)
        let action_reduce_inv = check_risk(&pos, Qty(dec!(1.5)), 500.0, -100, &limits);
        assert_eq!(action_reduce_inv, RiskAction::Reduce, "REDUCE on inventory exceeded");
        
        // REDUCE: Low liq gap (HALT değil, çünkü drawdown OK)
        let action_reduce_liq = check_risk(&pos, Qty(dec!(0.05)), 200.0, -100, &limits);
        assert_eq!(action_reduce_liq, RiskAction::Reduce, "REDUCE on low liq gap");
        
        // REDUCE: Leverage exceeded (HALT değil, çünkü drawdown OK)
        let pos_high_leverage = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 15);
        let action_reduce_leverage = check_risk(&pos_high_leverage, Qty(dec!(0.05)), 500.0, -100, &limits);
        assert_eq!(action_reduce_leverage, RiskAction::Reduce, "REDUCE on leverage exceeded");
        
        // OK: Her şey normal
        let action_ok = check_risk(&pos, Qty(dec!(0.05)), 500.0, -100, &limits);
        assert_eq!(action_ok, RiskAction::Ok, "OK when all checks pass");
        
        // Öncelik testi: HALT + REDUCE durumunda HALT kazanmalı
        // (Drawdown çok kötü + inventory exceeded → HALT)
        let action_priority = check_risk(&pos, Qty(dec!(1.5)), 500.0, -2500, &limits);
        assert_eq!(action_priority, RiskAction::Halt, "HALT should override REDUCE");
    }
}
