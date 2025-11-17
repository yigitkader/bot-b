#!/bin/bash
# Check optimization progress

echo "📊 Optimization Status Check"
echo "============================"
echo ""

# Check if process is running
if ps aux | grep -q "[o]ptimize.py"; then
    echo "✅ Optimization script is running"
    PID=$(ps aux | grep "[o]ptimize.py" | grep -v grep | awk '{print $2}')
    echo "   PID: $PID"
else
    echo "❌ Optimization script is not running"
fi

echo ""

# Check if cargo test is running
if ps aux | grep -q "[c]argo test.*backtest"; then
    echo "✅ Backtest is currently running"
else
    echo "⏸️  No backtest running (between tests or completed)"
fi

echo ""

# Check log file
if [ -f optimization_full.log ]; then
    echo "📝 Log file exists ($(wc -l < optimization_full.log) lines)"
    echo ""
    echo "Last 20 lines:"
    tail -20 optimization_full.log
else
    echo "📝 Log file not created yet"
fi

echo ""

# Check results file
if [ -f optimization_results.json ]; then
    echo "📊 Results file exists"
    if command -v python3 &> /dev/null; then
        COUNT=$(python3 -c "import json; f=open('optimization_results.json'); data=json.load(f); print(len(data))" 2>/dev/null || echo "0")
        echo "   Combinations tested: $COUNT"
        
        if [ "$COUNT" -gt 0 ]; then
            echo ""
            echo "Best result so far:"
            python3 -c "
import json
with open('optimization_results.json') as f:
    results = json.load(f)
    if results:
        best = max(results, key=lambda x: x['pnl'])
        print(f\"  PnL: \${best['pnl']:.2f}\")
        print(f\"  Win Rate: {best['win_rate']:.2f}%\")
        print(f\"  BASE={best['base_score']}, trend={best['trend_strength']}, SL={best['sl_multiplier']}x, TP={best['tp_multiplier']}x\")
" 2>/dev/null || echo "  (Could not parse)"
        fi
    fi
else
    echo "📊 Results file not created yet"
fi

echo ""

