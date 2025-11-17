#!/usr/bin/env python3
"""
Quick optimization test - tests first 10 combinations to verify pipeline works
"""

import subprocess
import re
import json
import shutil
from typing import Tuple
from pathlib import Path

# Reduced parameter ranges for quick test
BASE_SCORES = [4.5, 4.7]
TREND_STRENGTHS = [0.5, 0.6]
SL_MULTIPLIERS = [2.0, 3.0]
TP_MULTIPLIERS = [5.0, 6.0]

def modify_config(base_score: float, trend_strength: float, sl_mult: float, tp_mult: float):
    """Modify config.yaml with new parameters"""
    with open('config.yaml', 'r') as f:
        lines = f.readlines()
    
    for i, line in enumerate(lines):
        if line.strip().startswith('base_min_score:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}base_min_score: {base_score}\n"
        elif line.strip().startswith('trend_threshold_hft:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}trend_threshold_hft: {trend_strength}\n"
        elif line.strip().startswith('trend_threshold_normal:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}trend_threshold_normal: {trend_strength}\n"
        elif line.strip().startswith('atr_sl_multiplier:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}atr_sl_multiplier: {sl_mult}\n"
        elif line.strip().startswith('atr_tp_multiplier:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}atr_tp_multiplier: {tp_mult}\n"
    
    with open('config.yaml', 'w') as f:
        f.writelines(lines)

def run_backtest() -> Tuple[float, float, int, int]:
    """Run backtest and parse results"""
    try:
        result = subprocess.run(
            ['cargo', 'test', '--test', 'backtest', 
             'test_strategy_with_multiple_binance_symbols', 
             '--', '--ignored', '--nocapture'],
            capture_output=True,
            text=True,
            timeout=600
        )
        
        output = result.stdout + result.stderr
        
        # Parse aggregate win rate
        win_rate_match = re.search(r'Aggregate Win Rate: ([\d.]+)%', output)
        win_rate = float(win_rate_match.group(1)) if win_rate_match else 0.0
        
        # Parse total PnL
        pnl_match = re.search(r'Total PnL \(All Symbols\): \$([-\d.]+)', output)
        pnl = float(pnl_match.group(1)) if pnl_match else 0.0
        
        # Parse total trades
        trades_match = re.search(r'Total Trades: (\d+)', output)
        total_trades = int(trades_match.group(1)) if trades_match else 0
        
        # Parse winning trades
        winning_match = re.search(r'Total Winning Trades: (\d+)', output)
        winning_trades = int(winning_match.group(1)) if winning_match else 0
        
        return win_rate, pnl, total_trades, winning_trades
    except subprocess.TimeoutExpired:
        print(f"  ⚠️  Timeout (600s)")
        return 0.0, 0.0, 0, 0
    except Exception as e:
        print(f"  ❌ Error: {e}")
        return 0.0, 0.0, 0, 0

def main():
    print("🔍 Quick Optimization Test (16 combinations)\n")
    
    config_backup = 'config.yaml.bak'
    shutil.copy('config.yaml', config_backup)
    
    try:
        results = []
        best_result = None
        
        total = len(BASE_SCORES) * len(TREND_STRENGTHS) * len(SL_MULTIPLIERS) * len(TP_MULTIPLIERS)
        print(f"Testing {total} combinations...\n")
        
        combo = 0
        for base_score in BASE_SCORES:
            for trend_strength in TREND_STRENGTHS:
                for sl_mult in SL_MULTIPLIERS:
                    for tp_mult in TP_MULTIPLIERS:
                        combo += 1
                        print(f"[{combo}/{total}] Testing: "
                              f"BASE={base_score}, trend={trend_strength}, "
                              f"SL={sl_mult}x, TP={tp_mult}x")
                        
                        modify_config(base_score, trend_strength, sl_mult, tp_mult)
                        win_rate, pnl, total_trades, winning_trades = run_backtest()
                        
                        result = {
                            'base_score': base_score,
                            'trend_strength': trend_strength,
                            'sl_multiplier': sl_mult,
                            'tp_multiplier': tp_mult,
                            'win_rate': win_rate,
                            'pnl': pnl,
                            'total_trades': total_trades,
                            'winning_trades': winning_trades
                        }
                        results.append(result)
                        
                        print(f"  ✅ Win Rate={win_rate:.2f}%, PnL=${pnl:.2f}, Trades={total_trades}\n")
                        
                        if win_rate >= 50.0 and pnl > 0.0:
                            if best_result is None or \
                               win_rate > best_result['win_rate'] or \
                               (win_rate == best_result['win_rate'] and pnl > best_result['pnl']):
                                best_result = result
        
        shutil.copy(config_backup, 'config.yaml')
        
        print("\n" + "="*80)
        print("QUICK TEST RESULTS")
        print("="*80)
        
        if best_result:
            print(f"\n🏆 BEST: Win Rate={best_result['win_rate']:.2f}%, PnL=${best_result['pnl']:.2f}")
            print(f"   BASE={best_result['base_score']}, trend={best_result['trend_strength']}, "
                  f"SL={best_result['sl_multiplier']}x, TP={best_result['tp_multiplier']}x")
        else:
            if results:
                best_by_pnl = max(results, key=lambda x: x['pnl'])
                print(f"\n🏆 BEST BY PnL: ${best_by_pnl['pnl']:.2f}, Win Rate={best_by_pnl['win_rate']:.2f}%")
                print(f"   BASE={best_by_pnl['base_score']}, trend={best_by_pnl['trend_strength']}, "
                      f"SL={best_by_pnl['sl_multiplier']}x, TP={best_by_pnl['tp_multiplier']}x")
        
        print(f"\n📊 All Results:")
        for r in sorted(results, key=lambda x: x['pnl'], reverse=True):
            print(f"  PnL=${r['pnl']:7.2f}, WR={r['win_rate']:5.2f}%, "
                  f"BASE={r['base_score']}, trend={r['trend_strength']}, "
                  f"SL={r['sl_multiplier']}x, TP={r['tp_multiplier']}x")
        
        with open('optimization_quick_results.json', 'w') as f:
            json.dump(results, f, indent=2)
        print(f"\n✅ Results saved to optimization_quick_results.json")
    
    finally:
        if Path(config_backup).exists():
            shutil.copy(config_backup, 'config.yaml')

if __name__ == '__main__':
    main()

