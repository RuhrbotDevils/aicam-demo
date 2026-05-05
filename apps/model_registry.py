"""Unified AI model definitions loader.

Reads sidecar JSON files from ``config/models/`` and returns a list of
``ModelDef`` objects.

Schema:
    display_name       str      (required, unique per scope)
    scope              enum     (required: object_detection)
    active             bool     (required; false hides the model)
    input.width        int      (required)
    input.height       int      (required)
    input.format       str      (required, e.g. "RGB")
    hef_path           str      (required, absolute path)
    postprocess.so_path          str (required)
    postprocess.function_name    str (required)
    postprocess.output_format    str (required, see KNOWN_OUTPUT_FORMATS)
    labels             str      (optional)
    notes              str      (optional)

Loader behaviour:
    - Skips files that fail JSON/schema validation (warning logged).
    - Skips models with ``active=false``.
    - Skips models whose ``hef_path`` does not exist on disk
      (warning logged once per missing file per process).
    - Rejects duplicate ``display_name`` within a scope - both colliding
      files are dropped with an error log.
    - Returns the remaining list sorted by display_name.

No caching - the registry is rescanned on each call. File count is tiny
and this keeps the UI honest about on-disk state.

Author: Thomas Klute"""

from __future__ import annotations

import json
import logging
from enum import Enum
from pathlib import Path

from pydantic import BaseModel, ConfigDict, Field

logger = logging.getLogger(__name__)

DEFAULT_MODELS_DIR = "config/models"


class ModelScope(str, Enum):
    """Which part of the AI branch a model feeds."""

    object_detection = "object_detection"


_TAPPAS_YOLO_SO = "/usr/lib/aarch64-linux-gnu/hailo/tappas/post_processes/libyolo_hailortpp_post.so"
_YOLO26_SO = "/opt/robocup-ai-camera/apps/hailo_postprocess/libyolo26_post.so"

FAMILY_DEFAULTS: dict[str, dict[str, str]] = {
    "yolov5": {"so_path": _TAPPAS_YOLO_SO, "function_name": "filter_letterbox"},
    "yolov8": {"so_path": _TAPPAS_YOLO_SO, "function_name": "filter_letterbox"},
    "yolox": {"so_path": _TAPPAS_YOLO_SO, "function_name": "filter_letterbox"},
    "yolo26": {"so_path": _YOLO26_SO, "function_name": "yolo26"},
}

KNOWN_OUTPUT_FORMATS = set(FAMILY_DEFAULTS.keys()) | {"custom"}


class ModelInput(BaseModel):
    """Network input tensor specification."""

    model_config = ConfigDict(extra="forbid")

    width: int = Field(gt=0)
    height: int = Field(gt=0)
    format: str  # "RGB", "BGR", "NV12", ...


class ModelPostprocess(BaseModel):
    """hailofilter postprocess configuration for the model."""

    model_config = ConfigDict(extra="forbid")

    so_path: str = ""
    function_name: str = ""
    output_format: str = ""  # See KNOWN_OUTPUT_FORMATS


class ModelDef(BaseModel):
    """A single AI model definition loaded from a sidecar JSON file.

    Note: the source filename is NOT stored on this object. The UI and
    API identify models by ``display_name`` only. Exposing filenames to
    clients is a security concern (path-traversal attack surface via
    crafted selectors).
    """

    model_config = ConfigDict(extra="forbid", protected_namespaces=())

    display_name: str = Field(min_length=1)
    id: str | None = None
    scope: ModelScope
    active: bool
    input: ModelInput
    # Runtime: "hailo" (default, uses hef_path) or "pytorch" (uses model_path)
    runtime: str = "hailo"
    hef_path: str | None = None
    model_path: str | None = None
    postprocess: ModelPostprocess = Field(default_factory=ModelPostprocess)
    labels: str | list[str] | None = None
    class_map: dict[str, str] | None = None
    inference_fps: float | None = None
    notes: str | None = None
    publish_detections: bool = True


# Process-local sets to suppress repeated "file missing" warnings.
_missing_hef_warned: set[str] = set()
_missing_so_warned: set[str] = set()


def _reset_warning_state() -> None:
    """Test helper - clear the missing-file warning dedup sets."""
    _missing_hef_warned.clear()
    _missing_so_warned.clear()


def _apply_family_defaults(data: dict, filename: str) -> None:
    """Mutate the raw JSON dict to fill in so_path and
    function_name from ``FAMILY_DEFAULTS`` when they are absent.

    Runs BEFORE pydantic validation so ``ModelPostprocess`` can keep its
    strict ``str`` fields. No-op if the ``postprocess`` / ``output_format``
    keys are missing or malformed - pydantic will complain about those
    on its own with a clearer error.
    """
    pp = data.get("postprocess")
    if not isinstance(pp, dict):
        return
    fmt = pp.get("output_format")
    if not isinstance(fmt, str):
        return
    defaults = FAMILY_DEFAULTS.get(fmt)
    if defaults is None:
        # ``custom`` / ``centerpose`` have no defaults. If the JSON
        # omits so_path / function_name here, pydantic will reject it
        # below with its normal "field required" error.
        return
    for key, value in defaults.items():
        if pp.get(key) is None:
            pp[key] = value
            logger.debug(
                "ai_models: %s filled in postprocess.%s from family=%r default",
                filename,
                key,
                fmt,
            )


