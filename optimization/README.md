# Strategy Optimization Package

Centralized optimization system for parameter tuning with extensible architecture.

## 🏗️ Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for detailed architecture documentation.

## 📦 Structure

```
optimization/
├── __init__.py              # Package exports
├── __main__.py              # CLI entry point
├── parameter_space.py        # Parameter space definitions
├── config_manager.py         # Config.yaml management
├── backtest_runner.py        # Backtest execution & parsing
├── result_analyzer.py        # Result analysis & reporting
├── optimizer.py              # Main optimization coordinator
├── cli.py                    # Command-line interface
├── strategies/               # Optimization strategies (future)
├── metrics/                  # Custom metrics (future)
└── README.md                 # This file
```

## 🚀 Quick Start

```bash
# Quick test (16 combinations)
python3 -m optimization --preset quick

# Standard optimization (400 combinations)
python3 -m optimization --preset standard

# Wide optimization (1470 combinations)
python3 -m optimization --preset wide

# Check status
python3 -m optimization --check
```

## 📖 Usage

### Basic Usage

```python
from optimization import Optimizer, ParameterSpace, OptimizationPreset

# Create parameter space
space = ParameterSpace(preset=OptimizationPreset.STANDARD)

# Create optimizer
optimizer = Optimizer(parameter_space=space)

# Run optimization
analyzer = optimizer.run()

# Print results
analyzer.print_summary()
```

### Custom Parameter Space

```python
from optimization import ParameterSpace

space = ParameterSpace(
    base_scores=[4.5, 5.0, 5.5],
    trend_strengths=[0.5, 0.6, 0.7],
    sl_multipliers=[2.0, 3.0],
    tp_multipliers=[5.0, 6.0, 7.0],
)
```

### Progress Callback

```python
def progress_callback(combination, total, params, result=None):
    if result:
        print(f"[{combination}/{total}] WR={result.win_rate:.2f}%, PnL=${result.pnl:.2f}")

optimizer.run(progress_callback=progress_callback)
```

### Early Stopping

```python
def early_stopping(analyzer: ResultAnalyzer) -> bool:
    best = analyzer.get_best(min_win_rate=60.0, require_positive_pnl=True)
    return best is not None  # Stop if we found a good result

optimizer.run(early_stopping=early_stopping)
```

## 🔧 Extending

### Adding New Parameters

1. Add to `ParameterSet` dataclass
2. Update `ParameterSpace.generate_combinations()`
3. Update `ConfigManager.apply_parameters()`

### Adding New Metrics

1. Parse in `BacktestRunner._parse_output()`
2. Add to `BacktestResult` dataclass
3. Use in `ResultAnalyzer`

### Adding New Strategies

1. Create strategy class in `strategies/`
2. Implement search algorithm
3. Integrate with `Optimizer`

See [ARCHITECTURE.md](ARCHITECTURE.md) for detailed examples.

## 📊 Output Format

Results are saved to `optimization_results.json`:

```json
[
  {
    "base_score": 4.5,
    "trend_strength": 0.5,
    "sl_multiplier": 2.0,
    "tp_multiplier": 5.0,
    "win_rate": 25.25,
    "pnl": -453.61,
    "total_trades": 15,
    "winning_trades": 3,
    "losing_trades": 12,
    "success": true
  }
]
```

## 🧪 Testing

```bash
# Run optimization tests (when implemented)
pytest tests/optimization/

# Run backtest tests
cargo test --test backtest
```

## 📝 Best Practices

1. **Always backup config**: ConfigManager handles this automatically
2. **Use presets**: Start with presets before custom spaces
3. **Monitor progress**: Use progress callbacks for long runs
4. **Save results**: Results are auto-saved after each combination
5. **Check status**: Use `--check` to monitor running optimizations

## 🔮 Future Enhancements

- [ ] Parallel backtest execution
- [ ] Bayesian optimization strategy
- [ ] Adaptive parameter space refinement
- [ ] ML-based parameter prediction
- [ ] Real-time visualization
- [ ] Distributed optimization
