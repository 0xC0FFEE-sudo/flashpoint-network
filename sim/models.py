from __future__ import annotations

from typing import List, Optional
from pydantic import BaseModel, Field


class Intent(BaseModel):
    """
    An intent represents a trader's desired execution bundle in simplified form.

    - profit: expected profit if executed successfully (arbitrary units)
    - resources: abstract resources required (e.g., pairs, venues). Conflicts
      arise when two intents require any overlapping resource.
    - client_id: optional client-provided identifier
    - arrival_index: sequence number assigned when intent arrives (for FIFO)
    """

    profit: float
    resources: List[str] = Field(default_factory=list)
    client_id: Optional[str] = None
    arrival_index: Optional[int] = None


def conflicts(a: Intent, b: Intent) -> bool:
    """Return True if intents a and b conflict (share any resource)."""
    return bool(set(a.resources) & set(b.resources))
