"""XPUB/XSUB broker - central proxy that connects publishers to subscribers.

Can run standalone (scripts/run_bus.sh) or in-process as a background thread.

Author: Thomas Klute"""

from __future__ import annotations

import threading

import zmq

# Default broker endpoints - publishers connect to XSUB, subscribers to XPUB.
DEFAULT_XSUB_ENDPOINT = "tcp://127.0.0.1:5559"
DEFAULT_XPUB_ENDPOINT = "tcp://127.0.0.1:5560"


class Broker:
    """ZeroMQ XPUB/XSUB proxy.

    Publishers connect to ``xsub_endpoint`` and subscribers to ``xpub_endpoint``.
    The broker forwards messages between them with topic-based filtering.
    """

    def __init__(
        self,
        xsub_endpoint: str = DEFAULT_XSUB_ENDPOINT,
        xpub_endpoint: str = DEFAULT_XPUB_ENDPOINT,
    ):
        self.xsub_endpoint = xsub_endpoint
        self.xpub_endpoint = xpub_endpoint
        self._ctx: zmq.Context | None = None  # type: ignore[type-arg]
        self._thread: threading.Thread | None = None

    def run_blocking(self) -> None:
        """Run the proxy in the current thread (blocks forever)."""
        ctx = zmq.Context()
        try:
            xsub = ctx.socket(zmq.XSUB)
            xsub.setsockopt(zmq.RCVHWM, 100)
            xsub.bind(self.xsub_endpoint)

            xpub = ctx.socket(zmq.XPUB)
            xpub.setsockopt(zmq.SNDHWM, 100)
            xpub.bind(self.xpub_endpoint)

            zmq.proxy(xsub, xpub)
        except zmq.ContextTerminated:
            pass
        finally:
            ctx.term()

    def start_background(self) -> None:
        """Start the proxy in a daemon thread."""
        if self._thread is not None:
            return
        self._ctx = zmq.Context()

        def _run() -> None:
            assert self._ctx is not None
            try:
                xsub = self._ctx.socket(zmq.XSUB)
                xsub.setsockopt(zmq.RCVHWM, 100)
                xsub.bind(self.xsub_endpoint)

                xpub = self._ctx.socket(zmq.XPUB)
                xpub.setsockopt(zmq.SNDHWM, 100)
                xpub.bind(self.xpub_endpoint)

                zmq.proxy(xsub, xpub)
            except zmq.ContextTerminated:
                pass

        self._thread = threading.Thread(target=_run, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Stop the background broker by terminating its ZMQ context."""
        if self._ctx is not None:
            self._ctx.term()
            self._ctx = None
        if self._thread is not None:
            self._thread.join(timeout=2)
            self._thread = None
