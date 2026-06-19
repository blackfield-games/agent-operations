"""POST emitted render jobs to the mesh coordinator — the agents -> mesh transport.

``render_jobs`` (common/jobs.py) produces coordinator-valid ``RenderJobSpec``
payloads; this ships them to the coordinator's ``POST /jobs`` (see
mesh/coordinator/src/main.rs ``create_job``). The HTTP client is injected so the
transport is fully offline-testable — the CLI passes a real ``httpx.Client``, the
tests pass a scripted mock. The ingest token is threaded into the
``Authorization: Bearer`` header and is NEVER logged or stored in a result.

Coordinator response contract (``create_job`` + ``ingest_authorized``):
* 201 -> ``{"id": "<uuid>"}`` (enqueued, id assigned)
* 401 -> missing/invalid bearer token (only when the coordinator is tokened)
* 422 -> malformed spec rejected by ``validate_job_spec`` (a producer bug)
* 503 -> queue at capacity (retryable)
* other 5xx -> transient coordinator error
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from enum import Enum
from typing import Any, Protocol

import httpx

from common.jobs import RenderJobSpec

logger = logging.getLogger(__name__)

DEFAULT_TIMEOUT_SECS = 10.0


class PostOutcome(str, Enum):
    CREATED = "created"              # 201 — enqueued, id assigned
    UNAUTHORIZED = "unauthorized"    # 401 — bad/missing ingest token (config error)
    REJECTED = "rejected"            # 422 — malformed spec (producer bug; do not retry)
    UNAVAILABLE = "unavailable"      # 503 — queue at capacity (retryable)
    NETWORK_ERROR = "network_error"  # transport failure (retryable)
    ERROR = "error"                  # other unexpected status (e.g. 500)


@dataclass(frozen=True)
class PostResult:
    """Outcome of POSTing one render job. Holds no token (never leaked)."""

    region_id: str
    kind: str
    outcome: PostOutcome
    status_code: int | None = None
    job_id: str | None = None  # set only on CREATED
    detail: str | None = None

    @property
    def ok(self) -> bool:
        return self.outcome is PostOutcome.CREATED


class HttpResponse(Protocol):
    status_code: int

    def json(self) -> Any: ...


class HttpClient(Protocol):
    """The slice of ``httpx.Client`` post_jobs needs; the test mock matches it."""

    def post(
        self, url: str, *, json: Any, headers: dict[str, str], timeout: float
    ) -> HttpResponse: ...


def _classify(status_code: int) -> PostOutcome:
    return {
        201: PostOutcome.CREATED,
        401: PostOutcome.UNAUTHORIZED,
        422: PostOutcome.REJECTED,
        503: PostOutcome.UNAVAILABLE,
    }.get(status_code, PostOutcome.ERROR)


def _job_id(resp: HttpResponse) -> str | None:
    try:
        body = resp.json()
    except ValueError:
        return None
    if isinstance(body, dict) and body.get("id") is not None:
        return str(body["id"])
    return None


def _log_terminal(result: PostResult) -> None:
    """Loud, actionable logging for the non-retryable failure outcomes (FM2)."""
    if result.outcome is PostOutcome.REJECTED:
        logger.error(
            "POST /jobs rejected %s (%s) as a malformed spec (422) — a producer bug, "
            "not retrying; check RenderJobSpec against validate_job_spec.",
            result.region_id, result.kind,
        )
    elif result.outcome is PostOutcome.UNAUTHORIZED:
        logger.error(
            "POST /jobs unauthorized (401) for %s — check the ingest token "
            "(COORDINATOR_INGEST_TOKEN); the coordinator requires a valid bearer.",
            result.region_id,
        )
    elif result.outcome is PostOutcome.ERROR:
        logger.error(
            "POST /jobs for %s returned unexpected status %s",
            result.region_id, result.status_code,
        )


def post_jobs(
    jobs: list[RenderJobSpec],
    *,
    base_url: str,
    token: str | None,
    client: HttpClient,
    timeout: float = DEFAULT_TIMEOUT_SECS,
) -> list[PostResult]:
    """POST each job to ``{base_url}/jobs``; one result per job, in order.

    Never raises on an HTTP status or transport error — the region is already
    authored and composed, so a posting failure is reported, not fatal (FM3). The
    bearer token (when set) authenticates the request and is never echoed back.
    """
    url = f"{base_url.rstrip('/')}/jobs"
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"

    def attempt(job: RenderJobSpec) -> PostResult:
        region_id = job.region.region_id
        kind = job.kind.wire_name
        try:
            resp = client.post(url, json=job.to_request(), headers=headers, timeout=timeout)
        except httpx.RequestError as e:
            return PostResult(region_id, kind, PostOutcome.NETWORK_ERROR, detail=type(e).__name__)
        outcome = _classify(resp.status_code)
        if outcome is PostOutcome.CREATED:
            return PostResult(region_id, kind, outcome, status_code=201, job_id=_job_id(resp))
        result = PostResult(region_id, kind, outcome, status_code=resp.status_code)
        _log_terminal(result)
        return result

    return [attempt(job) for job in jobs]
