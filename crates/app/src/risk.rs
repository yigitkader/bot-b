//location: /crates/app/src/risk.rs
// Core risk checking logic (moved from risk crate)

use crate::types::*;

#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub inv_cap: Qty,
    pub min_liq_gap_bps: f64,
    pub dd_limit_bps: i64,
    pub max_leverage: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskAction {
    Ok,
    Widen, // Kullanılıyor (main.rs:2082, quote_generator.rs:53)
    Reduce,
    Halt,
}

/// Check risk based on position, inventory, liquidation gap, and drawdown
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
    use rust_decimal::prelude::ToPrimitive;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn create_test_position(symbol: &str, qty: Decimal, entry: Decimal, leverage: u32) -> Position {
        Position {
            symbol: symbol.to_string(),
            qty: Qty(qty),
            entry: Px(entry),
            leverage,
            liq_px: None,
        }
    }

    fn create_test_limits(
        inv_cap: Decimal,
        min_liq_gap: f64,
        dd_limit: i64,
        max_leverage: u32,
    ) -> RiskLimits {
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

        let action = check_risk(&pos, inv, 500.0, -2500, &limits);
        assert_eq!(action, RiskAction::Halt);
    }

    #[test]
    fn test_risk_reduce_on_inventory_exceeded() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let limits = create_test_limits(dec!(0.5), 300.0, 2000, 10);

        let inv = Qty(dec!(0.6));
        let action = check_risk(&pos, inv, 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }

    #[test]
    fn test_risk_reduce_on_low_liquidation_gap() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 5);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);

        let action = check_risk(&pos, inv, 200.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }

    #[test]
    fn test_risk_reduce_on_leverage_exceeded() {
        let pos = create_test_position("BTCUSDT", dec!(0.1), dec!(50000), 15);
        let inv = Qty(dec!(0.05));
        let limits = create_test_limits(dec!(1.0), 300.0, 2000, 10);

        let action = check_risk(&pos, inv, 500.0, -100, &limits);
        assert_eq!(action, RiskAction::Reduce);
    }
}

