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

ORIGINAL_CONFIG="config.yaml.bak"
cp config.yaml "$ORIGINAL_CONFIG"
restore_config() {
    mv "$ORIGINAL_CONFIG" config.yaml 2>/dev/null || true
}
trap restore_config EXIT

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
                
                python3 - "$base_score" "$trend_strength" "$sl_mult" "$tp_mult" <<'PY'
import sys, pathlib
path = pathlib.Path("config.yaml")
lines = path.read_text().splitlines()
base_score, trend, sl_mult, tp_mult = sys.argv[1:5]
targets = {
    "base_min_score": base_score,
    "trend_threshold_hft": trend,
    "trend_threshold_normal": trend,
    "atr_sl_multiplier": sl_mult,
    "atr_tp_multiplier": tp_mult,
}
for key, value in targets.items():
    key_found = False
    for idx, line in enumerate(lines):
        if line.strip().startswith(f"{key}:"):
            indent = line[:len(line) - len(line.lstrip())]
            lines[idx] = f"{indent}{key}: {value}"
            key_found = True
            break
    if not key_found:
        lines.append(f"  {key}: {value}")
path.write_text("\n".join(lines) + "\n")
PY
                
                # Run backtest
                OUTPUT=$(cargo test --test backtest test_strategy_with_multiple_binance_symbols -- --ignored --nocapture 2>&1)
                
                # Parse results
                WIN_RATE=$(echo "$OUTPUT" | grep "Aggregate Win Rate" | sed 's/.*: //' | sed 's/%//' | awk '{print $1}')
                PNL=$(echo "$OUTPUT" | grep "Total PnL (All Symbols)" | sed 's/.*\$//' | awk '{print $1}')
                WIN_RATE=${WIN_RATE:-0}
                PNL=${PNL:-0}
                
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

restore_config
trap - EXIT

echo ""
echo "🏆 BEST OPTIMIZATION:"
echo "$BEST_PARAMS"
echo "Win Rate: ${BEST_WIN_RATE}%"
echo "PnL: \$${BEST_PNL}"

