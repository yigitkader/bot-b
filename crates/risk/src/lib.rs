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
    if liq_gap_bps < lim.min_liq_gap_bps {
        return RiskAction::Reduce;
    }
    if pos.leverage > lim.max_leverage {
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
}
