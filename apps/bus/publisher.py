"""ZeroMQ publisher - sends typed messages to named topics.

Author: Thomas Klute"""

from __future__ import annotations

import zmq
from pydantic import BaseModel

from apps.bus.broker import DEFAULT_XSUB_ENDPOINT


class Publisher:
    """Publishes Pydantic messages to the ZMQ bus.

    Messages are sent as two-part ZMQ frames: ``[topic, json_payload]``.
    The topic is a UTF-8 string matching the doc 08 topic layout
    (e.g. ``media.frame_refs``, ``ai.object_detections``).
    """

    def __init__(self, endpoint: str = DEFAULT_XSUB_ENDPOINT):
        self._endpoint = endpoint
        self._ctx = zmq.Context()
        self._socket = self._ctx.socket(zmq.PUB)
        self._socket.setsockopt(zmq.SNDHWM, 100)  # drop oldest if subscriber lags
        self._socket.connect(self._endpoint)

    def send(self, topic: str, message: BaseModel) -> None:
        """Publish a message to the given topic."""
        payload = message.model_dump_json(by_alias=True)
        self._socket.send_multipart([topic.encode(), payload.encode()])

    def close(self) -> None:
        """Close the publisher socket."""
        self._socket.close()
        self._ctx.term()
