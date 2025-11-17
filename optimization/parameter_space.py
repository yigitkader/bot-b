"""
Parameter space definitions for optimization
"""

from dataclasses import dataclass
from typing import List, Iterator
from enum import Enum


class OptimizationPreset(Enum):
    """Predefined parameter ranges for different optimization scenarios"""
    QUICK = "quick"
    STANDARD = "standard"
    WIDE = "wide"
    CUSTOM = "custom"


@dataclass
class ParameterSet:
    """A single parameter combination"""
    base_score: float
    trend_strength: float
    sl_multiplier: float
    tp_multiplier: float
    
    def to_dict(self) -> dict:
        return {
            'base_score': self.base_score,
            'trend_strength': self.trend_strength,
            'sl_multiplier': self.sl_multiplier,
            'tp_multiplier': self.tp_multiplier,
        }


class ParameterSpace:
    """Defines the parameter space for optimization"""
    
    PRESETS = {
        OptimizationPreset.QUICK: {
            'base_scores': [4.5, 4.7],
            'trend_strengths': [0.5, 0.6],
            'sl_multipliers': [2.0, 3.0],
            'tp_multipliers': [5.0, 6.0],
        },
        OptimizationPreset.STANDARD: {
            'base_scores': [4.5, 4.7, 4.9, 5.0, 5.1],
            'trend_strengths': [0.5, 0.6, 0.65, 0.7],
            'sl_multipliers': [2.0, 2.5, 3.0, 3.5],
            'tp_multipliers': [5.0, 5.5, 6.0, 6.5, 7.0],
        },
        OptimizationPreset.WIDE: {
            'base_scores': [3.5, 4.0, 4.5, 5.0, 5.5, 6.0, 6.5],
            'trend_strengths': [0.4, 0.5, 0.6, 0.7, 0.8],
            'sl_multipliers': [1.5, 2.0, 2.5, 3.0, 3.5, 4.0],
            'tp_multipliers': [4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0],
        },
    }
    
    def __init__(
        self,
        base_scores: List[float] = None,
        trend_strengths: List[float] = None,
        sl_multipliers: List[float] = None,
        tp_multipliers: List[float] = None,
        preset: OptimizationPreset = None,
    ):
        if preset and preset != OptimizationPreset.CUSTOM:
            preset_params = self.PRESETS[preset]
            self.base_scores = base_scores or preset_params['base_scores']
            self.trend_strengths = trend_strengths or preset_params['trend_strengths']
            self.sl_multipliers = sl_multipliers or preset_params['sl_multipliers']
            self.tp_multipliers = tp_multipliers or preset_params['tp_multipliers']
        else:
            self.base_scores = base_scores or self.PRESETS[OptimizationPreset.STANDARD]['base_scores']
            self.trend_strengths = trend_strengths or self.PRESETS[OptimizationPreset.STANDARD]['trend_strengths']
            self.sl_multipliers = sl_multipliers or self.PRESETS[OptimizationPreset.STANDARD]['sl_multipliers']
            self.tp_multipliers = tp_multipliers or self.PRESETS[OptimizationPreset.STANDARD]['tp_multipliers']
    
    def total_combinations(self) -> int:
        """Calculate total number of parameter combinations"""
        return (
            len(self.base_scores) *
            len(self.trend_strengths) *
            len(self.sl_multipliers) *
            len(self.tp_multipliers)
        )
    
    def generate_combinations(self) -> Iterator[ParameterSet]:
        """Generate all parameter combinations"""
        for base_score in self.base_scores:
            for trend_strength in self.trend_strengths:
                for sl_mult in self.sl_multipliers:
                    for tp_mult in self.tp_multipliers:
                        yield ParameterSet(
                            base_score=base_score,
                            trend_strength=trend_strength,
                            sl_multiplier=sl_mult,
                            tp_multiplier=tp_mult,
                        )

