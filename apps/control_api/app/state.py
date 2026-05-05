"""State module.

Author: Thomas Klute"""

from __future__ import annotations

from dataclasses import dataclass, field

from .models import FeatureState, RuntimeStatus


@dataclass
class RuntimeRegistry:
    node_status: RuntimeStatus = RuntimeStatus.idle
    feature_states: dict[str, FeatureState] = field(
        default_factory=lambda: {
            "detection": FeatureState.enabled,
            "online_streaming": FeatureState.disabled,
            "recording": FeatureState.enabled,
        }
    )

    def start_feature(self, name: str) -> None:
        self.feature_states[name] = FeatureState.running

    def stop_feature(self, name: str) -> None:
        self.feature_states[name] = FeatureState.disabled
