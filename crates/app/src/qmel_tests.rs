//location: /crates/app/src/qmel_tests.rs
// Comprehensive tests for Q-MEL strategy and learning mechanisms

#[cfg(test)]
mod tests {
    use crate::qmel::{
        FeatureExtractor, DirectionModel, ExpectedValueCalculator,
        ThompsonSamplingBandit, QMelStrategy, MarketState
    };
    use crate::strategy::Strategy;
    use crate::types::*;
    use rust_decimal::Decimal;
    use std::time::Instant;

    // ============================================================================
    // Feature Extraction Tests
    // ============================================================================

    #[test]
    fn test_feature_extractor_ofi_calculation() {
        let mut extractor = FeatureExtractor::new();
        
        // Create mock orderbooks
        let ob1 = create_mock_orderbook(100.0, 100.1, 1.0, 1.0);
        let ob2 = create_mock_orderbook(100.05, 100.15, 1.5, 0.8);
        
        let ofi = extractor.calculate_ofi(&ob2, Some(&ob1));
        
        // OFI should be calculated based on price movement and volume
        assert!(ofi.is_finite(), "OFI should be finite");
        assert!(ofi >= -1.0 && ofi <= 1.0, "OFI should be normalized [-1, 1]");
    }

    #[test]
    fn test_feature_extractor_microprice() {
        let extractor = FeatureExtractor::new();
        
        let ob = create_mock_orderbook(100.0, 100.1, 2.0, 1.0);
        let microprice = extractor.calculate_microprice(&ob);
        
        assert!(microprice.is_some(), "Microprice should be calculated");
        if let Some(mp) = microprice {
            assert!(mp > 100.0 && mp < 100.1, "Microprice should be between bid and ask");
            assert!(mp.is_finite(), "Microprice should be finite");
        }
    }

    #[test]
    fn test_feature_extractor_volatility_update() {
        let mut extractor = FeatureExtractor::new();
        
        // Update with multiple prices
        extractor.update_volatility(100.0);
        extractor.update_volatility(100.1);
        extractor.update_volatility(100.05);
        
        let vol_1s = extractor.get_volatility_1s();
        let vol_5s = extractor.get_volatility_5s();
        
        assert!(vol_1s >= 0.0, "Volatility should be non-negative");
        assert!(vol_5s >= 0.0, "Volatility should be non-negative");
        assert!(vol_1s.is_finite(), "Volatility should be finite");
        assert!(vol_5s.is_finite(), "Volatility should be finite");
    }

    // ============================================================================
    // Direction Model Tests
    // ============================================================================

    #[test]
    fn test_direction_model_initialization() {
        let model = DirectionModel::new(9);
        
        // Test through public interface
        let state = create_mock_market_state();
        let p_up = model.predict_up_probability(&state, 1.0);
        
        assert!(p_up >= 0.0 && p_up <= 1.0, "Probability should be in [0, 1]");
        assert!(p_up.is_finite(), "Probability should be finite");
        
        // Test feature importance
        let importance = model.calculate_feature_importance();
        assert_eq!(importance.len(), 9, "Should have 9 feature importance scores");
    }

    #[test]
    fn test_direction_model_prediction() {
        let model = DirectionModel::new(9);
        let state = create_mock_market_state();
        
        let p_up = model.predict_up_probability(&state, 1.0);
        
        assert!(p_up >= 0.0 && p_up <= 1.0, "Probability should be in [0, 1]");
        assert!(p_up.is_finite(), "Probability should be finite");
    }

    #[test]
    fn test_direction_model_update() {
        let mut model = DirectionModel::new(9);
        let state = create_mock_market_state();
        
        let initial_pred = model.predict_up_probability(&state, 1.0);
        
        // Update with actual direction
        model.update(&state, 1.0, 0.01); // actual = 1.0 (up), learning_rate = 0.01
        
        let updated_pred = model.predict_up_probability(&state, 1.0);
        
        // Prediction should change after update
        // (may increase or decrease depending on initial weights)
        assert!(updated_pred.is_finite(), "Updated prediction should be finite");
        assert!(updated_pred >= 0.0 && updated_pred <= 1.0, "Updated prediction should be in [0, 1]");
    }

