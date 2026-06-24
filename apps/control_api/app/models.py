"""Models module.

Author: Thomas Klute"""

from __future__ import annotations

from enum import Enum
from typing import Any, Literal

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
    """Feature lifecycle state."""

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
    # All features default to enabled in the demo build. The flags
    # remain in config.yaml so an operator can disable a feature on a
    # specific node without code changes, but the UI no longer exposes
    # them - toggling features at runtime from the dashboard was a
    # leftover from earlier iterations and the actual lifecycle is
    # driven by /recording/start, /streaming/start, etc.
    detection: bool = True
    online_streaming: bool = True
    recording: bool = True
    cpu_detection: bool = True  # show CPU detection model panel + enable CPU detector service


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


class FirewallConfig(BaseModel):
    # Comma-separated CIDR allowlist for inbound TCP 22 (SSH) and
    # TCP 8000 (control_api web UI). "*" (the default) means "allow
    # from anywhere". Operator sets a real list (e.g.
    # "192.168.3.0/24, 198.51.100.250/32, 10.10.0.0/16") when the
    # camera may receive a public IP at a venue and the web UI / SSH
    # must not be reachable from the open internet. All other inbound
    # ports are dropped regardless of source (loopback,
    # established/related, ICMP echo, and the DHCP client port are
    # always allowed by the apply script). Whitespace around entries
    # is tolerated; invalid entries are logged and dropped; if no
    # entry parses, the apply script falls back to "*" so the
    # operator doesn't lock themselves out of a misconfigured box.
    allowed_ip_ranges: str = "*"


class NetworkConfig(BaseModel):
    camera_network: CameraNetworkConfig = Field(default_factory=CameraNetworkConfig)
    field_wifi: FieldWifiConfig = Field(default_factory=FieldWifiConfig)
    firewall: FirewallConfig = Field(default_factory=FirewallConfig)


class CameraConfig(BaseModel):
    width: int = 1920
    height: int = 1080
    fps: int = 30
    # Horizontally mirror frames at the producer (before
    # intervideosink), so every consumer - recording, streaming,
    # frame_export - gets the corrected image without per-consumer
    # plumbing. Operator-set when the physical camera mount inverts
    # left/right. Default false adds zero pipeline cost (no extra
    # element); true inserts a videoflip on Pi / sets flip-method=4
    # on the existing nvvidconv on Jetson (VIC-accelerated). Change
    # takes effect on the next media service restart.
    flip_horizontal: bool = False
    # Rotate the producer frames 180° (top↔bottom + left↔right).
    # Operator-set when the camera is physically mounted upside down.
    # Composes with `flip_horizontal`: rotate_180=true alone gives a
    # 180° rotation; combined with flip_horizontal=true the producer
    # emits frames that are rotated 180° AND then mirrored L↔R -
    # equivalent to a vertical-only flip. Tegra VIC (`nvvidconv`) does
    # the combined op in a single pass via the corresponding
    # `flip-method` enum value; the CPU `videoflip` path likewise
    # has all four combinations as single enum values, so no
    # extra element is inserted regardless.
    rotate_180: bool = False


class RecordingMode(str, Enum):
    manual = "manual"
    automatic = "automatic"


class RecordingConfig(BaseModel):
    directory: str = "recordings/"
    video_codec: str = "h264"
    audio_codec: str = "flac"
    audio_enabled: bool = True
    recording_mode: RecordingMode = RecordingMode.manual
    # H.264 quantization parameter for the Jetson recording encoder
    # (`nvv4l2h264enc`). Used only when the platform's encoder is
    # `nvv4l2_h264`; the Pi (`x264enc`) ignores this and continues to
    # use CBR at `video.streaming.bitrate_kbps` because x264enc pads
    # bits to fill the CBR target correctly. NVENC on JetPack 4.6 does
    # NOT - it emits "just enough" bits to maintain its internal
    # quality target under CBR, which under low-motion content
    # collapses to 200-300 kbps and produces the block-artifact
    # "upscaled" look. Switching to CQP (constant-quantization) mode
    # and pinning the QP guarantees consistent quality regardless of
    # motion.
    #
    # Sensible values:
    #   - 22-24: near-lossless; ~100+ Mbps recordings (archival)
    #   - 26-28: broadcast grade; ~15-30 Mbps
    #   - 30: matches Pi's ~8 Mbps target (default)
    #   - 32+: lower quality but smaller files
    # Higher number → smaller files, lower quality. Each +6 ≈
    # half the bitrate.
    encoder_quality_qp: int = 30


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
    # audio enabled.
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


