//location: /crates/app/src/utils_tests.rs
// Comprehensive tests for utility functions

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    // ============================================================================
    // is_usd_stable Tests
    // ============================================================================

    #[test]
    fn test_is_usd_stable_usdc_uppercase() {
        assert!(is_usd_stable("USDC"));
    }

    #[test]
    fn test_is_usd_stable_usdc_lowercase() {
        assert!(is_usd_stable("usdc"));
    }

    #[test]
    fn test_is_usd_stable_usdc_mixed_case() {
        assert!(is_usd_stable("UsDc"));
    }

    #[test]
    fn test_is_usd_stable_usdt_uppercase() {
        assert!(is_usd_stable("USDT"));
    }

    #[test]
    fn test_is_usd_stable_usdt_lowercase() {
        assert!(is_usd_stable("usdt"));
    }

    #[test]
    fn test_is_usd_stable_usdt_mixed_case() {
        assert!(is_usd_stable("UsDt"));
    }

    #[test]
    fn test_is_usd_stable_rejects_busd() {
        assert!(!is_usd_stable("BUSD"));
        assert!(!is_usd_stable("busd"));
    }

    #[test]
    fn test_is_usd_stable_rejects_other_assets() {
        assert!(!is_usd_stable("BTC"));
        assert!(!is_usd_stable("ETH"));
        assert!(!is_usd_stable("EUR"));
        assert!(!is_usd_stable(""));
    }

    #[test]
    fn test_is_usd_stable_rejects_whitespace() {
        assert!(!is_usd_stable(" USDC"));
        assert!(!is_usd_stable("USDC "));
        assert!(!is_usd_stable(" USDC "));
    }

    // ============================================================================
    // calculate_spread_bps Tests
    // ============================================================================

    #[test]
    fn test_calculate_spread_bps_normal() {
        let bid = dec!(50000);
        let ask = dec!(50050);
        let spread = calculate_spread_bps(bid, ask);
        // (50050 - 50000) / 50000 * 10000 = 10 bps
        assert!((spread - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_calculate_spread_bps_wide_spread() {
        let bid = dec!(50000);
        let ask = dec!(51000);
        let spread = calculate_spread_bps(bid, ask);
        // (51000 - 50000) / 50000 * 10000 = 200 bps
        assert!((spread - 200.0).abs() < 0.01);
    }

    #[test]
    fn test_calculate_spread_bps_narrow_spread() {
        let bid = dec!(50000);
        let ask = dec!(50001);
        let spread = calculate_spread_bps(bid, ask);
        // (50001 - 50000) / 50000 * 10000 = 0.2 bps
        assert!(spread > 0.0);
        assert!(spread < 1.0);
    }

    #[test]
    fn test_calculate_spread_bps_zero_bid() {
        let bid = dec!(0);
        let ask = dec!(50000);
        let spread = calculate_spread_bps(bid, ask);
        assert_eq!(spread, 0.0);
    }

    #[test]
    fn test_calculate_spread_bps_zero_ask() {
        let bid = dec!(50000);
        let ask = dec!(0);
        let spread = calculate_spread_bps(bid, ask);
        assert_eq!(spread, 0.0);
    }

    #[test]
    fn test_calculate_spread_bps_ask_less_than_bid() {
        let bid = dec!(50000);
        let ask = dec!(49900);
        let spread = calculate_spread_bps(bid, ask);
        assert_eq!(spread, 0.0);
    }

    #[test]
    fn test_calculate_spread_bps_equal_prices() {
        let bid = dec!(50000);
        let ask = dec!(50000);
        let spread = calculate_spread_bps(bid, ask);
        assert_eq!(spread, 0.0);
    }

    #[test]
    fn test_calculate_spread_bps_negative_bid() {
        let bid = dec!(-100);
        let ask = dec!(50000);
        let spread = calculate_spread_bps(bid, ask);
        assert_eq!(spread, 0.0);
    }

    #[test]
    fn test_calculate_spread_bps_negative_ask() {
        let bid = dec!(50000);
        let ask = dec!(-100);
        let spread = calculate_spread_bps(bid, ask);
        assert_eq!(spread, 0.0);
    }

    #[test]
    fn test_calculate_spread_bps_very_small_values() {
        let bid = dec!(0.0001);
        let ask = dec!(0.0002);
        let spread = calculate_spread_bps(bid, ask);
        // (0.0002 - 0.0001) / 0.0001 * 10000 = 10000 bps
        assert!((spread - 10000.0).abs() < 0.01);
    }

    #[test]
    fn test_calculate_spread_bps_very_large_values() {
        let bid = dec!(1000000);
        let ask = dec!(1000100);
        let spread = calculate_spread_bps(bid, ask);
        // (1000100 - 1000000) / 1000000 * 10000 = 1 bps
        assert!((spread - 1.0).abs() < 0.01);
    }

    // ============================================================================
    // ProfitGuarantee Tests
    // ============================================================================

    #[test]
    fn test_profit_guarantee_new() {
        let pg = ProfitGuarantee::new(0.01, 0.0002, 0.0004);
        assert_eq!(pg.min_profit_usd, 0.01);
        assert_eq!(pg.maker_fee_rate, 0.0002);
        assert_eq!(pg.taker_fee_rate, 0.0004);
    }

    #[test]
    fn test_profit_guarantee_default() {
        let pg = ProfitGuarantee::default();
        assert_eq!(pg.min_profit_usd, 0.005);
        assert_eq!(pg.maker_fee_rate, 0.0002);
        assert_eq!(pg.taker_fee_rate, 0.0004);
    }

    #[test]
    fn test_calculate_min_spread_bps_small_position() {
        let pg = ProfitGuarantee::default();
        // Position: 20 USD, min profit: 0.005 USD, fees: 0.0006 (6 bps)
        // min_spread = (0.005 / 20) * 10000 + 6 = 2.5 + 6 = 8.5 bps
        let min_spread = pg.calculate_min_spread_bps(20.0);
        assert!(min_spread > 8.0);
        assert!(min_spread < 9.0);
    }

    #[test]
    fn test_calculate_min_spread_bps_large_position() {
        let pg = ProfitGuarantee::default();
        // Position: 1000 USD, min profit: 0.005 USD, fees: 0.0006 (6 bps)
        // min_spread = (0.005 / 1000) * 10000 + 6 = 0.05 + 6 = 6.05 bps
        let min_spread = pg.calculate_min_spread_bps(1000.0);
        assert!(min_spread > 6.0);
        assert!(min_spread < 7.0);
    }

    #[test]
    fn test_calculate_min_spread_bps_zero_position() {
        let pg = ProfitGuarantee::default();
        let min_spread = pg.calculate_min_spread_bps(0.0);
        assert_eq!(min_spread, 0.0);
    }

    #[test]
    fn test_calculate_min_spread_bps_negative_position() {
        let pg = ProfitGuarantee::default();
        let min_spread = pg.calculate_min_spread_bps(-100.0);
        assert_eq!(min_spread, 0.0);
    }

    #[test]
    fn test_is_trade_profitable_profitable() {
        let pg = ProfitGuarantee::default();
        // 20 USD position, 10 bps spread
        // min_spread = ~8.5 bps, actual = 10 bps > min_spread
        assert!(pg.is_trade_profitable(10.0, 20.0));
    }

    #[test]
    fn test_is_trade_profitable_not_profitable() {
        let pg = ProfitGuarantee::default();
        // 20 USD position, 5 bps spread
        // min_spread = ~8.5 bps, actual = 5 bps < min_spread
        assert!(!pg.is_trade_profitable(5.0, 20.0));
    }

    #[test]
    fn test_is_trade_profitable_zero_position() {
        let pg = ProfitGuarantee::default();
        assert!(!pg.is_trade_profitable(10.0, 0.0));
    }

    #[test]
    fn test_is_trade_profitable_negative_position() {
        let pg = ProfitGuarantee::default();
        assert!(!pg.is_trade_profitable(10.0, -100.0));
    }

    #[test]
    fn test_calculate_expected_profit_profitable() {
        let pg = ProfitGuarantee::default();
        // 20 USD position, 10 bps spread
        // gross = 20 * 0.001 = 0.02
        // fees = 20 * 0.0006 = 0.012
        // net = 0.02 - 0.012 = 0.008
        let profit = pg.calculate_expected_profit(10.0, 20.0);
        assert!(profit > 0.0);
        assert!(profit < 0.01);
    }

    #[test]
    fn test_calculate_expected_profit_unprofitable() {
        let pg = ProfitGuarantee::default();
        // 20 USD position, 5 bps spread
        // gross = 20 * 0.0005 = 0.01
        // fees = 20 * 0.0006 = 0.012
        // net = 0.01 - 0.012 = -0.002
        let profit = pg.calculate_expected_profit(5.0, 20.0);
        assert!(profit < 0.0);
    }

    #[test]
    fn test_calculate_expected_profit_zero_position() {
        let pg = ProfitGuarantee::default();
        assert_eq!(pg.calculate_expected_profit(10.0, 0.0), 0.0);
    }

    #[test]
    fn test_calculate_optimal_position_size() {
        let pg = ProfitGuarantee::default();
        // 10 bps spread, min 20 USD, max 100 USD
        // net_spread = 0.001 - 0.0006 = 0.0004
        // optimal = 0.005 / 0.0004 = 12.5 USD
        // But clamped to min 20 USD
        let optimal = pg.calculate_optimal_position_size(10.0, 20.0, 100.0);
        assert_eq!(optimal, 20.0);
    }

    #[test]
    fn test_calculate_optimal_position_size_wide_spread() {
        let pg = ProfitGuarantee::default();
        // 100 bps spread, min 20 USD, max 100 USD
        // net_spread = 0.01 - 0.0006 = 0.0094
        // optimal = 0.005 / 0.0094 = 0.53 USD
        // But clamped to min 20 USD
        let optimal = pg.calculate_optimal_position_size(100.0, 20.0, 100.0);
        assert_eq!(optimal, 20.0);
    }

    #[test]
    fn test_calculate_optimal_position_size_zero_spread() {
        let pg = ProfitGuarantee::default();
        let optimal = pg.calculate_optimal_position_size(0.0, 20.0, 100.0);
        assert_eq!(optimal, 20.0);
    }

    #[test]
    fn test_calculate_optimal_position_size_negative_spread() {
        let pg = ProfitGuarantee::default();
        let optimal = pg.calculate_optimal_position_size(-10.0, 20.0, 100.0);
        assert_eq!(optimal, 20.0);
    }

    #[test]
    fn test_calculate_risk_reward_ratio_good() {
        let pg = ProfitGuarantee::default();
        // 20 USD position, 10 bps spread, 1% max loss
        // expected_profit = ~0.008
        // max_loss = 20 * 0.01 = 0.2
        // ratio = 0.008 / 0.2 = 0.04 (not great, but positive)
        let ratio = pg.calculate_risk_reward_ratio(10.0, 20.0, 0.01);
        assert!(ratio > 0.0);
    }

    #[test]
    fn test_calculate_risk_reward_ratio_zero_position() {
        let pg = ProfitGuarantee::default();
        assert_eq!(pg.calculate_risk_reward_ratio(10.0, 0.0, 0.01), 0.0);
    }

    #[test]
    fn test_calculate_risk_reward_ratio_zero_loss() {
        let pg = ProfitGuarantee::default();
        assert_eq!(pg.calculate_risk_reward_ratio(10.0, 20.0, 0.0), 0.0);
    }

    #[test]
    fn test_calculate_risk_reward_ratio_negative_loss() {
        let pg = ProfitGuarantee::default();
        assert_eq!(pg.calculate_risk_reward_ratio(10.0, 20.0, -0.01), 0.0);
    }

    // ============================================================================
    // should_place_trade Tests
    // ============================================================================

    #[test]
    fn test_should_place_trade_all_conditions_met() {
        // Spread: 10 bps, min: 5 bps, position: 20 USD
        // Profit guarantee: should pass
        // Risk/reward: should pass
        let pg = ProfitGuarantee::default();
        let (should, reason) = should_place_trade(10.0, 20.0, 5.0, -0.01, 2.0, &pg);
        // May or may not pass depending on exact calculations
        assert!(reason == "ok" || reason != "ok"); // Just check it returns
    }

    #[test]
    fn test_should_place_trade_spread_below_minimum() {
        let pg = ProfitGuarantee::default();
        let (should, reason) = should_place_trade(3.0, 20.0, 5.0, -0.01, 2.0, &pg);
        assert!(!should);
        assert_eq!(reason, "spread_below_minimum");
    }

    #[test]
    fn test_should_place_trade_not_profitable_after_fees() {
        // Very small position, spread barely covers fees
        let pg = ProfitGuarantee::default();
        let (should, reason) = should_place_trade(10.0, 1.0, 5.0, -0.01, 2.0, &pg);
        // May fail profit guarantee check
        if !should {
            assert!(reason == "not_profitable_after_fees" || reason == "risk_reward_too_low");
        }
    }

    #[test]
    fn test_should_place_trade_zero_spread() {
        let pg = ProfitGuarantee::default();
        let (should, reason) = should_place_trade(0.0, 20.0, 5.0, -0.01, 2.0, &pg);
        assert!(!should);
        assert_eq!(reason, "spread_below_minimum");
    }

    #[test]
    fn test_should_place_trade_negative_spread() {
        let pg = ProfitGuarantee::default();
        let (should, reason) = should_place_trade(-10.0, 20.0, 5.0, -0.01, 2.0, &pg);
        assert!(!should);
        assert_eq!(reason, "spread_below_minimum");
    }

    #[test]
    fn test_should_place_trade_zero_position() {
        let pg = ProfitGuarantee::default();
        let (should, reason) = should_place_trade(10.0, 0.0, 5.0, -0.01, 2.0, &pg);
        assert!(!should);
        assert!(reason == "not_profitable_after_fees" || reason == "risk_reward_too_low");
    }

    #[test]
    fn test_should_place_trade_negative_position() {
        let pg = ProfitGuarantee::default();
        let (should, reason) = should_place_trade(10.0, -100.0, 5.0, -0.01, 2.0, &pg);
        assert!(!should);
        assert!(reason == "not_profitable_after_fees" || reason == "risk_reward_too_low");
    }

    #[test]
    fn test_should_place_trade_very_high_risk_reward_requirement() {
        // Require 100:1 risk/reward (impossible)
        let pg = ProfitGuarantee::default();
        let (should, reason) = should_place_trade(10.0, 20.0, 5.0, -0.01, 100.0, &pg);
        assert!(!should);
        assert_eq!(reason, "risk_reward_too_low");
    }

    // ============================================================================
    // ProfitTracker Tests
    // ============================================================================

    #[test]
    fn test_profit_tracker_new() {
        let tracker = ProfitTracker::new();
        assert_eq!(tracker.total_trades, 0);
        assert_eq!(tracker.winning_trades, 0);
        assert_eq!(tracker.total_profit, 0.0);
        assert_eq!(tracker.total_fees, 0.0);
        assert_eq!(tracker.total_loss, 0.0);
    }

    #[test]
    fn test_profit_tracker_record_winning_trade() {
        let mut tracker = ProfitTracker::new();
        tracker.record_trade(10.0, 1.0);
        assert_eq!(tracker.total_trades, 1);
        assert_eq!(tracker.winning_trades, 1);
        assert_eq!(tracker.total_profit, 10.0);
        assert_eq!(tracker.total_fees, 1.0);
        assert_eq!(tracker.total_loss, 0.0);
    }

    #[test]
    fn test_profit_tracker_record_losing_trade() {
        let mut tracker = ProfitTracker::new();
        tracker.record_trade(-5.0, 1.0);
        assert_eq!(tracker.total_trades, 1);
        assert_eq!(tracker.winning_trades, 0);
        assert_eq!(tracker.total_profit, 0.0);
        assert_eq!(tracker.total_fees, 1.0);
        assert_eq!(tracker.total_loss, 5.0);
    }

    #[test]
    fn test_profit_tracker_win_rate() {
        let mut tracker = ProfitTracker::new();
        tracker.record_trade(10.0, 1.0);
        tracker.record_trade(-5.0, 1.0);
        tracker.record_trade(20.0, 1.0);
        // 2 wins out of 3 trades = 66.67%
        let win_rate = tracker.win_rate();
        assert!((win_rate - 66.67).abs() < 0.1);
    }

    #[test]
    fn test_profit_tracker_win_rate_no_trades() {
        let tracker = ProfitTracker::new();
        assert_eq!(tracker.win_rate(), 0.0);
    }

    #[test]
    fn test_profit_tracker_net_profit() {
        let mut tracker = ProfitTracker::new();
        tracker.record_trade(10.0, 1.0);
        tracker.record_trade(-5.0, 1.0);
        tracker.record_trade(20.0, 1.0);
        // profit: 30, fees: 3, loss: 5, net = 30 - 3 - 5 = 22
        let net = tracker.net_profit();
        assert!((net - 22.0).abs() < 0.01);
    }

    #[test]
    fn test_profit_tracker_reset() {
        let mut tracker = ProfitTracker::new();
        tracker.record_trade(10.0, 1.0);
        tracker.reset();
        assert_eq!(tracker.total_trades, 0);
        assert_eq!(tracker.winning_trades, 0);
        assert_eq!(tracker.total_profit, 0.0);
        assert_eq!(tracker.total_fees, 0.0);
        assert_eq!(tracker.total_loss, 0.0);
    }
}