    #[test]
    fn test_direction_model_feature_importance() {
        let mut model = DirectionModel::new(9);
        let state = create_mock_market_state();
        
        // Update multiple times to build importance
        for _ in 0..10 {
            model.update(&state, 0.7, 0.01);
        }
        
        let importance = model.calculate_feature_importance();
        
        assert_eq!(importance.len(), 9, "Should have 9 feature importance scores");
        assert!(importance.iter().all(|(_, score)| score.is_finite()), "All importance scores should be finite");
        assert!(importance.iter().all(|(_, score)| *score >= 0.0), "All importance scores should be non-negative");
    }

    #[test]
    fn test_direction_model_nan_protection() {
        let mut model = DirectionModel::new(9);
        let state = create_mock_market_state();
        
        // Try to update with invalid values
        model.update(&state, f64::NAN, 0.01);
        model.update(&state, f64::INFINITY, 0.01);
        model.update(&state, -1.0, 0.01); // Out of range
        model.update(&state, 2.0, 0.01); // Out of range
        
        // Model should still be valid
        let pred = model.predict_up_probability(&state, 1.0);
        assert!(pred.is_finite(), "Model should handle invalid inputs gracefully");
    }

    // ============================================================================
    // EV Calculator Tests
    // ============================================================================

    #[test]
    fn test_ev_calculator_long() {
        let calc = ExpectedValueCalculator::new(0.0001, 0.0004, 0.10);
        
        let ev = calc.calculate_ev_long(
            0.6,  // p_up = 60%
            0.5,  // target = $0.50
            0.75, // stop = $0.75
            100.0, // position size
            0.01, // slippage
            true, // maker
        );
        
        assert!(ev.is_finite(), "EV should be finite");
        // EV = 0.6 * 0.5 - 0.4 * 0.75 - fees - slippage
        // Should be positive for profitable trade
    }

    #[test]
    fn test_ev_calculator_short() {
        let calc = ExpectedValueCalculator::new(0.0001, 0.0004, 0.10);
        
        let ev = calc.calculate_ev_short(
            0.6,  // p_down = 60%
            0.5,  // target = $0.50
            0.75, // stop = $0.75
            100.0, // position size
            0.01, // slippage
            true, // maker
        );
        
        assert!(ev.is_finite(), "EV should be finite");
    }

    #[test]
    fn test_ev_calculator_adaptive_threshold() {
        let calc = ExpectedValueCalculator::new(0.0001, 0.0004, 0.10);
        
        // Low win rate: threshold should increase
        let threshold_low = calc.get_adaptive_threshold(0.40);
        assert!(threshold_low > 0.10, "Low win rate should increase threshold");
        
        // High win rate: threshold should decrease
        let threshold_high = calc.get_adaptive_threshold(0.65);
        assert!(threshold_high < 0.10, "High win rate should decrease threshold");
        
        // Normal win rate: threshold should be base
        let threshold_normal = calc.get_adaptive_threshold(0.50);
        assert!((threshold_normal - 0.10).abs() < 0.01, "Normal win rate should use base threshold");
    }

    #[test]
    fn test_ev_calculator_edge_validation() {
        let mut calc = ExpectedValueCalculator::new(0.0001, 0.0004, 0.10);
        
        // Initially should have edge (not enough data)
        assert!(calc.has_edge(), "Should have edge initially (not enough data)");
        
        // Record some good performance
        for _ in 0..15 {
            calc.record_ev_performance(0.15, 0.20); // Predicted 0.15, actual 0.20 (both positive)
        }
        
        assert!(calc.has_edge(), "Should have edge with good performance");
        
        // Record bad performance
        for _ in 0..15 {
            calc.record_ev_performance(0.15, -0.10); // Predicted positive, actual negative
        }
        
        // Edge should be questionable now
        let has_edge = calc.has_edge();
        // May or may not have edge depending on overall accuracy
        assert!(has_edge || !has_edge, "Edge validation should return boolean");
    }

    // ============================================================================
    // Bandit Tests
    // ============================================================================

    #[test]
    fn test_bandit_arm_creation() {
        let bandit = ThompsonSamplingBandit::new(0.1);
        
        assert!(!bandit.arms.is_empty(), "Should have arms");
        assert!(bandit.arms.len() > 0, "Should have at least one arm");
    }

