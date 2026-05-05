"""Standard message envelope - required fields for every inter-module message.

Author: Thomas Klute"""

from __future__ import annotations

from datetime import datetime

from pydantic import BaseModel, field_validator

SUPPORTED_SCHEMA_VERSIONS = {"1.0"}


class MessageEnvelope(BaseModel):
    """Base envelope embedded in every inter-module message (doc 08, Rule 1)."""

    schema_version: str = "1.0"
    message_type: str
    message_id: str
    session_id: str
    source_module: str
    created_at: datetime

    @field_validator("schema_version")
    @classmethod
    def validate_schema_version(cls, v: str) -> str:
        if v not in SUPPORTED_SCHEMA_VERSIONS:
            raise ValueError(
                f"Unsupported schema_version '{v}'. Supported: {SUPPORTED_SCHEMA_VERSIONS}"
            )
        return v

    @field_validator("session_id")
    @classmethod
    def session_id_non_empty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("session_id must not be empty")
        return v

    model_config = {"extra": "forbid"}
