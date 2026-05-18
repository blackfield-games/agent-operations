"""Temporal workflow wrapping the LangGraph supervisor.

Pattern from research-agent-runtime.md: LangGraph as a library, each specialist
call wrapped as an idempotent Temporal activity. The LLM call must itself be an
activity (sampling non-determinism breaks Temporal replay).

This file is a stub — register with the worker once Temporal SDK is in the
service container and Ray Serve endpoints are wired.
"""

from __future__ import annotations

from datetime import timedelta
from temporalio import workflow, activity

from common.types import WorldBrief, LayerSpec, ValidatorVerdict
from .supervisor import build_graph


@activity.defn
async def run_world_brief(brief: WorldBrief) -> list[LayerSpec]:
    graph = build_graph()
    result = await graph.ainvoke({"brief": brief, "layers": [], "rounds": 0})
    return result.get("layers", [])


@workflow.defn
class GenerateRegion:
    @workflow.run
    async def run(self, brief: WorldBrief) -> list[LayerSpec]:
        return await workflow.execute_activity(
            run_world_brief,
            brief,
            start_to_close_timeout=timedelta(minutes=10),
            heartbeat_timeout=timedelta(seconds=30),
        )
