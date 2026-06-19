"""Tests for the render-job posting transport (runtime/poster.py).

These pin the agents -> coordinator HTTP seam: every job is POSTed to
{base_url}/jobs with the exact CreateJobRequest body and the bearer header, and
each coordinator status is classified into a distinct, non-collapsed outcome
(FM2). The HTTP client is injected, so no live coordinator is needed. Run from
the agents/ dir:
    .venv/bin/python -m pytest test_poster.py -v
"""

import httpx

from common.jobs import RenderJobSpec
from common.types import JobKind, RegionCoord
from runtime.poster import PostOutcome, post_jobs


def _job(x=42, y=-17, layer=0) -> RenderJobSpec:
    return RenderJobSpec(
        kind=JobKind.DIFFUSION_TILE,
        region=RegionCoord(x=x, y=y, layer=layer),
        deadline_secs=3600,
        max_payout_wei="1000000000000000000",
        inputs={"region_id": f"r{x:+05d}_{y:+05d}_l{layer}", "world": "world.usda"},
    )


class _Resp:
    def __init__(self, status_code: int, body=None):
        self.status_code = status_code
        self._body = body

    def json(self):
        if self._body is None:
            raise ValueError("no json body")
        return self._body


class _Client:
    """Mock HttpClient: yields scripted responses (or raises) in order, recording
    every call so a test can assert the exact URL, body, header, and timeout."""

    def __init__(self, *responses):
        self._responses = list(responses)
        self.calls: list[dict] = []

    def post(self, url, *, json, headers, timeout):
        self.calls.append({"url": url, "json": json, "headers": headers, "timeout": timeout})
        r = self._responses.pop(0)
        if isinstance(r, Exception):
            raise r
        return r


# --- 201: enqueued, id extracted, exact wire body + header (FM1/FM2) ---

def test_created_returns_job_id_and_posts_exact_request():
    job = _job()
    client = _Client(_Resp(201, {"id": "11111111-2222-3333-4444-555555555555"}))

    [result] = post_jobs([job], base_url="http://coord:8080", token="tok-abc", client=client)

    assert result.outcome is PostOutcome.CREATED
    assert result.ok
    assert result.job_id == "11111111-2222-3333-4444-555555555555"
    assert result.status_code == 201

    call = client.calls[0]
    assert call["url"] == "http://coord:8080/jobs"
    assert call["json"] == job.to_request()  # exact CreateJobRequest body
    assert call["headers"]["Authorization"] == "Bearer tok-abc"  # exact bearer format


# --- each status classified distinctly, not collapsed (FM2) ---

def test_unauthorized_is_terminal_config_error():
    client = _Client(_Resp(401))
    [result] = post_jobs([_job()], base_url="http://c", token="bad", client=client)
    assert result.outcome is PostOutcome.UNAUTHORIZED
    assert not result.ok
    assert len(client.calls) == 1  # no retry


def test_rejected_is_terminal_producer_bug():
    client = _Client(_Resp(422))
    [result] = post_jobs([_job()], base_url="http://c", token="t", client=client)
    assert result.outcome is PostOutcome.REJECTED
    assert len(client.calls) == 1  # 422 is a producer bug — never retried


def test_unavailable_classified():
    client = _Client(_Resp(503))
    [result] = post_jobs([_job()], base_url="http://c", token="t", client=client)
    assert result.outcome is PostOutcome.UNAVAILABLE
    assert result.status_code == 503


def test_unexpected_status_is_error():
    client = _Client(_Resp(500))
    [result] = post_jobs([_job()], base_url="http://c", token="t", client=client)
    assert result.outcome is PostOutcome.ERROR
    assert result.status_code == 500


# --- network failure is non-fatal, never crashes the pipeline (FM3) ---

def test_network_error_is_non_fatal_result():
    client = _Client(httpx.ConnectError("connection refused"))
    [result] = post_jobs([_job()], base_url="http://c", token="t", client=client)
    assert result.outcome is PostOutcome.NETWORK_ERROR
    assert result.detail == "ConnectError"  # class name only, no message echo


# --- token handling (security) ---

def test_no_token_omits_authorization_header():
    client = _Client(_Resp(201, {"id": "x"}))
    post_jobs([_job()], base_url="http://c", token=None, client=client)
    assert "Authorization" not in client.calls[0]["headers"]


def test_token_never_appears_in_results():
    secret = "super-secret-ingest-token"
    client = _Client(_Resp(401))
    results = post_jobs([_job()], base_url="http://c", token=secret, client=client)
    assert secret not in repr(results)
    for r in results:
        assert secret not in repr(r)
        assert all(secret != getattr(r, f) for f in ("region_id", "kind", "job_id", "detail"))


# --- batch: one result per job, in order ---

def test_multiple_jobs_one_result_each_in_order():
    jobs = [_job(x=1), _job(x=2), _job(x=3)]
    client = _Client(
        _Resp(201, {"id": "a"}), _Resp(422), _Resp(201, {"id": "c"})
    )
    results = post_jobs(jobs, base_url="http://c", token="t", client=client)
    assert [r.region_id for r in results] == [j.region.region_id for j in jobs]
    assert [r.outcome for r in results] == [
        PostOutcome.CREATED, PostOutcome.REJECTED, PostOutcome.CREATED
    ]


def test_empty_jobs_no_calls():
    client = _Client()
    assert post_jobs([], base_url="http://c", token="t", client=client) == []
    assert client.calls == []


def test_base_url_trailing_slash_normalized():
    client = _Client(_Resp(201, {"id": "x"}))
    post_jobs([_job()], base_url="http://c:8080/", token="t", client=client)
    assert client.calls[0]["url"] == "http://c:8080/jobs"  # no double slash