class TelemetryConfig(BaseModel):
    game_controller_port: int = 3838
    # Synthetic GameController test source. When true, control_api
    # spawns a background thread that publishes random GC messages on
    # `telemetry.game_state` + `telemetry.penalties` at 1 Hz, so the
    # broadcast overlay renders without a real GC on the wire.
    # Intended for demos and overlay-parity validation; not a
    # replacement for a live GC feed (no game-state machine).
    gc_test_mode: bool = False


class DeploymentConfig(BaseModel):
    """Per-host hardware platform selection.

    Set by the deployment script; never edited via the GUI, never
    mutated at runtime. Defaults preserve the existing Pi behaviour
    so configs without this section keep working.

    Fields:

    - ``platform``: ``"pi"`` (Raspberry Pi 5 + Hailo, the existing
      target) or ``"jetson"`` (Original Jetson Nano + IMX477). On
      ``"jetson"`` the AI features are force-disabled by
      :func:`normalize_for_platform` because the Jetson role is
      recording + streaming only.
    - ``camera_backend``: which GStreamer source element the media
      service uses - ``"libcamera"`` on Pi (``libcamerasrc``),
      ``"nvargus"`` on Jetson (``nvarguscamerasrc``), or ``"v4l2"``
      as a generic USB fallback (``v4l2src``).
    - ``video_encoder``: H.264 encoder choice - ``"x264"`` (software,
      current default) or ``"nvv4l2_h264"`` (Jetson hardware NVENC).

    These fields are consumed by the Rust media service via direct
    YAML reads (see ``MediaDeploymentConfig`` in
    ``apps/media_service/src/main.rs``).
    """

    platform: Literal["pi", "jetson"] = "pi"
    camera_backend: Literal["libcamera", "nvargus", "v4l2"] = "libcamera"
    video_encoder: Literal["x264", "nvv4l2_h264"] = "x264"


class AppConfig(BaseModel):
    node: NodeConfig = Field(default_factory=NodeConfig)
    network: NetworkConfig = Field(default_factory=NetworkConfig)
    features: FeatureFlag = Field(default_factory=FeatureFlag)
    video: VideoConfig = Field(default_factory=VideoConfig)
    audio: AudioConfig = Field(default_factory=AudioConfig)
    ai: AIConfig = Field(default_factory=AIConfig)
    telemetry: TelemetryConfig = Field(default_factory=TelemetryConfig)
    # deployment.platform=="jetson" disables AI features (recording +
    # streaming only role). See normalize_for_platform. Default is
    # Pi-shaped so configs without this section keep working.
    deployment: DeploymentConfig = Field(default_factory=DeploymentConfig)


def normalize_for_platform(cfg: AppConfig) -> AppConfig:
    """Enforce platform-specific feature gating.

    On Jetson the role is recording + streaming only - there is no
    Hailo accelerator and CPU detection on a 4 GB Nano isn't
    practical, so the AI feature flags and ``ai.*_model`` slots are
    forced off regardless of what the config file carries. A Pi
    config copied verbatim to a Jetson (e.g. by an operator
    inheriting team-level defaults) still produces a sane runtime
    instead of a service crash-loop trying to load a Hailo HEF that
    isn't there.

    Caller contract: invoke this once at config-load time, after
    Pydantic validation. The returned :class:`AppConfig` is the
    runtime-effective view; the raw on-disk file is left unchanged.

    On Pi this is a no-op so existing deployments are unaffected.
    """

    if cfg.deployment.platform != "jetson":
        return cfg

    cfg.features.detection = False
    cfg.features.cpu_detection = False
    cfg.ai.object_detection_model = None
    cfg.ai.cpu_object_detection_model = None
    return cfg


class HealthResponse(BaseModel):
    status: RuntimeStatus
    services: dict[str, ServiceStatus]
    # Human-readable list of concrete failures observed at the
    # moment of the probe. Empty when everything is green; populated when
    # `/api/v1/health` would otherwise have to report "running" for a
    # service that is, in fact, broken (no camera detected, live pipeline
    # in error state, etc.).
    issues: list[str] = Field(default_factory=list)


class StatusResponse(BaseModel):
    status: RuntimeStatus
    features: dict[str, FeatureState]
    detail: dict[str, Any] = Field(default_factory=dict)