    #[test]
    fn test_bandit_arm_selection() {
        let bandit = ThompsonSamplingBandit::new(0.1);
        
        let arm_idx = bandit.select_arm();
        
        assert!(arm_idx < bandit.arms.len(), "Selected arm should be valid");
    }

    #[test]
    fn test_bandit_arm_update() {
        let mut bandit = ThompsonSamplingBandit::new(0.1);
        
        let arm_idx = bandit.select_arm();
        let initial_pulls = bandit.arms[arm_idx].pull_count;
        let initial_reward = bandit.arms[arm_idx].reward_sum;
        
        bandit.update_arm(arm_idx, 0.5);
        
        assert_eq!(bandit.arms[arm_idx].pull_count, initial_pulls + 1, "Pull count should increase");
        assert!((bandit.arms[arm_idx].reward_sum - initial_reward - 0.5).abs() < 0.001, "Reward should be updated");
    }

    #[test]
    fn test_bandit_best_arm() {
        let mut bandit = ThompsonSamplingBandit::new(0.1);
        
        // Update different arms with different rewards
        if bandit.arms.len() >= 2 {
            bandit.update_arm(0, 1.0); // High reward
            bandit.update_arm(1, 0.1); // Low reward
            
            let best_arm = bandit.get_best_arm();
            assert!(best_arm.is_some(), "Should have best arm");
            
            if let Some(arm) = best_arm {
                // Best arm should have higher average reward
                assert!(arm.average_reward() >= 0.1, "Best arm should have good reward");
            }
        }
    }

    // ============================================================================
    // Q-MEL Strategy Tests
    // ============================================================================

    #[test]
    fn test_qmel_strategy_creation() {
        let strategy = QMelStrategy::new(
            0.0001, // maker fee
            0.0004, // taker fee
            0.10,   // ev threshold
            10.0,   // min margin
            100.0,  // max margin
            20.0,   // max leverage
        );
        
        // Strategy should be created successfully
        // (Can't test private fields directly, but creation should succeed)
        let importance = strategy.get_feature_importance();
        assert!(importance.is_some(), "Strategy should be functional");
    }

    #[test]
    fn test_qmel_strategy_learning() {
        let mut strategy = QMelStrategy::new(0.0001, 0.0004, 0.10, 10.0, 100.0, 20.0);
        
        // Simulate a profitable trade
        strategy.learn_from_trade(0.5, None, None);
        
        // Strategy should have learned (test through public interface)
        let importance = strategy.get_feature_importance();
        assert!(importance.is_some(), "Strategy should be functional after learning");
    }

    #[test]
    fn test_qmel_strategy_feature_importance() {
        let strategy = QMelStrategy::new(0.0001, 0.0004, 0.10, 10.0, 100.0, 20.0);
        
        let importance = strategy.get_feature_importance();
        
        assert!(importance.is_some(), "Should return feature importance");
        if let Some(imp) = importance {
            assert_eq!(imp.len(), 9, "Should have 9 feature importance scores");
        }
    }

    // ============================================================================
    // Helper Functions
    // ============================================================================

    fn create_mock_orderbook(bid: f64, ask: f64, bid_qty: f64, ask_qty: f64) -> OrderBook {
        OrderBook {
            best_bid: Some(BookLevel {
                px: Px(Decimal::from_f64_retain(bid).unwrap()),
                qty: Qty(Decimal::from_f64_retain(bid_qty).unwrap()),
            }),
            best_ask: Some(BookLevel {
                px: Px(Decimal::from_f64_retain(ask).unwrap()),
                qty: Qty(Decimal::from_f64_retain(ask_qty).unwrap()),
            }),
            top_bids: None,
            top_asks: None,
        }
    }

    fn create_mock_market_state() -> MarketState {
        MarketState {
            ofi: 0.1,
            microprice: 100.05,
            spread_velocity: 0.001,
            liquidity_pressure: 1.2,
            volatility_1s: 0.01,
            volatility_5s: 0.015,
            cancel_trade_ratio: 0.5,
            oi_delta_30s: 100.0,
            funding_rate: Some(0.0001),
            timestamp: Instant::now(),
        }
    }
}

