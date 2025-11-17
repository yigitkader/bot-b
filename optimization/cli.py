"""
Command-line interface for optimization
"""

import argparse
import sys
from pathlib import Path
from .optimizer import Optimizer
from .parameter_space import ParameterSpace, OptimizationPreset
from .result_analyzer import ResultAnalyzer


def main():
    """Main CLI entry point"""
    parser = argparse.ArgumentParser(
        description='Strategy optimization tool',
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    
    parser.add_argument(
        '--preset',
        type=str,
        choices=['quick', 'standard', 'wide'],
        default='standard',
        help='Optimization preset (default: standard)',
    )
    
    parser.add_argument(
        '--config',
        type=str,
        default='config.yaml',
        help='Path to config.yaml (default: config.yaml)',
    )
    
    parser.add_argument(
        '--output',
        type=str,
        default='optimization_results.json',
        help='Path to results JSON file (default: optimization_results.json)',
    )
    
    parser.add_argument(
        '--test',
        type=str,
        default='test_strategy_with_multiple_binance_symbols',
        help='Backtest test name (default: test_strategy_with_multiple_binance_symbols)',
    )
    
    parser.add_argument(
        '--timeout',
        type=int,
        default=600,
        help='Backtest timeout in seconds (default: 600)',
    )
    
    parser.add_argument(
        '--check',
        action='store_true',
        help='Check optimization status and results',
    )
    
    args = parser.parse_args()
    
    if args.check:
        check_status(args.output)
        return
    
    preset_map = {
        'quick': OptimizationPreset.QUICK,
        'standard': OptimizationPreset.STANDARD,
        'wide': OptimizationPreset.WIDE,
    }
    
    preset = preset_map[args.preset]
    parameter_space = ParameterSpace(preset=preset)
    
    optimizer = Optimizer(
        parameter_space=parameter_space,
        config_path=args.config,
        results_path=args.output,
        test_name=args.test,
        timeout=args.timeout,
    )
    
    def progress_callback(combination, total, params, result=None):
        if result:
            status = "✅" if result.success else "❌"
            print(
                f"[{combination}/{total}] {status} "
                f"BASE={params.base_score}, trend={params.trend_strength}, "
                f"SL={params.sl_multiplier}x, TP={params.tp_multiplier}x | "
                f"WR={result.win_rate:.2f}%, PnL=${result.pnl:.2f}"
            )
    
    try:
        analyzer = optimizer.run(progress_callback=progress_callback)
        analyzer.print_summary()
        print(f"\n✅ Results saved to {args.output}")
    
    except KeyboardInterrupt:
        print("\n\n⚠️  Optimization interrupted by user")
        print(f"Progress saved to {args.output}")
        sys.exit(1)
    
    except Exception as e:
        print(f"\n❌ Error: {e}")
        sys.exit(1)


def check_status(results_path: str):
    """Check optimization status"""
    import json
    import subprocess
    
    print("📊 Optimization Status Check")
    print("=" * 60)
    print()
    
    # Check if process is running
    try:
        result = subprocess.run(
            ['ps', 'aux'],
            capture_output=True,
            text=True,
        )
        if 'optimize' in result.stdout or 'optimization' in result.stdout:
            print("✅ Optimization script is running")
        else:
            print("❌ Optimization script is not running")
    except:
        pass
    
    print()
    
    # Check if cargo test is running
    try:
        result = subprocess.run(
            ['ps', 'aux'],
            capture_output=True,
            text=True,
        )
        if 'cargo test.*backtest' in result.stdout:
            print("✅ Backtest is currently running")
        else:
            print("⏸️  No backtest running (between tests or completed)")
    except:
        pass
    
    print()
    
    # Check results file
    results_file = Path(results_path)
    if not results_file.exists():
        print("📊 Results file not found")
        return
    
    try:
        with open(results_file) as f:
            results = json.load(f)
        
        if not results:
            print("📊 No results yet")
            return
        
        print(f"✅ Results file exists")
        print(f"   Combinations tested: {len(results)}")
        print()
        
        if results:
            best = max(results, key=lambda x: x.get('pnl', 0))
            print("🏆 Best result so far:")
            print(f"   PnL: ${best.get('pnl', 0):.2f}")
            print(f"   Win Rate: {best.get('win_rate', 0):.2f}%")
            print(f"   BASE={best.get('base_score', 0)}, "
                  f"trend={best.get('trend_strength', 0)}, "
                  f"SL={best.get('sl_multiplier', 0)}x, "
                  f"TP={best.get('tp_multiplier', 0)}x")
    
    except Exception as e:
        print(f"❌ Error reading results: {e}")


if __name__ == '__main__':
    main()

