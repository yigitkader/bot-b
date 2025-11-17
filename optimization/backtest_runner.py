"""
Backtest execution and result parsing
"""

import subprocess
import re
from dataclasses import dataclass
from typing import Optional


@dataclass
class BacktestResult:
    """Results from a single backtest run"""
    win_rate: float
    pnl: float
    total_trades: int
    winning_trades: int
    losing_trades: int
    success: bool
    error: Optional[str] = None
    
    def to_dict(self) -> dict:
        return {
            'win_rate': self.win_rate,
            'pnl': self.pnl,
            'total_trades': self.total_trades,
            'winning_trades': self.winning_trades,
            'losing_trades': self.losing_trades,
            'success': self.success,
            'error': self.error,
        }


class BacktestRunner:
    """Runs backtests and parses results"""
    
    def __init__(
        self,
        test_name: str = 'test_strategy_with_multiple_binance_symbols',
        timeout: int = 600,
    ):
        self.test_name = test_name
        self.timeout = timeout
    
    def run(self) -> BacktestResult:
        """Execute backtest and return parsed results"""
        try:
            result = subprocess.run(
                [
                    'cargo', 'test', '--test', 'backtest',
                    self.test_name,
                    '--', '--ignored', '--nocapture'
                ],
                capture_output=True,
                text=True,
                timeout=self.timeout,
            )
            
            output = result.stdout + result.stderr
            return self._parse_output(output)
        
        except subprocess.TimeoutExpired:
            return BacktestResult(
                win_rate=0.0,
                pnl=0.0,
                total_trades=0,
                winning_trades=0,
                losing_trades=0,
                success=False,
                error=f"Timeout after {self.timeout}s",
            )
        
        except Exception as e:
            return BacktestResult(
                win_rate=0.0,
                pnl=0.0,
                total_trades=0,
                winning_trades=0,
                losing_trades=0,
                success=False,
                error=str(e),
            )
    
    def _parse_output(self, output: str) -> BacktestResult:
        """Parse backtest output to extract metrics"""
        # Try aggregate format first (multi-symbol)
        win_rate_match = re.search(r'Aggregate Win Rate: ([\d.]+)%', output)
        if not win_rate_match:
            # Try single symbol format
            win_rate_match = re.search(r'Win Rate: ([\d.]+)%', output)
        
        win_rate = float(win_rate_match.group(1)) if win_rate_match else 0.0
        
        # Try aggregate PnL format first
        pnl_match = re.search(r'Total PnL \(All Symbols\): \$([-\d.]+)', output)
        if not pnl_match:
            # Try single symbol format
            pnl_match = re.search(r'Total PnL: \$([-\d.]+)', output)
        
        pnl = float(pnl_match.group(1)) if pnl_match else 0.0
        
        # Parse trades
        trades_match = re.search(r'Total Trades: (\d+)', output)
        total_trades = int(trades_match.group(1)) if trades_match else 0
        
        # Try aggregate winning trades
        winning_match = re.search(r'Total Winning Trades: (\d+)', output)
        if not winning_match:
            # Try single symbol format
            winning_match = re.search(r'Winning Trades: (\d+)', output)
        winning_trades = int(winning_match.group(1)) if winning_match else 0
        
        # Try aggregate losing trades
        losing_match = re.search(r'Total Losing Trades: (\d+)', output)
        if not losing_match:
            # Try single symbol format
            losing_match = re.search(r'Losing Trades: (\d+)', output)
        losing_trades = int(losing_match.group(1)) if losing_match else 0
        
        # Success if we got at least win_rate or pnl
        success = (win_rate_match is not None or pnl_match is not None) and total_trades > 0
        
        return BacktestResult(
            win_rate=win_rate,
            pnl=pnl,
            total_trades=total_trades,
            winning_trades=winning_trades,
            losing_trades=losing_trades,
            success=success,
            error=None if success else "Failed to parse backtest output",
        )

