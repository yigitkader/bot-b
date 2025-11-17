"""
Main optimization coordinator

This module provides the core optimization engine that coordinates
parameter space exploration, config management, backtest execution,
and result analysis.

Architecture:
- Extensible: Easy to add new optimization strategies
- Configurable: All parameters driven by config
- Observable: Progress tracking and result persistence
- Testable: Modular design for easy testing
"""

import json
from pathlib import Path
from typing import Optional, Callable, Iterator
from .parameter_space import ParameterSpace, OptimizationPreset, ParameterSet
from .config_manager import ConfigManager
from .backtest_runner import BacktestRunner, BacktestResult
from .result_analyzer import ResultAnalyzer


class Optimizer:
    """Main optimization coordinator"""
    
    def __init__(
        self,
        parameter_space: ParameterSpace,
        config_path: str = 'config.yaml',
        results_path: str = 'optimization_results.json',
        test_name: str = 'test_strategy_with_multiple_binance_symbols',
        timeout: int = 600,
    ):
        self.parameter_space = parameter_space
        self.config_manager = ConfigManager(config_path)
        self.backtest_runner = BacktestRunner(test_name, timeout)
        self.result_analyzer = ResultAnalyzer()
        self.results_path = Path(results_path)
        self._current_combination = 0
        self._total_combinations = parameter_space.total_combinations()
    
    def run(
        self,
        progress_callback: Optional[Callable[[int, int, ParameterSet, Optional[BacktestResult]], None]] = None,
        early_stopping: Optional[Callable[[ResultAnalyzer], bool]] = None,
    ) -> ResultAnalyzer:
        """Run full optimization"""
        print("🔍 Starting systematic strategy optimization...\n")
        print(f"Testing {self._total_combinations} combinations...\n")
        
        self.config_manager.backup()
        
        try:
            for params in self.parameter_space.generate_combinations():
                self._current_combination += 1
                
                if progress_callback:
                    progress_callback(self._current_combination, self._total_combinations, params)
                else:
                    self._print_progress(params)
                
                self.config_manager.apply_parameters(params)
                backtest_result = self.backtest_runner.run()
                
                self.result_analyzer.add_result(params, backtest_result)
                
                if progress_callback:
                    progress_callback(
                        self._current_combination,
                        self._total_combinations,
                        params,
                        backtest_result,
                    )
                else:
                    self._print_result(backtest_result)
                
                self._save_results()
                
                # Early stopping check
                if early_stopping and early_stopping(self.result_analyzer):
                    print(f"\n⏹️  Early stopping triggered at combination {self._current_combination}")
                    break
        
        finally:
            self.config_manager.restore()
        
        return self.result_analyzer
    
    def _print_progress(self, params) -> None:
        """Print current combination being tested"""
        print(
            f"[{self._current_combination}/{self._total_combinations}] "
            f"Testing: BASE={params.base_score}, trend={params.trend_strength}, "
            f"SL={params.sl_multiplier}x, TP={params.tp_multiplier}x"
        )
    
    def _print_result(self, result) -> None:
        """Print backtest result"""
        status = "✅" if result.success else "❌"
        print(
            f"  {status} Result: Win Rate={result.win_rate:.2f}%, "
            f"PnL=${result.pnl:.2f}, Trades={result.total_trades}"
        )
        if result.error:
            print(f"  ⚠️  {result.error}")
        print()
    
    def _save_results(self) -> None:
        """Save results to JSON file"""
        with open(self.results_path, 'w') as f:
            json.dump(self.result_analyzer.results, f, indent=2)
    
    def get_progress(self) -> tuple:
        """Get current progress"""
        return self._current_combination, self._total_combinations

