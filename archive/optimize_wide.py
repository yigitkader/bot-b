#!/usr/bin/env python3
"""
Wide parameter optimization - tests broader ranges to find working parameters
"""

import subprocess
import re
import json
import shutil
from typing import Tuple
from pathlib import Path

# Wider parameter ranges
BASE_SCORES = [3.5, 4.0, 4.5, 5.0, 5.5, 6.0]
TREND_STRENGTHS = [0.4, 0.5, 0.6, 0.7, 0.8]
SL_MULTIPLIERS = [1.5, 2.0, 2.5, 3.0, 4.0]
TP_MULTIPLIERS = [4.0, 5.0, 6.0, 7.0, 8.0, 10.0]

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
        
        win_rate_match = re.search(r'Aggregate Win Rate: ([\d.]+)%', output)
        win_rate = float(win_rate_match.group(1)) if win_rate_match else 0.0
        
        pnl_match = re.search(r'Total PnL \(All Symbols\): \$([-\d.]+)', output)
        pnl = float(pnl_match.group(1)) if pnl_match else 0.0
        
        trades_match = re.search(r'Total Trades: (\d+)', output)
        total_trades = int(trades_match.group(1)) if trades_match else 0
        
        winning_match = re.search(r'Total Winning Trades: (\d+)', output)
        winning_trades = int(winning_match.group(1)) if winning_match else 0
        
        return win_rate, pnl, total_trades, winning_trades
    except Exception as e:
        print(f"  ❌ Error: {e}")
        return 0.0, 0.0, 0, 0

def main():
    print("🔍 Wide Parameter Optimization\n")
    
    config_backup = 'config.yaml.bak'
    shutil.copy('config.yaml', config_backup)
    
    try:
        results = []
        best_result = None
        
        total = len(BASE_SCORES) * len(TREND_STRENGTHS) * len(SL_MULTIPLIERS) * len(TP_MULTIPLIERS)
        print(f"Testing {total} combinations (this will take several hours)...\n")
        print("Starting optimization...\n")
        
        combo = 0
        for base_score in BASE_SCORES:
            for trend_strength in TREND_STRENGTHS:
                for sl_mult in SL_MULTIPLIERS:
                    for tp_mult in TP_MULTIPLIERS:
                        combo += 1
                        if combo % 50 == 0:
                            print(f"Progress: {combo}/{total} ({combo*100//total}%)\n")
                        
                        print(f"[{combo}/{total}] BASE={base_score}, trend={trend_strength}, SL={sl_mult}x, TP={tp_mult}x")
                        
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
                        
                        status = "✅" if pnl > 0 else "❌"
                        print(f"  {status} WR={win_rate:.2f}%, PnL=${pnl:.2f}, Trades={total_trades}\n")
                        
                        if win_rate >= 50.0 and pnl > 0.0:
                            if best_result is None or \
                               win_rate > best_result['win_rate'] or \
                               (win_rate == best_result['win_rate'] and pnl > best_result['pnl']):
                                best_result = result
                                print(f"  🎯 NEW BEST: WR={win_rate:.2f}%, PnL=${pnl:.2f}\n")
        
        shutil.copy(config_backup, 'config.yaml')
        
        print("\n" + "="*80)
        print("OPTIMIZATION SUMMARY")
        print("="*80)
        
        if best_result:
            print(f"\n🏆 BEST (Win Rate >= 50% & Positive PnL):")
            print(f"BASE_MIN_SCORE: {best_result['base_score']}")
            print(f"Trend Strength: {best_result['trend_strength']}")
            print(f"SL Multiplier: {best_result['sl_multiplier']}x")
            print(f"TP Multiplier: {best_result['tp_multiplier']}x")
            print(f"Win Rate: {best_result['win_rate']:.2f}%")
            print(f"PnL: ${best_result['pnl']:.2f}")
            print(f"Total Trades: {best_result['total_trades']}")
        else:
            if results:
                best_by_pnl = max(results, key=lambda x: x['pnl'])
                print(f"\n🏆 BEST BY PnL:")
                print(f"BASE_MIN_SCORE: {best_by_pnl['base_score']}")
                print(f"Trend Strength: {best_by_pnl['trend_strength']}")
                print(f"SL Multiplier: {best_by_pnl['sl_multiplier']}x")
                print(f"TP Multiplier: {best_by_pnl['tp_multiplier']}x")
                print(f"Win Rate: {best_by_pnl['win_rate']:.2f}%")
                print(f"PnL: ${best_by_pnl['pnl']:.2f}")
                print(f"Total Trades: {best_by_pnl['total_trades']}")
        
        if results:
            print(f"\n📊 TOP 10 BY PnL:")
            top_by_pnl = sorted(results, key=lambda x: x['pnl'], reverse=True)[:10]
            for i, r in enumerate(top_by_pnl, 1):
                print(f"{i:2}. PnL=${r['pnl']:8.2f}, WR={r['win_rate']:5.2f}%, "
                      f"BASE={r['base_score']}, trend={r['trend_strength']}, "
                      f"SL={r['sl_multiplier']}x, TP={r['tp_multiplier']}x")
        
        with open('optimization_wide_results.json', 'w') as f:
            json.dump(results, f, indent=2)
        print(f"\n✅ Results saved to optimization_wide_results.json")
    
    finally:
        if Path(config_backup).exists():
            shutil.copy(config_backup, 'config.yaml')

if __name__ == '__main__':
    main()

