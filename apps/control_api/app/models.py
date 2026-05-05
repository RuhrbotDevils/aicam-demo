"""Models module.

Author: Thomas Klute"""

from __future__ import annotations

from enum import Enum
from typing import Any

from pydantic import BaseModel, Field


class NodeRole(str, Enum):
    standalone = "standalone"
    master = "master"
    slave = "slave"


class RuntimeStatus(str, Enum):
    idle = "idle"
    starting = "starting"
    running = "running"
    degraded = "degraded"
    stopping = "stopping"
    error = "error"


class FeatureState(str, Enum):
    """Feature lifecycle state (doc 03)."""

    disabled = "disabled"
    enabled = "enabled"
    starting = "starting"
    running = "running"
    stopping = "stopping"
    error = "error"


class ServiceStatus(str, Enum):
    """Per-service status reported in health checks."""

    running = "running"
    stub = "stub"
    stopped = "stopped"
    error = "error"


class FeatureFlag(BaseModel):
    detection: bool = True
    online_streaming: bool = True
    recording: bool = True
    cpu_detection: bool = True


class CameraPositionConfig(BaseModel):
    """Estimated camera position in field coordinates (meters).

    Origin is the center mark (0, 0, 0).
    x: along the long side (positive = right when facing from camera side)
    y: perpendicular to camera side (negative = camera side)
    z: height above ground
    """

    x: float = 0.0
    y: float = -4.0
    z: float = 2.0


class NodeConfig(BaseModel):
    id: str = "cam-01"
    role: NodeRole = NodeRole.standalone
    camera_position: CameraPositionConfig = Field(default_factory=CameraPositionConfig)


class CameraNetworkConfig(BaseModel):
    interface: str = "eth0"
    address: str = "192.168.50.10"
    netmask: str = "255.255.255.0"


class WifiProfile(BaseModel):
    name: str
    ssid: str
    password: str
    address: str
    netmask: str


class FieldWifiConfig(BaseModel):
    interface: str = "wlan0"
    listen_only: bool = True
    profiles: list[WifiProfile] = Field(default_factory=list)


class NetworkConfig(BaseModel):
    camera_network: CameraNetworkConfig = Field(default_factory=CameraNetworkConfig)
    field_wifi: FieldWifiConfig = Field(default_factory=FieldWifiConfig)


class CameraConfig(BaseModel):
    width: int = 1920
    height: int = 1080
    fps: int = 30


class RecordingMode(str, Enum):
    manual = "manual"
    automatic = "automatic"


class RecordingConfig(BaseModel):
    directory: str = "recordings/"
    video_codec: str = "h264"
    audio_codec: str = "flac"
    audio_enabled: bool = True
    recording_mode: RecordingMode = RecordingMode.manual


class StreamingConfig(BaseModel):
    enabled: bool = False
    platform: str = "youtube"
    # YouTube and most RTMP servers split the destination into a base
    # URL ("rtmp://a.rtmp.youtube.com/live2/") and a per-stream key
    # ("xxxx-xxxx-xxxx-xxxx"). The media service streams to
    # `<rtmp_url stripped of trailing slash>/<stream_key>`.
    rtmp_url: str = ""
    stream_key: str = ""
    # Default tuned for 720p15 streaming output (the streaming consumer
    # downscales internally to 1280x720 @ 15 fps before encoding).
    # YouTube's recommended range for 720p30 is 2500-4000 kbps - for
    # 720p15 the lower end is plenty and avoids YouTube's "stream
    # bitrate higher than recommended" warning out of the box. Users
    # can bump this in the Config page if their upstream link allows.
    bitrate_kbps: int = 2500
    # Grace period before the media service's streaming
    # flow-check concludes no buffers reached rtmpsink. Default 10 s
    # covers libcamera + ALSA + flvmux warmup on Pi 5 at 1080p with
    # audio enabled; the dev-container smoke harness overrides via
    # the SMOKE_GRACE_S env var.
    flow_check_grace_s: int = 10


class VideoConfig(BaseModel):
    source: str = "live"
    camera: CameraConfig = Field(default_factory=CameraConfig)
    recording: RecordingConfig = Field(default_factory=RecordingConfig)
    streaming: StreamingConfig = Field(default_factory=StreamingConfig)


class AudioConfig(BaseModel):
    device: str = "default"
    agc: bool = True
    volume: float = 1.0


class AIConfig(BaseModel):
    """AI model selection.

    Models are defined as sidecar JSON files in ``config/models/``.
    Selection is by display_name; there is no flat HEF-path block.
    See ``apps/model_registry.py`` for the full registry schema and loader behaviour.
    """

    accelerator: str = "hailo"
    # display_name of the selected object_detection model, or None to
    # disable the AI branch.
    object_detection_model: str | None = None
    # CPU object detection model (PyTorch). Selected independently from
    # the Hailo model. Only active when features.cpu_detection is True.
    cpu_object_detection_model: str | None = None


class AppConfig(BaseModel):
    node: NodeConfig = Field(default_factory=NodeConfig)
    network: NetworkConfig = Field(default_factory=NetworkConfig)
    features: FeatureFlag = Field(default_factory=FeatureFlag)
    video: VideoConfig = Field(default_factory=VideoConfig)
    audio: AudioConfig = Field(default_factory=AudioConfig)
    ai: AIConfig = Field(default_factory=AIConfig)


class HealthResponse(BaseModel):
    status: RuntimeStatus
    services: dict[str, ServiceStatus]


class StatusResponse(BaseModel):
    status: RuntimeStatus
    features: dict[str, FeatureState]
    detail: dict[str, Any] = Field(default_factory=dict)
