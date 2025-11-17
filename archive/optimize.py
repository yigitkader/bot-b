#!/usr/bin/env python3
"""
Strategy optimization script
Tests different parameter combinations systematically to find optimal settings
Uses config.yaml for parameter modification (no external dependencies)
"""

import subprocess
import re
import json
import shutil
from typing import List, Tuple, Optional
from pathlib import Path

# Parameter ranges to test - wider ranges to find working parameters
BASE_SCORES = [3.5, 4.0, 4.5, 5.0, 5.5, 6.0, 6.5]
TREND_STRENGTHS = [0.4, 0.5, 0.6, 0.7, 0.8]
SL_MULTIPLIERS = [1.5, 2.0, 2.5, 3.0, 3.5, 4.0]
TP_MULTIPLIERS = [4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]

def modify_config(base_score: float, trend_strength: float, sl_mult: float, tp_mult: float):
    """Modify config.yaml with new parameters using regex"""
    with open('config.yaml', 'r') as f:
        lines = f.readlines()
    
    modified = False
    for i, line in enumerate(lines):
        # Update base_min_score
        if line.strip().startswith('base_min_score:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}base_min_score: {base_score}\n"
            modified = True
        # Update trend_threshold_hft
        elif line.strip().startswith('trend_threshold_hft:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}trend_threshold_hft: {trend_strength}\n"
            modified = True
        # Update trend_threshold_normal
        elif line.strip().startswith('trend_threshold_normal:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}trend_threshold_normal: {trend_strength}\n"
            modified = True
        # Update atr_sl_multiplier
        elif line.strip().startswith('atr_sl_multiplier:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}atr_sl_multiplier: {sl_mult}\n"
            modified = True
        # Update atr_tp_multiplier
        elif line.strip().startswith('atr_tp_multiplier:'):
            indent = len(line) - len(line.lstrip())
            lines[i] = f"{' ' * indent}atr_tp_multiplier: {tp_mult}\n"
            modified = True
    
    # If keys don't exist, add them to trending section
    if not modified or not any('base_min_score:' in line for line in lines):
        # Find trending section and add keys
        in_trending = False
        trending_indent = 0
        for i, line in enumerate(lines):
            if line.strip().startswith('trending:'):
                in_trending = True
                trending_indent = len(line) - len(line.lstrip())
            elif in_trending and line.strip() and not line.strip().startswith('#'):
                if len(line) - len(line.lstrip()) <= trending_indent:
                    # End of trending section, insert before this line
                    indent = ' ' * (trending_indent + 2)
                    new_lines = [
                        f"{indent}base_min_score: {base_score}\n",
                        f"{indent}trend_threshold_hft: {trend_strength}\n",
                        f"{indent}trend_threshold_normal: {trend_strength}\n",
                        f"{indent}atr_sl_multiplier: {sl_mult}\n",
                        f"{indent}atr_tp_multiplier: {tp_mult}\n"
                    ]
                    lines[i:i] = new_lines
                    break
    
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
        
        # Parse winning/losing trades
        winning_match = re.search(r'Total Winning Trades: (\d+)', output)
        winning_trades = int(winning_match.group(1)) if winning_match else 0
        
        return win_rate, pnl, total_trades, winning_trades
    except subprocess.TimeoutExpired:
        print(f"  ⚠️  Timeout (600s) - skipping")
        return 0.0, 0.0, 0, 0
    except Exception as e:
        print(f"  ❌ Error: {e}")
        return 0.0, 0.0, 0, 0

def main():
    print("🔍 Starting systematic strategy optimization...\n")
    
    # Backup original config
    config_backup = 'config.yaml.bak'
    shutil.copy('config.yaml', config_backup)
    
    try:
        results = []
        best_result: Optional[dict] = None
        
        total_combinations = len(BASE_SCORES) * len(TREND_STRENGTHS) * len(SL_MULTIPLIERS) * len(TP_MULTIPLIERS)
        print(f"Testing {total_combinations} combinations...\n")
        
        combination_num = 0
        for base_score in BASE_SCORES:
            for trend_strength in TREND_STRENGTHS:
                for sl_mult in SL_MULTIPLIERS:
                    for tp_mult in TP_MULTIPLIERS:
                        combination_num += 1
                        print(f"[{combination_num}/{total_combinations}] Testing: "
                              f"BASE_MIN_SCORE={base_score}, trend_strength={trend_strength}, "
                              f"SL={sl_mult}x, TP={tp_mult}x")
                        
                        # Modify config.yaml
                        modify_config(base_score, trend_strength, sl_mult, tp_mult)
                        
                        # Run backtest
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
                        
                        print(f"  ✅ Result: Win Rate={win_rate:.2f}%, PnL=${pnl:.2f}, Trades={total_trades}\n")
                        
                        # Track best result (prioritize win_rate >= 50% and positive PnL)
                        if win_rate >= 50.0 and pnl > 0.0:
                            if best_result is None or \
                               win_rate > best_result['win_rate'] or \
                               (win_rate == best_result['win_rate'] and pnl > best_result['pnl']):
                                best_result = result
        
        # Restore original config
        shutil.copy(config_backup, 'config.yaml')
        
        # Save all results
        with open('optimization_results.json', 'w') as f:
            json.dump(results, f, indent=2)
        
        # Print summary
        print("\n" + "="*80)
        print("OPTIMIZATION SUMMARY")
        print("="*80)
        
        if best_result:
            print(f"\n🏆 BEST OPTIMIZATION (Win Rate >= 50% & Positive PnL):")
            print(f"BASE_MIN_SCORE: {best_result['base_score']}")
            print(f"Trend Strength: {best_result['trend_strength']}")
            print(f"SL Multiplier: {best_result['sl_multiplier']}x")
            print(f"TP Multiplier: {best_result['tp_multiplier']}x")
            print(f"Win Rate: {best_result['win_rate']:.2f}%")
            print(f"PnL: ${best_result['pnl']:.2f}")
            print(f"Total Trades: {best_result['total_trades']}")
        else:
            print("\n⚠️  No combination achieved 50%+ win rate with positive PnL")
            # Find best by PnL
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
        
        # Top 5 by win rate
        if results:
            print(f"\n📊 TOP 5 BY WIN RATE:")
            top_by_winrate = sorted(results, key=lambda x: x['win_rate'], reverse=True)[:5]
            for i, r in enumerate(top_by_winrate, 1):
                print(f"{i}. Win Rate={r['win_rate']:.2f}%, PnL=${r['pnl']:.2f}, "
                      f"BASE={r['base_score']}, trend={r['trend_strength']}, "
                      f"SL={r['sl_multiplier']}x, TP={r['tp_multiplier']}x")
            
            # Top 5 by PnL
            print(f"\n💰 TOP 5 BY PnL:")
            top_by_pnl = sorted(results, key=lambda x: x['pnl'], reverse=True)[:5]
            for i, r in enumerate(top_by_pnl, 1):
                print(f"{i}. PnL=${r['pnl']:.2f}, Win Rate={r['win_rate']:.2f}%, "
                      f"BASE={r['base_score']}, trend={r['trend_strength']}, "
                      f"SL={r['sl_multiplier']}x, TP={r['tp_multiplier']}x")
        
        print(f"\n✅ Results saved to optimization_results.json")
    
    finally:
        # Always restore original config
        if Path(config_backup).exists():
            shutil.copy(config_backup, 'config.yaml')

if __name__ == '__main__':
    main()
