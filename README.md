# Blackfield — Agent Operations

**Combined-arms tactical combat with a living, operator-rendered frontier.**

Blackfield pairs hand-tuned competitive arenas with an ambient open world rendered by a distributed network of operators. The project is built on Unreal Engine 5 (Lyra), and its compute economy settles on Base.

Website: [blackfield.games](https://blackfield.games)

## Repository Layout

| Directory   | Contents                                          |
| ----------- | ------------------------------------------------- |
| `engine/`   | Unreal Engine 5 project                           |
| `mesh/`     | Distributed GPU pool and earner client            |
| `agents/`   | World-generation content pipeline                 |
| `contracts/`| On-chain contracts (Base, Foundry)                |
| `assets/`   | Brand and art assets                              |
| `docs/`     | Public documentation                              |

## Architecture

Three loosely coupled backends sit behind the game client.

### Mesh

A distributed render pool. The coordinator service (Rust, Axum) queues render jobs and dispatches them to earner clients over WebSocket or HTTP polling. Each earner registers its available GPU and the job types it can handle, and signs every result with a secp256k1 session key; the coordinator verifies the signature and checks the result is well-formed — a 256-bit output hash, a fetchable artifact URL, and a render time that is non-zero and plausible for the job's deadline — before accepting it, then records a pending EAS render receipt for each validated job. A background relayer drains that backlog, submitting each pending receipt to the `RenderReceipts` contract and marking it with the returned attestation UID so the backlog clears as receipts are published. The job queue, results, and pending receipts persist in SQLite, so an interrupted coordinator reclaims in-flight work on restart rather than dropping it. Shared wire types live in `proto`. Connected GPUs, queue depth, and the attestation backlog are exposed at `/stats`.

### Contracts

A Foundry project targeting Base.

- **ComputeMeter** converts $BLCKFLD into per-buyer compute credit, debited as validated jobs complete.
- **RegionAuthority** mints a region NFT against a $BLCKFLD stake; holders earn a share of the fees generated within their region.
- **RenderReceipts** publishes each validated render's attestation to the Ethereum Attestation Service.
- **ArtifactTemplate** mints player-authored equipment and structure templates as ERC-1155 tokens; each mint debits a rarity-scaled $BLCKFLD fee from the recipient's `ComputeMeter` credit.

### Agents

The world-generation pipeline. A LangGraph supervisor routes a world brief through eight specialist stages: director, terrain, biome, prop, lighting, NPC, optimization, and validation. Each stage writes its own OpenUSD layer, which `compose` then sublayers into a single `world.usda` in a fixed strength order. When validation fails, the supervisor can return the brief to the earliest failing stage for revision. Each stage runs as a Temporal activity, so an individual step can fail and resume without restarting the pipeline.

These pieces connect end to end: the agents author scene patches, which become render jobs; the mesh dispatches those jobs to earners, who render and cryptographically sign the output; and validated work is metered by `ComputeMeter` and attested by `RenderReceipts` on Base.

## Building and Testing

Each backend builds and tests independently.

**Mesh** — Rust workspace (`proto`, `coordinator`, `earner`):

```
cd mesh
cargo test
cargo run -p coordinator   # serves 127.0.0.1:8787
```

**Contracts** — Foundry (dependencies vendored under `contracts/lib`):

```
cd contracts
forge test
```

**Agents** — Python (dependencies in `pyproject.toml`):

```
cd agents
python -m venv .venv
.venv/bin/pip install -e ".[dev]"
.venv/bin/python -m pytest
```

## Project Status

Blackfield is in pre-production. The codebase is under active development and is not yet ready for release.
