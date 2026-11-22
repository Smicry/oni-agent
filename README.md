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
./target/release/oni serve           # start BOTH systems + Web console (:8789)
# ./target/release/oni agent --interval 10 --window 120   # fast loop only (legacy)
./target/release/oni window 300      # request a construction window (agent resumes + takes over)
./target/release/oni hold "<reason>" / oni release        # pause/resume for code changes
./target/release/oni escalations / bus timeline / status
```

Open **http://localhost:8789/** for the control console. It has a single AI on/off switch
(`stop` = hold: freeze world + agent stands down; `start` = release + open a work window so the
agent resumes and takes over game speed) plus speed buttons (1x/2x/3x). Game pause/resume is
fully delegated to the agent — no separate game-pause button.

## Two-speed architecture

- **Fast system** (daemon, every 10s): sensing, danger detection, execution, safe degradation.
- **Slow system** (LLM, every 5s): strategy, replanning, exception handling, rule updates.

The fast system publishes escalations and a shared snapshot; the slow system writes semantic
goals (`ai_goals.jsonl`) and precise orders. No duplicate MCP collection: the daemon is the only
sampler. See [`docs/coordination.md`](docs/coordination.md) and [`docs/dual-system.md`](docs/dual-system.md).

**The authoritative fast/slow contract is [`docs/FAST_SLOW_CONTRACT.md`](docs/FAST_SLOW_CONTRACT.md).**
In short: the slow system decides *where to go*, the fast system decides *how to survive right now* —
the slow system **never blocks** the fast system. `escalations` are FYI reports for strategy, not
approval gates; the fast system keeps executing life-saving actions while the LLM thinks.

The slow system is **not purely reactive**: besides escalations, a 60s *proactive-planning heartbeat*
re-reads the strategic plan and pushes the next milestone even when nothing is wrong. It reads
`plan.json`（AI-owned, `locked:true`) and injects a progress summary into the prompt. An endgame
meta-decision detects a wiped colony and advises rollback instead of burning LLM calls.

Escalation dedup uses a `kind|node|category` silence key (180s) with deterioration override
(oxygen/food −20%, breath −15, bladder +15) so the same issue doesn't spam while real decline breaks
through. On life-red-line emergencies the fast system skips only *expansion* goals — survival goals
(toilet/oxygen/material) always run.

## Key mechanisms

- **Official facts first**: reachability, idle counts, power, materials and buildings come from game APIs, not guesses.
- **Effect criteria**: success checks use end-state metrics (oxygen, food, population, reachable cells), not intermediate stock.
- **Semantic actions**: plans contain intents (`surface`, `open_chamber`, `element:X`, `material`), not hardcoded coordinates.
- **Preflight before write**: every dig/build goes through preflight + dry-run + apply + re-read.
- **Single control switch (AI on/off)**: the user controls the agent, not the game clock. `stop` (hold) is the *only* user stop signal — it freezes the world and stands the agent down, surviving reloads/restarts. While AI is on, the agent owns game pause/resume entirely: any non-hold pause is auto-resumed with a fresh work window, so long-running jobs (shafts, research) are never stranded by a stale "user paused" flag. Old per-pause owners (`agent`/`none`/`user`) are all treated as resumable now; only `hold` is authoritative.
- **Decision record**: each slow-system decision is logged with rationale and expected outcome (see `decisions.jsonl`).
- **Structured failure feedback**: every goal execution writes `goal_outcomes.jsonl` with a
  `fail_kind` (`no_target`/`no_material`/`research_locked`/`preflight`/`rejected`/`no_placement`/`bad_goal`).
  The LLM sees *why* a goal failed, not just a count, so it fixes the root cause instead of renaming goals.
- **Permanent circuit breaker**: a goal that has never succeeded and fails ≥3 times is banned for
  the whole save (time cooldowns used to let the same dead goal back in; `oxygen` 0/8 once dragged
  the colony to death). An idempotency streak (`🔁` in the prompt) also forces a switch after 3
  repeated decisions with no progress.
- **Two isolated command channels**: `commands.jsonl` holds only real `source=user` instructions;
  AI-set semantic goals live in `ai_goals.jsonl`. Mixing them made 63% of decisions claim a
  "user instruction" that never existed — a self-reinforcing hallucination loop.
- **Result-bearing chat history**: escalation cards in the conversation carry the slow system's
  resolve note as `result`, so later decisions can see how a prior issue was handled (feedback loop).
- **Loop → escalation**: an action fingerprint (`kind@x,y`) repeated ≥3 times means the executor is
  spinning (same dig order over and over) — the fast system stops repeating it, reports a structured
  card, and defers the node so the slow system can pick a different route.
- **Parameterised site policy**: "what counts as a safe build site" is a *policy*, not a hardcoded
  rule. `site_policy.json` holds `co2_max_kg` / `strict_prefabs` / `relax_after_fail`; the slow
  system sees them in its prompt and can adjust them (`oni policy set`, or a `policy` field in its
  decision) with immediate + persisted effect. The fast system also self-relaxes after repeated
  failures. Facts (what a cell is, whether an order landed) stay in code.
- **Singleflight + cache**: concurrent reads of the same cell URI collapse into one MCP request and
  the read cache TTL covers a whole cycle, so the per-cycle preflight no longer floods the mod
  endpoint (it used to queue hundreds of requests and stall the cycle for ~90s).

## MCode integration (Hook + Skill)

This repo ships a project-level Hook and Skill for [MCode](https://www.minimax.io/):

```
.minimax/
├── hooks.json                       # UserPromptSubmit → oni pause (freeze world on new message)
├── scripts/pause-on-prompt.mjs      # silent, fail-open; path resolved relative to the script
├── scripts/session-id.sh            # stable per-save key from initial save hash
└── skills/oni-colony-ops/SKILL.md   # colony takeover loop: freeze→observe→replan→execute→verify
```

The Hook is project-scoped and inactive until enabled in MCode → Settings → Hooks → Project.
It only affects this repository and never blocks the conversation.

## Docs
- [`AGENTS.md`](AGENTS.md) — engineering rules for agents working in this repo
- [`docs/knowledge-base.md`](docs/knowledge-base.md) — sourced ONI mechanics
- [`docs/coordination.md`](docs/coordination.md) — fast/slow system coordination
- [`design/graph-model.md`](design/graph-model.md) — chamber graph design

## Requirements

Rust 1.94+. In-game mods: `Jlib` + `ONI MCP Server` at `http://127.0.0.1:8788/mcp/` (protocol 2025-11-25).
