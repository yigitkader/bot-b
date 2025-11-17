"""
Configuration file management for optimization
"""

import shutil
from pathlib import Path
from typing import Optional
from .parameter_space import ParameterSet


class ConfigManager:
    """Manages config.yaml modifications during optimization"""
    
    def __init__(self, config_path: str = 'config.yaml'):
        self.config_path = Path(config_path)
        self.backup_path = Path(f'{config_path}.bak')
        self._backed_up = False
    
    def backup(self) -> None:
        """Create backup of original config"""
        if not self._backed_up and self.config_path.exists():
            shutil.copy(self.config_path, self.backup_path)
            self._backed_up = True
    
    def restore(self) -> None:
        """Restore original config from backup"""
        if self.backup_path.exists():
            shutil.copy(self.backup_path, self.config_path)
    
    def apply_parameters(self, params: ParameterSet) -> None:
        """Apply parameter set to config.yaml"""
        if not self.config_path.exists():
            raise FileNotFoundError(f"Config file not found: {self.config_path}")
        
        with open(self.config_path, 'r') as f:
            lines = f.readlines()
        
        modified_keys = set()
        
        for i, line in enumerate(lines):
            stripped = line.strip()
            
            if stripped.startswith('base_min_score:'):
                indent = len(line) - len(line.lstrip())
                lines[i] = f"{' ' * indent}base_min_score: {params.base_score}\n"
                modified_keys.add('base_min_score')
            
            elif stripped.startswith('trend_threshold_hft:'):
                indent = len(line) - len(line.lstrip())
                lines[i] = f"{' ' * indent}trend_threshold_hft: {params.trend_strength}\n"
                modified_keys.add('trend_threshold_hft')
            
            elif stripped.startswith('trend_threshold_normal:'):
                indent = len(line) - len(line.lstrip())
                lines[i] = f"{' ' * indent}trend_threshold_normal: {params.trend_strength}\n"
                modified_keys.add('trend_threshold_normal')
            
            elif stripped.startswith('atr_sl_multiplier:'):
                indent = len(line) - len(line.lstrip())
                lines[i] = f"{' ' * indent}atr_sl_multiplier: {params.sl_multiplier}\n"
                modified_keys.add('atr_sl_multiplier')
            
            elif stripped.startswith('atr_tp_multiplier:'):
                indent = len(line) - len(line.lstrip())
                lines[i] = f"{' ' * indent}atr_tp_multiplier: {params.tp_multiplier}\n"
                modified_keys.add('atr_tp_multiplier')
        
        if len(modified_keys) < 5:
            self._add_missing_keys(lines, params)
        
        with open(self.config_path, 'w') as f:
            f.writelines(lines)
    
    def _add_missing_keys(self, lines: list, params: ParameterSet) -> None:
        """Add missing parameter keys to trending section"""
        in_trending = False
        trending_indent = 0
        insert_pos = None
        
        for i, line in enumerate(lines):
            if line.strip().startswith('trending:'):
                in_trending = True
                trending_indent = len(line) - len(line.lstrip())
            elif in_trending:
                current_indent = len(line) - len(line.lstrip()) if line.strip() else 0
                if line.strip() and current_indent <= trending_indent and not line.strip().startswith('#'):
                    insert_pos = i
                    break
        
        if insert_pos is None:
            insert_pos = len(lines)
        
        indent = ' ' * (trending_indent + 2)
        new_lines = [
            f"{indent}base_min_score: {params.base_score}\n",
            f"{indent}trend_threshold_hft: {params.trend_strength}\n",
            f"{indent}trend_threshold_normal: {params.trend_strength}\n",
            f"{indent}atr_sl_multiplier: {params.sl_multiplier}\n",
            f"{indent}atr_tp_multiplier: {params.tp_multiplier}\n"
        ]
        lines[insert_pos:insert_pos] = new_lines

