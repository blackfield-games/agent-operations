# blackfield — agent operations

BF6-register combined-arms combat. arenas hand-tuned. ambient frontier rendered by every operator on the mesh.

built on UE5 Lyra. $BLCKFLD on base.

## layout

```
mesh/         proprietary gpu pool + earner client
agents/       langgraph + temporal + ray runtime
contracts/    base / clanker (foundry)
engine/       ue5 project
assets/       brand
docs/         public docs only (internal docs gitignored)
```

## architecture

three backends behind the game, loosely coupled.

- **mesh** — the gpu pool. `coordinator` (rust / axum) queues render jobs and dispatches them to `earner` clients over a websocket, or an http poll. earners register their gpu and the job kinds they handle, and sign every result with a secp256k1 session key; the coordinator verifies the signer before a result counts. queue and results persist in sqlite, so a restart reclaims in-flight work instead of dropping it. `proto` holds the wire types shared by both sides. gpus joined and queue depth are exposed at `/stats`.
- **contracts** — foundry, targeting base. `ComputeMeter` burns $BLCKFLD into per-buyer compute credit, debited per validated job. `RegionAuthority` mints a region nft against a $BLCKFLD stake — holders earn a cut of the fees inside their region. `RenderReceipts` relays a validated render's attestation to EAS. `ArtifactTemplate` mints player-authored gear / structure templates (erc-1155) once a $BLCKFLD burn pays the rarity-scaled fee.
- **agents** — the content pipeline. a langgraph supervisor walks a world brief through eight specialists (director → terrain → biome → prop → lighting → npc → optimization → validator). each writes its own openusd layer; `compose` sublayers them into one `world.usda` in fixed strength order. the validator can route a rejection back to the earliest failing specialist for a re-run. each node wraps as a temporal activity so a single step can crash and resume.

the seam: agents author scene patches, those become render jobs the mesh dispatches, earners render and sign them, and validated work is metered by `ComputeMeter` and attested by `RenderReceipts` on base.

## build + test

each backend builds and tests on its own.

mesh — rust workspace (`proto`, `coordinator`, `earner`):

```
cd mesh
cargo test
cargo run -p coordinator   # serves 127.0.0.1:8787
```

contracts — foundry (deps vendored under `contracts/lib`):

```
cd contracts
forge test
```

agents — python (deps in `pyproject.toml`):

```
cd agents
python -m venv .venv
.venv/bin/pip install -e ".[dev]"
.venv/bin/python -m pytest
```

## status

phase 0. nothing here is shippable yet.
