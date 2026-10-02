# refactor-check workspace

Rust tooling for verifying code correctness using LLM + SMT solver.

## Crates

- **[`crates/refactor-check/`](crates/refactor-check/)** — Check whether a code refactoring preserved semantic equivalence. See [`crates/refactor-check/README.md`](crates/refactor-check/README.md).
- **[`crates/deductive-check/`](crates/deductive-check/)** — Deductive verification of code against specifications. See [`crates/deductive-check/AGENTS.md`](crates/deductive-check/AGENTS.md).
- **[`crates/core/`](crates/core/)** — Shared infrastructure (LLM client, solver interface, piece management).

## Build

```bash
cargo build --release --workspace
```

## Inference backends

`refactor-check` supports both OpenAI-compatible APIs and the local Codex CLI.
Use an existing Codex login and local model configuration without an API key:

```bash
./target/release/refactor-check --input example_input.txt --llm-provider codex
```

Optional `--primary-model`, `--judge-model`, and `--codex-reasoning-effort`
overrides select your Codex models and reasoning effort. See the
[refactor-check usage guide](crates/refactor-check/README.md#codex-cli-backend).
The SMT solver and verification workflow are the same for both backends.
The `deductive-check` executable continues to use its existing API backend.

## Test

```bash
cargo test --workspace
```

## Lint

```bash
cargo clippy --workspace -- -W clippy::all
```
