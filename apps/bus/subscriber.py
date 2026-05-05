"""ZeroMQ subscriber - receives typed messages from named topics.

Author: Thomas Klute"""

from __future__ import annotations

from typing import TypeVar

import zmq

from apps.bus.broker import DEFAULT_XPUB_ENDPOINT
from apps.schemas.envelope import MessageEnvelope

T = TypeVar("T", bound=MessageEnvelope)


class Subscriber:
    """Subscribes to one or more topics on the ZMQ bus.

    Messages are received as two-part ZMQ frames: ``[topic, json_payload]``.
    Use ``receive()`` for raw (topic, json) pairs or ``receive_model()``
    to deserialize directly into a Pydantic model.
    """

    def __init__(
        self,
        topics: list[str],
        endpoint: str = DEFAULT_XPUB_ENDPOINT,
    ):
        self._endpoint = endpoint
        self._ctx = zmq.Context()
        self._socket = self._ctx.socket(zmq.SUB)
        self._socket.connect(self._endpoint)
        for topic in topics:
            self._socket.setsockopt_string(zmq.SUBSCRIBE, topic)

    def receive(self, timeout_ms: int | None = None) -> tuple[str, str] | None:
        """Receive the next message as (topic, json_payload).

        Returns ``None`` if timeout expires without a message.
        """
        if timeout_ms is not None:
            self._socket.setsockopt(zmq.RCVTIMEO, timeout_ms)
        else:
            self._socket.setsockopt(zmq.RCVTIMEO, -1)
        try:
            parts = self._socket.recv_multipart()
            topic = parts[0].decode()
            payload = parts[1].decode()
            return topic, payload
        except zmq.Again:
            return None

    def receive_model(
        self, model_class: type[T], timeout_ms: int | None = None
    ) -> tuple[str, T] | None:
        """Receive and deserialize into a Pydantic model.

        Returns ``None`` if timeout expires.
        """
        result = self.receive(timeout_ms=timeout_ms)
        if result is None:
            return None
        topic, payload = result
        return topic, model_class.model_validate_json(payload)

    def close(self) -> None:
        """Close the subscriber socket."""
        self._socket.close()
        self._ctx.term()
