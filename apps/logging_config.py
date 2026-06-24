"""Shared structured logging configuration for all Python services.

Usage:
    from apps.logging_config import get_logger

    logger = get_logger("my_service")
    logger.info("started", port=8000, session_id="abc")

Output (JSON to stdout):
    {"timestamp": "2026-04-06T...", "level": "info", "service": "my_service",
     "event": "started", "port": 8000, "session_id": "abc"}

For services that still use stdlib ``logging.getLogger(__name__)`` - most
notably the FastAPI control API, which is launched under uvicorn and
therefore inherits uvicorn's access/error logger config but nothing for
``apps.*`` loggers - call :func:`configure_stdlib_logging` at module
import time so those loggers actually emit to stdout.

Author: Thomas Klute"""

from __future__ import annotations

import logging
import os
import sys

import structlog

_STDLIB_HANDLER_FLAG = "_aicam_apps_stdlib_handler"


class _LiveStdout:
    """File-like wrapper that resolves ``sys.stdout`` on every write.

    Background daemon threads (e.g. ``ReplayFrameSource.run``) that
    emit log lines can outlive a pytest stdout-capture window. The
    structlog ``PrintLogger`` binds its file at construction, and
    ``cache_logger_on_first_use=True`` keeps that binding for the
    process lifetime - so a logger created during test A keeps
    writing to test A's captured stream long after pytest has closed
    it, surfacing as ``ValueError: I/O operation on closed file``.
    Forwarding through the live ``sys.stdout`` avoids that.
    """

    def write(self, s: str) -> int:
        return sys.stdout.write(s)

    def flush(self) -> None:
        sys.stdout.flush()


_LIVE_STDOUT = _LiveStdout()


def configure_logging(level: str = "INFO") -> None:
    """Configure structlog for JSON output to stdout.

    Call once at service startup. Safe to call multiple times.
    """
    structlog.configure(
        processors=[
            structlog.contextvars.merge_contextvars,
            structlog.processors.add_log_level,
            structlog.processors.TimeStamper(fmt="iso"),
            structlog.processors.StackInfoRenderer(),
            structlog.processors.format_exc_info,
            structlog.processors.JSONRenderer(),
        ],
        wrapper_class=structlog.make_filtering_bound_logger(
            getattr(logging, level.upper(), logging.INFO)
        ),
        context_class=dict,
        logger_factory=structlog.PrintLoggerFactory(file=_LIVE_STDOUT),  # type: ignore[arg-type]
        cache_logger_on_first_use=True,
    )


def get_logger(service: str) -> structlog.stdlib.BoundLogger:
    """Return a logger bound with the service name."""
    configure_logging()
    log: structlog.stdlib.BoundLogger = structlog.get_logger(service=service)
    return log


def configure_stdlib_logging(
    logger_name: str = "apps",
    level: str | None = None,
    stream=None,
) -> logging.Logger:
    """Attach a stdout ``StreamHandler`` to the ``apps`` logger tree.

    This is the escape hatch for code that still uses
    ``logging.getLogger(__name__)`` rather than structlog - notably the
    FastAPI control API, which runs under uvicorn. Without this call,
    uvicorn configures its own ``uvicorn.*`` loggers but leaves the
    ``apps.*`` tree with no handler, so every ``logger.info(...)`` call
    is silently dropped.

    The handler is attached to ``apps`` (not root) so that uvicorn's
    own access/error logs are not double-routed. ``propagate`` is
    disabled on the ``apps`` logger for the same reason.

    Idempotent: subsequent calls return the existing logger unchanged.

    Args:
        logger_name: Parent logger to attach the handler to. Default
            ``"apps"`` matches the package layout.
        level: Logging level name; defaults to ``$AICAM_LOG_LEVEL`` or
            ``"INFO"``.
        stream: File-like object to write to. Default is
            ``sys.stdout`` (looked up at call time so tests can
            redirect).
    """
    target = logging.getLogger(logger_name)
    if getattr(target, _STDLIB_HANDLER_FLAG, False):
        return target

    resolved_level = (level or os.environ.get("AICAM_LOG_LEVEL") or "INFO").upper()
    numeric_level = getattr(logging, resolved_level, logging.INFO)

    handler = logging.StreamHandler(stream if stream is not None else sys.stdout)
    handler.setFormatter(
        logging.Formatter(
            fmt="%(asctime)s %(levelname)s %(name)s: %(message)s",
            datefmt="%Y-%m-%dT%H:%M:%S%z",
        )
    )
    handler.setLevel(numeric_level)

    target.addHandler(handler)
    target.setLevel(numeric_level)
    # Leave ``propagate`` at its default (True). Nothing inside the
    # control_api process attaches a handler to the root logger
    # (uvicorn only configures its own ``uvicorn.*`` loggers), so
    # propagation does not cause duplicates in practice - and keeping
    # it lets pytest's ``caplog`` fixture capture our records.

    setattr(target, _STDLIB_HANDLER_FLAG, True)
    return target
