#!/usr/bin/env python3
"""
Strategy optimization script
Tests different parameter combinations systematically to find optimal settings
"""

import subprocess
import re
import json
from typing import List, Tuple, Optional

# Parameter ranges to test
BASE_SCORES = [4.5, 4.7, 4.9, 5.0, 5.1]
TREND_STRENGTHS = [0.5, 0.6, 0.65, 0.7]
SL_MULTIPLIERS = [1.5, 2.0]
TP_MULTIPLIERS = [4.0, 4.5, 5.0]

def modify_trending_rs(base_score: float, trend_strength: float, sl_mult: float, tp_mult: float):
    """Modify trending.rs with new parameters"""
    with open('src/trending.rs', 'r') as f:
        content = f.read()
    
    # Replace BASE_MIN_SCORE
    content = re.sub(
        r'const BASE_MIN_SCORE: f64 = [0-9.]+',
        f'const BASE_MIN_SCORE: f64 = {base_score}',
        content
    )
    
    # Replace trend_strength threshold
    content = re.sub(
        r'let is_strong_trend = trend_strength >= [0-9.]+',
        f'let is_strong_trend = trend_strength >= {trend_strength}',
        content
    )
    
    # Replace SL multiplier
    content = re.sub(
        r'const ATR_SL_MULTIPLIER: f64 = [0-9.]+',
        f'const ATR_SL_MULTIPLIER: f64 = {sl_mult}',
        content
    )
    
    # Replace TP multiplier
    content = re.sub(
        r'const ATR_TP_MULTIPLIER: f64 = [0-9.]+',
        f'const ATR_TP_MULTIPLIER: f64 = {tp_mult}',
        content
    )
    
    with open('src/trending.rs', 'w') as f:
        f.write(content)

def run_backtest() -> Tuple[float, float, int, int]:
    """Run backtest and parse results"""
    try:
        result = subprocess.run(
            ['cargo', 'test', '--test', 'backtest', 
             'test_strategy_with_multiple_binance_symbols', 
             '--', '--ignored', '--nocapture'],
            capture_output=True,
            text=True,
            timeout=300
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
    except Exception as e:
        print(f"  Error: {e}")
        return 0.0, 0.0, 0, 0

def main():
    print("🔍 Starting systematic strategy optimization...\n")
    
    # Backup original file
    with open('src/trending.rs', 'r') as f:
        original_content = f.read()
    
    results = []
    best_result: Optional[Tuple] = None
    
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
                    
                    # Modify trending.rs
                    modify_trending_rs(base_score, trend_strength, sl_mult, tp_mult)
                    
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
                    
                    print(f"  Result: Win Rate={win_rate:.2f}%, PnL=${pnl:.2f}, Trades={total_trades}\n")
                    
                    # Track best result (prioritize win_rate >= 50% and positive PnL)
                    if win_rate >= 50.0 and pnl > 0.0:
                        if best_result is None or \
                           win_rate > best_result['win_rate'] or \
                           (win_rate == best_result['win_rate'] and pnl > best_result['pnl']):
                            best_result = result
    
    # Restore original file
    with open('src/trending.rs', 'w') as f:
        f.write(original_content)
    
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

if __name__ == '__main__':
    main()

