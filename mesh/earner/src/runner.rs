//! Render job runner. Stubbed for the scaffold — real impl shells out to:
//!   - Houdini (terrain / foliage) via MCP
//!   - 3DGS toolchain (Postshot / Polycam capture optimization)
//!   - Overworld-class local diffusion (frontier tiles)
//!   - NVIDIA ACE / local quantized LLM (npc_tick)
//!
//! All sandboxed in gVisor+nvproxy on Linux (TODO).

use anyhow::Result;
use proto::{JobKind, JobSpec};

pub async fn render(job: &JobSpec) -> Result<Vec<u8>> {
    match job.kind {
        JobKind::Terrain => Ok(stub_usd_layer("Terrain", &job.region.region_id()).into_bytes()),
        JobKind::Foliage => Ok(stub_usd_layer("Foliage", &job.region.region_id()).into_bytes()),
        JobKind::NpcTick => Ok(b"{}\n".to_vec()),
        JobKind::DiffusionTile => Ok(vec![0u8; 1024]), // placeholder PNG bytes
        JobKind::Optimization => Ok(stub_usd_layer("Optimization", &job.region.region_id()).into_bytes()),
    }
}

fn stub_usd_layer(kind: &str, region_id: &str) -> String {
    format!(
        "#usda 1.0\n(\n    defaultPrim = \"{kind}\"\n)\n\ndef Scope \"{kind}\"\n{{\n    custom string regionId = \"{region_id}\"\n    custom string source = \"earner-stub\"\n}}\n"
    )
}
