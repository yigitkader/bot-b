"""
Result analysis and reporting for optimization
"""

from typing import List, Optional, Dict
from .parameter_space import ParameterSet
from .backtest_runner import BacktestResult


class ResultAnalyzer:
    """Analyzes and reports optimization results"""
    
    def __init__(self):
        self.results: List[Dict] = []
    
    def add_result(
        self,
        params: ParameterSet,
        backtest_result: BacktestResult,
    ) -> None:
        """Add a result to the collection"""
        result = {
            **params.to_dict(),
            **backtest_result.to_dict(),
        }
        self.results.append(result)
    
    def get_best(
        self,
        min_win_rate: float = 50.0,
        require_positive_pnl: bool = True,
    ) -> Optional[Dict]:
        """Get best result based on criteria"""
        candidates = self.results
        
        if require_positive_pnl:
            candidates = [r for r in candidates if r['pnl'] > 0.0]
        
        candidates = [r for r in candidates if r['win_rate'] >= min_win_rate]
        
        if not candidates:
            return None
        
        return max(
            candidates,
            key=lambda x: (x['win_rate'], x['pnl'])
        )
    
    def get_best_by_pnl(self) -> Optional[Dict]:
        """Get result with highest PnL"""
        if not self.results:
            return None
        return max(self.results, key=lambda x: x['pnl'])
    
    def get_best_by_win_rate(self) -> Optional[Dict]:
        """Get result with highest win rate"""
        if not self.results:
            return None
        return max(self.results, key=lambda x: x['win_rate'])
    
    def get_top_n_by_pnl(self, n: int = 5) -> List[Dict]:
        """Get top N results by PnL"""
        if not self.results:
            return []
        return sorted(self.results, key=lambda x: x['pnl'], reverse=True)[:n]
    
    def get_top_n_by_win_rate(self, n: int = 5) -> List[Dict]:
        """Get top N results by win rate"""
        if not self.results:
            return []
        return sorted(self.results, key=lambda x: x['win_rate'], reverse=True)[:n]
    
    def get_summary(self) -> Dict:
        """Get summary statistics"""
        if not self.results:
            return {
                'total_combinations': 0,
                'successful_runs': 0,
                'avg_win_rate': 0.0,
                'avg_pnl': 0.0,
                'best_pnl': 0.0,
                'best_win_rate': 0.0,
            }
        
        successful = [r for r in self.results if r['success']]
        
        return {
            'total_combinations': len(self.results),
            'successful_runs': len(successful),
            'avg_win_rate': sum(r['win_rate'] for r in successful) / len(successful) if successful else 0.0,
            'avg_pnl': sum(r['pnl'] for r in successful) / len(successful) if successful else 0.0,
            'best_pnl': max((r['pnl'] for r in successful), default=0.0),
            'best_win_rate': max((r['win_rate'] for r in successful), default=0.0),
        }
    
    def print_summary(self) -> None:
        """Print formatted summary"""
        summary = self.get_summary()
        best = self.get_best()
        best_by_pnl = self.get_best_by_pnl()
        
        print("\n" + "="*80)
        print("OPTIMIZATION SUMMARY")
        print("="*80)
        
        print(f"\n📊 Statistics:")
        print(f"  Total Combinations: {summary['total_combinations']}")
        print(f"  Successful Runs: {summary['successful_runs']}")
        print(f"  Average Win Rate: {summary['avg_win_rate']:.2f}%")
        print(f"  Average PnL: ${summary['avg_pnl']:.2f}")
        
        if best:
            print(f"\n🏆 BEST (Win Rate >= 50% & Positive PnL):")
            print(f"  BASE_MIN_SCORE: {best['base_score']}")
            print(f"  Trend Strength: {best['trend_strength']}")
            print(f"  SL Multiplier: {best['sl_multiplier']}x")
            print(f"  TP Multiplier: {best['tp_multiplier']}x")
            print(f"  Win Rate: {best['win_rate']:.2f}%")
            print(f"  PnL: ${best['pnl']:.2f}")
            print(f"  Total Trades: {best['total_trades']}")
        else:
            if best_by_pnl:
                print(f"\n🏆 BEST BY PnL:")
                print(f"  BASE_MIN_SCORE: {best_by_pnl['base_score']}")
                print(f"  Trend Strength: {best_by_pnl['trend_strength']}")
                print(f"  SL Multiplier: {best_by_pnl['sl_multiplier']}x")
                print(f"  TP Multiplier: {best_by_pnl['tp_multiplier']}x")
                print(f"  Win Rate: {best_by_pnl['win_rate']:.2f}%")
                print(f"  PnL: ${best_by_pnl['pnl']:.2f}")
                print(f"  Total Trades: {best_by_pnl['total_trades']}")
        
        top_by_pnl = self.get_top_n_by_pnl(5)
        if top_by_pnl:
            print(f"\n💰 TOP 5 BY PnL:")
            for i, r in enumerate(top_by_pnl, 1):
                print(f"  {i}. PnL=${r['pnl']:8.2f}, WR={r['win_rate']:5.2f}%, "
                      f"BASE={r['base_score']}, trend={r['trend_strength']}, "
                      f"SL={r['sl_multiplier']}x, TP={r['tp_multiplier']}x")
        
        top_by_wr = self.get_top_n_by_win_rate(5)
        if top_by_wr:
            print(f"\n📈 TOP 5 BY WIN RATE:")
            for i, r in enumerate(top_by_wr, 1):
                print(f"  {i}. WR={r['win_rate']:5.2f}%, PnL=${r['pnl']:8.2f}, "
                      f"BASE={r['base_score']}, trend={r['trend_strength']}, "
                      f"SL={r['sl_multiplier']}x, TP={r['tp_multiplier']}x")