def _validate_one(path: Path) -> ModelDef | None:
    """Parse and validate one JSON file. Returns None on any failure."""
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        logger.warning("ai_models: failed to read %s: %s", path.name, e)
        return None
    # Fill in defaults from the family table BEFORE validating
    # so the ModelPostprocess fields can stay strict strings.
    if isinstance(data, dict):
        _apply_family_defaults(data, path.name)
    try:
        md = ModelDef.model_validate(data)
    except Exception as e:
        logger.warning("ai_models: schema validation failed for %s: %s", path.name, e)
        return None
    if md.postprocess.output_format and md.postprocess.output_format not in KNOWN_OUTPUT_FORMATS:
        logger.warning(
            "ai_models: %s uses unknown output_format=%r (known: %s)",
            path.name,
            md.postprocess.output_format,
            sorted(KNOWN_OUTPUT_FORMATS),
        )
    return md


def load_models(
    scope: ModelScope | None = None,
    directory: str | Path = DEFAULT_MODELS_DIR,
) -> list[ModelDef]:
    """Load model definitions from the registry directory.

    Args:
        scope: If given, only models matching this scope are returned.
        directory: Registry directory (defaults to ``config/models``).

    Returns:
        List of ``ModelDef`` sorted by display_name, filtered per the
        rules in the module docstring.
    """
    dir_path = Path(directory)
    if not dir_path.is_dir():
        logger.warning("ai_models: registry directory not found: %s", dir_path)
        return []

    # Load + validate every file first, then apply filters and dedup so
    # duplicate-name errors are deterministic regardless of file order.
    parsed: list[tuple[Path, ModelDef]] = []
    for json_file in sorted(dir_path.glob("*.json")):
        md = _validate_one(json_file)
        if md is not None:
            parsed.append((json_file, md))

    # Reject duplicate display_name within the same scope.
    seen: dict[tuple[ModelScope, str], Path] = {}
    duplicates: set[tuple[ModelScope, str]] = set()
    for src, md in parsed:
        key = (md.scope, md.display_name)
        if key in seen:
            logger.error(
                "ai_models: duplicate display_name=%r in scope=%s "
                "(files: %s, %s) - both files dropped",
                md.display_name,
                md.scope.value,
                seen[key].name,
                src.name,
            )
            duplicates.add(key)
        else:
            seen[key] = src

    results: list[ModelDef] = []
    for src, md in parsed:
        if (md.scope, md.display_name) in duplicates:
            continue
        if not md.active:
            continue
        if scope is not None and md.scope != scope:
            continue
        # Validate model file exists based on runtime
        if md.runtime == "hailo":
            if not md.hef_path or not Path(md.hef_path).exists():
                if md.hef_path and md.hef_path not in _missing_hef_warned:
                    logger.warning(
                        "ai_models: %s hef_path does not exist: %s (model hidden)",
                        src.name,
                        md.hef_path,
                    )
                    _missing_hef_warned.add(md.hef_path or "")
                continue
            # Hailo models must have postprocess configured
            if not md.postprocess.so_path or not md.postprocess.function_name:
                logger.warning("ai_models: %s hailo model missing postprocess config", src.name)
                continue
            if not Path(md.postprocess.so_path).exists():
                if md.postprocess.so_path not in _missing_so_warned:
                    logger.warning(
                        "ai_models: %s so_path does not exist: %s (model hidden)",
                        src.name,
                        md.postprocess.so_path,
                    )
                    _missing_so_warned.add(md.postprocess.so_path)
                continue
        elif md.runtime in ("pytorch", "onnx"):
            if not md.model_path or not Path(md.model_path).exists():
                if md.model_path and md.model_path not in _missing_hef_warned:
                    logger.warning(
                        "ai_models: %s model_path does not exist: %s (model hidden)",
                        src.name,
                        md.model_path,
                    )
                    _missing_hef_warned.add(md.model_path or "")
                continue
        results.append(md)

    results.sort(key=lambda m: m.display_name)
    logger.debug(
        "ai_models: loaded %d definitions from %s (scope=%s)",
        len(results),
        dir_path,
        scope.value if scope else "all",
    )
    return results


def load_model_by_display_name(
    display_name: str,
    scope: ModelScope | None = None,
    directory: str | Path = DEFAULT_MODELS_DIR,
) -> ModelDef | None:
    """Return the single model matching ``display_name`` (and ``scope``, if given).

    Returns None if no match - which includes inactive, missing hef,
    name collision, or absence.
    """
    for md in load_models(scope=scope, directory=directory):
        if md.display_name == display_name:
            return md
    return None
