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
