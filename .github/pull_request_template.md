## What
<!-- One-paragraph summary of the change. -->

## Why
<!-- The problem / motivation. -->

## How
<!-- Key implementation points, tradeoffs, alternatives considered. -->

## Testing
<!-- Tests added/updated. For hot-path changes, paste before/after from scripts/bench-all.sh. -->

## Checklist
- [ ] `cargo fmt --all` and `cargo clippy --all-targets` are clean
- [ ] `cargo test` passes
- [ ] Behavior changes are covered by tests
- [ ] Hot path stays allocation-free (or the added allocation is justified)
