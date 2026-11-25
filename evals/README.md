# agent86 eval set

Repository tasks with hidden checks, run through `agent86 run --json` by the
`agent86-eval` crate (`crates/agent86-eval`). How to run it, the task layout, and
how to add a task: [docs/evals.md](../docs/evals.md).

```text
cargo run -p agent86-eval -- validate
cargo run -p agent86-eval -- run --model <provider:model> --window 32768
```
