"""Load + save ``config.yaml``.

Reads the file via PyYAML, validates against the ``AppConfig`` Pydantic
model, and writes back via ``yaml.safe_dump``. Unknown keys are dropped
by the model's ``extra`` config (see ``models.py``) so a stale
``config.yaml`` does not crash startup.

Author: Thomas Klute"""

from __future__ import annotations

from pathlib import Path

import yaml

from .models import AppConfig


class ConfigStore:
    def __init__(self, path: str | Path):
        self.path = Path(path)

    def load(self) -> AppConfig:
        if not self.path.exists():
            return AppConfig()
        data = yaml.safe_load(self.path.read_text()) or {}
        return AppConfig.model_validate(data)

    def save(self, config: AppConfig) -> None:
        self.path.write_text(yaml.safe_dump(config.model_dump(mode="json"), sort_keys=False))
