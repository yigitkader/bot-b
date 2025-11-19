# Optimization Architecture

## 🏗️ Mimari Prensipler

### 1. Separation of Concerns
- **Python Layer**: High-level optimization coordination, config management, result analysis
- **Rust Layer**: Backtest execution, performance-critical calculations
- **Tests Layer**: Unit and integration tests only

### 2. Single Source of Truth
- All optimization logic in `optimization/` package
- Config-driven parameter management
- Centralized result storage

### 3. Extensibility
- Plugin-based parameter space definitions
- Strategy pattern for different optimization algorithms
- Easy to add new metrics and analyzers

## 📁 Dizin Yapısı

```
optimization/
├── __init__.py              # Package exports
├── __main__.py              # CLI entry point
├── parameter_space.py        # Parameter space definitions
├── config_manager.py         # Config.yaml management
├── backtest_runner.py        # Backtest execution & parsing
├── result_analyzer.py       # Result analysis & reporting
├── optimizer.py              # Main optimization coordinator
├── cli.py                    # Command-line interface
├── strategies/               # Optimization strategies (future)
│   ├── __init__.py
│   ├── grid_search.py        # Grid search strategy
│   ├── random_search.py      # Random search strategy
│   └── bayesian.py           # Bayesian optimization (future)
├── metrics/                  # Custom metrics (future)
│   ├── __init__.py
│   ├── sharpe.py
│   └── drawdown.py
└── README.md                 # Documentation
```

## 🔄 Data Flow

```
User Input (CLI)
    ↓
ParameterSpace (defines search space)
    ↓
Optimizer (coordinates)
    ↓
ConfigManager (applies params to config.yaml)
    ↓
BacktestRunner (executes cargo test)
    ↓
ResultAnalyzer (parses & analyzes)
    ↓
Results (JSON output)
```

## 🎯 Genişletilebilirlik

### Yeni Parametre Ekleme

1. **ParameterSpace'e ekle:**
```python
class ParameterSpace:
    def __init__(self, ..., new_param: List[float] = None):
        self.new_param = new_param or [default_values]
    
    def generate_combinations(self):
        for new_val in self.new_param:
            # Add to combinations
```

2. **ConfigManager'e ekle:**
```python
def apply_parameters(self, params: ParameterSet):
    # Add new parameter update logic
    if line.strip().startswith('new_param:'):
        lines[i] = f"{indent}new_param: {params.new_param}\n"
```

3. **ParameterSet'e ekle:**
```python
@dataclass
class ParameterSet:
    # ... existing fields
    new_param: float
```

### Yeni Optimizasyon Stratejisi

1. **Strategy interface oluştur:**
```python
# optimization/strategies/base.py
class OptimizationStrategy:
    def search(self, space: ParameterSpace) -> Iterator[ParameterSet]:
        raise NotImplementedError
```

2. **Yeni strateji implement et:**
```python
# optimization/strategies/random_search.py
class RandomSearchStrategy(OptimizationStrategy):
    def search(self, space: ParameterSpace) -> Iterator[ParameterSet]:
        # Random sampling logic
```

3. **Optimizer'a entegre et:**
```python
optimizer = Optimizer(
    parameter_space=space,
    strategy=RandomSearchStrategy()
)
```

### Yeni Metrik Ekleme

1. **BacktestRunner'da parse et:**
```python
def _parse_output(self, output: str) -> BacktestResult:
    # Add new metric parsing
    new_metric_match = re.search(r'New Metric: ([\d.]+)', output)
    new_metric = float(new_metric_match.group(1)) if new_metric_match else 0.0
```

2. **BacktestResult'a ekle:**
```python
@dataclass
class BacktestResult:
    # ... existing fields
    new_metric: float
```

3. **ResultAnalyzer'da kullan:**
```python
def get_best_by_new_metric(self) -> Optional[Dict]:
    return max(self.results, key=lambda x: x['new_metric'])
```

## 🧪 Test Stratejisi

### Unit Tests
- `tests/optimization/` - Python optimization package tests
- Test each module independently

### Integration Tests
- `tests/backtest.rs` - Backtest execution tests
- Verify end-to-end optimization flow

### E2E Tests
- Full optimization run with small parameter space
- Verify results are saved correctly

## 📊 Monitoring & Observability

### Progress Tracking
- Real-time progress updates
- Estimated time remaining
- Current best result

### Result Persistence
- JSON results file
- Config backups
- Optimization history

### Error Handling
- Graceful degradation
- Error recovery
- Detailed error logging

## 🚀 Future Enhancements

1. **Parallel Execution**: Run multiple backtests concurrently
2. **Early Stopping**: Stop if no improvement after N iterations
3. **Adaptive Search**: Focus on promising regions
4. **ML Integration**: Use ML to predict good parameters
5. **Visualization**: Plot optimization progress and results

