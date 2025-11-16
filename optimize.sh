#!/bin/bash
# Strategy optimization script
# Tests different parameter combinations to find optimal settings

echo "🔍 Starting systematic strategy optimization..."
echo ""

# Parameter ranges to test
BASE_SCORES=(4.5 4.7 4.9 5.0 5.1)
TREND_STRENGTHS=(0.5 0.6 0.65 0.7)
SL_MULTIPLIERS=(1.5 2.0)
TP_MULTIPLIERS=(4.0 4.5 5.0)

RESULTS_FILE="optimization_results.json"
BEST_WIN_RATE=0.0
BEST_PNL=0.0
BEST_PARAMS=""

echo "Testing combinations..."
echo ""

for base_score in "${BASE_SCORES[@]}"; do
    for trend_strength in "${TREND_STRENGTHS[@]}"; do
        for sl_mult in "${SL_MULTIPLIERS[@]}"; do
            for tp_mult in "${TP_MULTIPLIERS[@]}"; do
                echo "Testing: BASE_MIN_SCORE=$base_score, trend_strength=$trend_strength, SL=${sl_mult}x, TP=${tp_mult}x"
                
                # Modify trending.rs
                sed -i.bak "s/const BASE_MIN_SCORE: f64 = [0-9.]*/const BASE_MIN_SCORE: f64 = $base_score/" src/trending.rs
                sed -i.bak "s/let is_strong_trend = trend_strength >= [0-9.]*/let is_strong_trend = trend_strength >= $trend_strength/" src/trending.rs
                sed -i.bak "s/const ATR_SL_MULTIPLIER: f64 = [0-9.]*/const ATR_SL_MULTIPLIER: f64 = $sl_mult/" src/trending.rs
                sed -i.bak "s/const ATR_TP_MULTIPLIER: f64 = [0-9.]*/const ATR_TP_MULTIPLIER: f64 = $tp_mult/" src/trending.rs
                
                # Run backtest
                OUTPUT=$(cargo test --test backtest test_strategy_with_multiple_binance_symbols -- --ignored --nocapture 2>&1)
                
                # Parse results
                WIN_RATE=$(echo "$OUTPUT" | grep "Aggregate Win Rate" | sed 's/.*: //' | sed 's/%//' | awk '{print $1}')
                PNL=$(echo "$OUTPUT" | grep "Total PnL (All Symbols)" | sed 's/.*\$//' | awk '{print $1}')
                
                echo "  Result: Win Rate=${WIN_RATE}%, PnL=\$${PNL}"
                echo ""
                
                # Track best (prioritize win_rate > 50% and positive PnL)
                if (( $(echo "$WIN_RATE >= 50.0" | bc -l) )) && (( $(echo "$PNL > 0" | bc -l) )); then
                    if (( $(echo "$WIN_RATE > $BEST_WIN_RATE" | bc -l) )) || \
                       (( $(echo "$WIN_RATE == $BEST_WIN_RATE && $PNL > $BEST_PNL" | bc -l) )); then
                        BEST_WIN_RATE=$WIN_RATE
                        BEST_PNL=$PNL
                        BEST_PARAMS="BASE_MIN_SCORE=$base_score, trend_strength=$trend_strength, SL=${sl_mult}x, TP=${tp_mult}x"
                    fi
                fi
            done
        done
    done
done

# Restore original file
mv src/trending.rs.bak src/trending.rs 2>/dev/null || true

echo ""
echo "🏆 BEST OPTIMIZATION:"
echo "$BEST_PARAMS"
echo "Win Rate: ${BEST_WIN_RATE}%"
echo "PnL: \$${BEST_PNL}"

