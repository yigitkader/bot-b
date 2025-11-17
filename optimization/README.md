# Strategy Optimization Package

Centralized optimization system for parameter tuning.

## Structure

```
optimization/
├── __init__.py          # Package exports
├── parameter_space.py   # Parameter definitions and presets
├── config_manager.py    # Config.yaml management
├── backtest_runner.py   # Backtest execution and parsing
├── result_analyzer.py   # Result analysis and reporting
├── optimizer.py         # Main optimization coordinator
└── cli.py               # Command-line interface
```

## Usage

### Basic Usage

```bash
# Quick test (16 combinations)
./optimize --preset quick

# Standard optimization (400 combinations)
./optimize --preset standard

# Wide optimization (1470 combinations)
./optimize --preset wide
```

### Check Status

```bash
./optimize --check
```

### Custom Configuration

```python
from optimization import Optimizer, ParameterSpace

# Define custom parameter space
space = ParameterSpace(
    base_scores=[4.5, 5.0, 5.5],
    trend_strengths=[0.5, 0.6, 0.7],
    sl_multipliers=[2.0, 3.0],
    tp_multipliers=[5.0, 6.0, 7.0],
)

# Run optimization
optimizer = Optimizer(parameter_space=space)
analyzer = optimizer.run()
analyzer.print_summary()
```

## Presets

- **quick**: 16 combinations (for testing)
- **standard**: 400 combinations (balanced)
- **wide**: 1470 combinations (comprehensive)

## Output

Results are saved to `optimization_results.json` with the following structure:

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

