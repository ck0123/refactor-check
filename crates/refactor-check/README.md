# refactor-check

Check whether a code refactoring preserved semantic equivalence — automatically, using LLM + SMT solver.

## What it does

`refactor-check` takes a human-readable description of a refactoring ("before" and "after" versions of the same function) and verifies that the two versions are semantically equivalent.

It works by:

1. **Decomposing** the code into sub-pieces (loops, conditionals, helper functions) using an LLM
2. **Generating** an SMT-LIB2 formula for each piece that asserts the "before" and "after" produce the same results
3. **Running** an SMT solver (Z3) on each formula — in parallel
4. **Judging** each result: a cheap LLM checks whether SAT/UNSAT/UNKNOWN is actually reasonable for the piece
5. **Reporting**: if all pieces are `unsat` (no counterexample) → equivalent. If any piece is `sat` → not equivalent.

If a formula is too complex and the solver times out, the system automatically asks the LLM to split the code into smaller pieces and try again.

## Build

```bash
cargo build --release
```

## Usage

```bash
./target/release/refactor-check \
  --input example_input.txt \
  --primary-model "$MODEL_ID"
```

Set `OPENROUTER_API_KEY` for the API backend, or supply `--api-key`.

### Codex CLI backend

With an installed, authenticated Codex CLI, no API key is needed:

```bash
codex login
./target/release/refactor-check --input example_input.txt --llm-provider codex
```

Omitting `--primary-model` and `--codex-reasoning-effort` preserves your local
Codex model/provider and reasoning configuration. To override them:

```bash
./target/release/refactor-check \
  --input example_input.txt \
  --llm-provider codex \
  --primary-model "$CODEX_MODEL" \
  --codex-reasoning-effort high
```

`--judge-model` overrides the judge and splitting-judge model; all other roles
use the primary model. Each request starts a fresh `codex exec --ephemeral`
session in a temporary directory, sends the role-tagged conversation through
stdin, and reads only `--output-last-message`. Progress output is never treated
as a model answer. The invocation selects read-only mode, disables shell and
web search, omits project instructions, and asks for text-only inference.
Other local Codex configuration, including configured MCP servers, remains
inherited; this adapter is not a general-purpose isolation boundary.

The per-request timeout defaults to 300 seconds and includes prompt delivery.
Failed processes, missing/empty final responses, and timeouts are errors, not
verification success. Timeout or cancellation terminates the direct CLI child;
the adapter does not promise process-tree supervision of arbitrary descendants.
`--codex-path` selects a different executable. API connection/key/service-tier
and stream options apply only to the API backend; Z3 options apply to both.
Codex model availability follows your configured provider and account.

This backend uses Codex's supported
[non-interactive interface](https://learn.chatgpt.com/docs/non-interactive-mode)
and has been tested with Codex CLI 0.160.0. It does not extract or copy login tokens.

### Required

| Flag | Description |
|------|-------------|
| `--input <path>` | Path to the refactoring description file |
| `--primary-model <model>` | Primary model; required for the default API backend |

### Optional

| Flag | Default | Description |
|------|---------|-------------|
| `--solver-path` | `z3` | Path to the SMT solver binary |
| `--solver-args` | `-in` | Arguments passed to the solver |
| `--solver-timeout-secs` | `60` | Max seconds per solver invocation |
| `--llm-provider` | `api` | Inference backend: `api` or `codex` |
| `--api-key` | `OPENROUTER_API_KEY` | API backend key |
| `--judge-model` | primary model | Model for result and decomposition evaluation |
| `--codex-path` | `codex` | Codex CLI executable |
| `--codex-reasoning-effort` | local Codex config | Codex reasoning effort override |
| `--codex-timeout-secs` | `300` | Wall-clock timeout for each Codex invocation |
| `--api-base` | `https://openrouter.ai/api/v1` | API endpoint |
| `--stream-timeout-ms` | `3000` | Per-chunk stream timeout |
| `--max-stream-retries` | `5` | Retries on stream errors |
| `--service-tier` | `auto` | API service tier (auto/default/flex/scale/priority) |

The CLI uses one API key for all roles. Role-specific API credentials are available
through the library configuration, not through Codex login-token extraction.

## Example input file

The format is a plain-text before/after function comparison. Any human-readable format works — the LLM parses it.

```
--- BEFORE ---

int abs(int x) {
    if (x < 0) return -x;
    return x;
}

--- AFTER ---

int abs(int x) {
    return (x < 0) ? -x : x;
}
```

## How to read the output

```
=== Overall Equivalent ===
true

=== Verified Formulas ===
1/1 reasonable (SAT: 0, UNSAT: 1, UNKNOWN: 0)

=== Open Pieces ===
0

--- Formula ---
[abs #1]
(declare-fun ...)
Outcome: Unsat, Verdict: REASONABLE
```

- **SAT**: the solver found a counterexample → this piece is NOT equivalent
- **UNSAT**: the SMT solver proved no counterexample exists → this piece is equivalent
- **UNKNOWN**: the solver couldn't decide → inconclusive, may need manual review
- **Open Pieces**: formulas that timed out or were rejected by the judge — not fully resolved

## Architecture

- `src/llm.rs` — async streaming LLM client (async-openai), per-role models and API keys, retry with partial content tolerance
- `src/smt.rs` — SMT solver subprocess manager with timeout support, per-piece tracing
- `src/agent.rs` — compositional verification entry point
- `src/machine.rs` — state machine orchestrating split → judge-split → generate → solve → judge → result
- `src/transitions.rs` — state machine transitions for all phases
- `src/states.rs` — state structs: `WaitForSplit`, `WaitForSplittingJudge`, `WaitForGeneration`, `WaitForResults`, `WaitForExplanation`
- `src/behaviors/` — per-phase logic: `splitter`, `splitting_judge`, `generation`, `results`, `child_judge`, `explain`
- `src/piece_manager.rs` — `PieceManager` trait for piece lifecycle (ID assignment, phase tracking)
- `src/provider.rs` — trait abstractions for `LlmProvider` and `SolverProvider`

### Testing

- `tests/agent_integration.rs` — integration tests with `SequenceLlm` and `FakeSolver`
- `tests/struct_max_sat.rs` — SAT detection regression test
- `tests/log_replay.rs` — replay-based tests using `LogReplayLlm` and `LogReplaySolver` mocks with per-pieceid message queues, including split depth exhaustion via splitting judge
- `tests/e2e_api.rs` — end-to-end tests against live API
- `tests/helpers/` — shared test utilities sub-crate (`refactor-check-test-helpers`)
- `parse_log.rb` — Ruby script to convert a refactor-check trace log into a Rust test case
