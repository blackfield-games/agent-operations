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
import time
from collections.abc import Callable
from dataclasses import dataclass
from enum import Enum
from typing import Any, Protocol
from urllib.parse import urlparse

import httpx

from common.jobs import RenderJobSpec

logger = logging.getLogger(__name__)

DEFAULT_TIMEOUT_SECS = 10.0
DEFAULT_MAX_ATTEMPTS = 3
DEFAULT_BACKOFF_BASE_SECS = 0.5

# Cleartext-over-http warning is suppressed for loopback (local dev), where a
# bearer token never crosses a network an adversary can observe.
_LOOPBACK = frozenset({"localhost", "127.0.0.1", "::1"})


class PostOutcome(str, Enum):
    CREATED = "created"              # 201 — enqueued, id assigned
    UNAUTHORIZED = "unauthorized"    # 401 — bad/missing ingest token (config error)
    REJECTED = "rejected"            # 422/413 — bad/oversized spec (producer bug; do not retry)
    UNAVAILABLE = "unavailable"      # 503 — queue at capacity (retryable)
    TIMEOUT = "timeout"              # 408 — coordinator stalled reading the body (retryable)
    NETWORK_ERROR = "network_error"  # transport failure (retryable)
    ERROR = "error"                  # other unexpected status (e.g. 500)


# Transient outcomes worth a backed-off retry. The coordinator's own 503 (queue
# full) and 408 (stalled body) are documented as retryable; a network error may
# not have reached it at all. 422/413 (producer bug) and 401 (bad token) are
# terminal — retrying them only hammers the coordinator (FM2).
RETRYABLE = frozenset({PostOutcome.UNAVAILABLE, PostOutcome.TIMEOUT, PostOutcome.NETWORK_ERROR})


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
        408: PostOutcome.TIMEOUT,    # stalled-body timeout; coordinator says retry
        413: PostOutcome.REJECTED,   # body over the request cap — a producer bug
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
            "POST /jobs rejected %s (%s) as a bad spec (HTTP %s) — a producer bug, "
            "not retrying; check RenderJobSpec against validate_job_spec.",
            result.region_id, result.kind, result.status_code,
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
    max_attempts: int = DEFAULT_MAX_ATTEMPTS,
    backoff_base_secs: float = DEFAULT_BACKOFF_BASE_SECS,
    sleep: Callable[[float], None] = time.sleep,
) -> list[PostResult]:
    """POST each job to ``{base_url}/jobs``; one result per job, in order.

    Never raises on an HTTP status or transport error — the region is already
    authored and composed, so a posting failure is reported, not fatal (FM3). The
    bearer token (when set) authenticates the request and is never echoed back.

    Retryable outcomes (503, 408, network error) are retried up to
    ``max_attempts`` with exponential backoff (``backoff_base_secs * 2**n``);
    201/401/422/413 are terminal and return on the first response. ``sleep`` is
    injected so tests drive the backoff without real time.
    """
    url = f"{base_url.rstrip('/')}/jobs"
    headers = {"Content-Type": "application/json"}
    # A whitespace-only token is treated as no token (header omitted), not sent as
    # a bogus `Bearer ` credential — the transport stays correct in isolation even
    # if a caller forgets to sanitize (the CLI already strips).
    if token and token.strip():
        headers["Authorization"] = f"Bearer {token}"
        parsed = urlparse(base_url)
        if parsed.scheme == "http" and parsed.hostname not in _LOOPBACK:
            logger.warning(
                "posting a bearer token over http to %s — it is sent in cleartext; "
                "use https in production", parsed.hostname,
            )
    attempts = max(1, max_attempts)

    def attempt(job: RenderJobSpec) -> PostResult:
        region_id = job.region.region_id
        kind = job.kind.wire_name
        body = job.to_request()
        result = PostResult(region_id, kind, PostOutcome.NETWORK_ERROR)
        for n in range(1, attempts + 1):
            try:
                resp = client.post(url, json=body, headers=headers, timeout=timeout)
            except httpx.RequestError as e:
                result = PostResult(region_id, kind, PostOutcome.NETWORK_ERROR, detail=type(e).__name__)
            else:
                outcome = _classify(resp.status_code)
                if outcome is PostOutcome.CREATED:
                    job_id = _job_id(resp)
                    if job_id is None:
                        logger.warning(
                            "POST /jobs returned 201 for %s with no job id in the body "
                            "(job created but unpollable)", region_id,
                        )
                    return PostResult(region_id, kind, outcome, status_code=201, job_id=job_id)
                result = PostResult(region_id, kind, outcome, status_code=resp.status_code)
                if outcome not in RETRYABLE:
                    _log_terminal(result)
                    return result
            if n < attempts:
                sleep(backoff_base_secs * 2 ** (n - 1))
        logger.warning(
            "POST /jobs for %s (%s) failed after %d attempt(s): %s",
            region_id, kind, attempts, result.outcome.value,
        )
        return result

    return [attempt(job) for job in jobs]
