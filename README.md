# oni-agent — Autonomous Agent for Oxygen Not Included

A Rust MCP client + control loop that lets an AI operate an ONI colony over the long term
without wasting game cycles while the agent thinks.

## Layout

```
oni-agent/
├── crates/oni-mcp      MCP client (streamable HTTP transport)
├── crates/oni-world    World model + preflight checks (coords/footprint/worksites)
├── crates/oni-engine   Control loop + resident daemon (sense→judge→decide→exec→verify)
├── crates/oni-cli      `oni` command-line binary
└── ~/.oni-agent/       Runtime state (logs, snapshots, per-save isolation)
```

`oni-agent` is the whole agent; `oni-mcp` is only its transport layer. The binary is `oni`.

## Quick start

```bash
cargo build --release
./target/release/oni doctor          # connectivity / catalog / protocol check
./target/release/oni observe         # colony situation card + ledger + trend
./target/release/oni observe --cached # read daemon snapshot (zero MCP pressure)
./target/release/oni plan            # pick this cycle's single goal from the dependency graph
./target/release/oni act             # execute (preflight → dry-run → apply)
./target/release/oni preflight dig x1,y1,x2,y2
./target/release/oni preflight build x,y Prefab
./target/release/oni probe x1,y1,x2,y2
./target/release/oni graph [--full]  # chamber graph + air budget + route to surface
./target/release/oni agent --interval 10 --window 120
./target/release/oni window 300      # request a construction window
./target/release/oni hold "<reason>" / oni release
./target/release/oni escalations / bus timeline / status
```

## Two-speed architecture

- **Fast system** (daemon, every 10s): sensing, danger detection, execution, safe degradation.
- **Slow system** (the AI agent, on demand): strategy, replanning, exception handling, rule updates.

The fast system publishes escalations and a shared snapshot; the slow system writes `plan.json`
and precise orders. No duplicate MCP collection: the daemon is the only sampler.
See [`docs/coordination.md`](docs/coordination.md) and [`docs/dual-system.md`](docs/dual-system.md).

## Key mechanisms

- **Official facts first**: reachability, idle counts, power, materials and buildings come from game APIs, not guesses.
- **Effect criteria**: success checks use end-state metrics (oxygen, food, population, reachable cells), not intermediate stock.
- **Semantic actions**: plans contain intents (`surface`, `open_chamber`, `element:X`, `material`), not hardcoded coordinates.
- **Preflight before write**: every dig/build goes through preflight + dry-run + apply + re-read.
- **Pause ownership**: pauses are explicit (`owner`, `reason`, `since`) and survive process restarts; resume is owner-checked.
- **Decision record**: each slow-system decision is logged with rationale and expected outcome (see `decisions.jsonl`).

## Docs

- [`AGENTS.md`](AGENTS.md) — engineering rules for agents working in this repo
- [`docs/knowledge-base.md`](docs/knowledge-base.md) — sourced ONI mechanics
- [`docs/coordination.md`](docs/coordination.md) — fast/slow system coordination
- [`design/graph-model.md`](design/graph-model.md) — chamber graph design

## Requirements

Rust 1.94+. In-game mods: `Jlib` + `ONI MCP Server` at `http://127.0.0.1:8788/mcp/` (protocol 2025-11-25).
