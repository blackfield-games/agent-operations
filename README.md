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

A distributed render pool. The coordinator service (Rust, Axum) queues render jobs and dispatches them to earner clients over WebSocket or HTTP polling. Jobs enter the queue at runtime through a `POST /jobs` ingestion endpoint — the seam the agents pipeline uses to turn a scene patch into a render job — which validates every field of the request (a non-zero, bounded deadline, a payout that parses as bounded wei, a size-capped inputs blob, and an optional `buyer` that — when present — must be a well-formed EVM address, so the validated compute can be metered to it) and mints the job id server-side, so an untrusted caller can neither overwrite an existing job nor inject a value that crashes `/stats` later, before the spec is persisted `queued`. When an ingest token is configured (`--ingest-token` / `COORDINATOR_INGEST_TOKEN`), `POST /jobs` additionally requires an `Authorization: Bearer <token>` header — compared in constant time, with any absent, malformed, or wrong value rejected `401` — so only the authorized world-gen pipeline can enqueue payable work; left unset the endpoint stays open for local development and the coordinator logs a startup warning. The queued backlog is bounded by `--max-queued-jobs` / `COORDINATOR_MAX_QUEUED_JOBS` (default 10000): a create that would exceed the cap is shed with a retryable `503` rather than growing the queue without bound, while the boot-time seed and crash-recovery requeue are exempt. Each earner registers its available GPU and the job types it can handle — proving control of its secp256k1 session key by signing the registration (over a single-use challenge the coordinator issues on each WebSocket connection, so a captured registration can't be replayed), so a client can only register an identity it holds the key for — and signs every result with that same key; the coordinator verifies both signatures and checks the result is well-formed — a 256-bit output hash, a fetchable artifact URL, and a render time that is non-zero and plausible for the job's deadline — before accepting it, then records a pending EAS render receipt for each validated job. A background relayer drains that backlog, submitting each pending receipt to the `RenderReceipts` contract and marking it with the returned attestation UID so the backlog clears as receipts are published. Alongside the receipt, when compute metering is enabled (`--compute-rate-wei` / `COORDINATOR_COMPUTE_RATE_WEI`, default `0` = off, opt-in) and the job carries a buyer, the coordinator records a pending `ComputeMeter` debit — `rate × render-seconds` in wei, the metering twin of the receipt — in the same settle transaction, so a crash before the on-chain `spend` cannot lose the charge. A debit relayer drains that backlog the same way the attestation relayer drains receipts — submitting each pending debit as a `ComputeMeter.spend` and marking it once it lands — with the live Base spender operator-gated; an unmetered or buyerless job still settles and is still attested, accruing no debit. The job queue, results, pending receipts, and pending debits persist in SQLite, so an interrupted coordinator reclaims in-flight work on restart rather than dropping it. Shared wire types live in `proto`. Connected GPUs, queue depth, and both the attestation and debit backlogs are exposed at `/stats`.

### Contracts

A Foundry project targeting Base.

- **ComputeMeter** converts $BLCKFLD into per-buyer compute credit, debited as validated jobs complete.
- **RegionAuthority** mints a region NFT against a $BLCKFLD stake; fees earned within a region — including artifact mint royalties and validated render fees — are deposited on-chain and claimed by the current holder, with accrued fees settled to the outgoing holder on transfer.
- **RenderReceipts** publishes each validated render's attestation to the Ethereum Attestation Service, and lets an authorized coordinator revoke the receipt for a render later found invalid, withdrawing the on-chain attestation and excluding it from the live receipt counts. Each validated receipt also routes a configurable real-$BLCKFLD fee-share — scaled by the render's seconds and paid by the issuing coordinator — into the receipt's region fee pool on `RegionAuthority`; a region with no staked holder simply skips the fee so the attestation is never blocked.
- **ArtifactTemplate** mints player-authored equipment and structure templates as ERC-1155 tokens; each mint debits a rarity-scaled $BLCKFLD fee from the recipient's `ComputeMeter` credit and routes a separate rarity-scaled $BLCKFLD royalty into the minted artifact's region fee pool on `RegionAuthority`. A template may declare an optional max supply at registration (0 = unlimited), capping how many units can ever be minted so a high-rarity tier can be made genuinely scarce.

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
