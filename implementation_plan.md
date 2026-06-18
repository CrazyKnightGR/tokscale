# Implementation Plan - Claude Subscription Quota & Limit Estimation

The goal is to add detailed metrics (tokens used/limits, burn rate, cost rate, and runs-out predictions) to the Claude subscription usage views, matching the features of `claude-code-usage-monitor`.

## User Review Required

We are modifying the subscription usage display schema by adding text-only child metrics under the main "Session" and "Weekly" progress bar rows. When a metric's `remaining_percent` is negative, we will hide the progress bar and let the custom `remaining_label` use the available space.

## Proposed Changes

### CLI & TUI Common

#### [MODIFY] [claude.rs](file:///home/janisx/Downloads/tokscale-3.1.3/tokscale-3.1.3/crates/tokscale-cli/src/commands/usage/claude.rs)

We will modify `claude.rs` to:
1. Scan local Claude Code messages from the `~/.claude/projects/` directory using the core library `tokscale_core::parse_local_unified_messages`.
2. Compute the current active token burn rate (tokens/minute) and cost rate (dollars/minute) by analyzing messages in the last 1 hour.
3. Determine token limits based on the user's plan.
4. Calculate detailed child metrics for both the 5-hour rolling session window (`Session`) and the 7-day window (`Weekly`):
   - **Tokens**: Used vs limit (e.g. `2.5k/19k used`)
   - **Burn Rate**: Consumption speed in tokens/min
   - **Cost Rate**: Spend speed in $/min
   - **Runs Out**: Predicted depletion time relative to the reset window

#### [MODIFY] [mod.rs](file:///home/janisx/Downloads/tokscale-3.1.3/tokscale-3.1.3/crates/tokscale-cli/src/commands/usage/mod.rs)

In `render_light`, we will detect if `m.remaining_percent < 0.0`. If so, we will skip rendering the progress bar and print the custom label with an expanded width of 25 characters to avoid truncation.

### TUI Screens

#### [MODIFY] [usage.rs](file:///home/janisx/Downloads/tokscale-3.1.3/tokscale-3.1.3/crates/tokscale-cli/src/tui/ui/usage.rs)

In `metric_line`, we will handle `metric.remaining_percent < 0.0` similarly to `render_light`. We will omit the progress bar and expand the `value` field to 25 characters width, allowing the text-only metric details to span across the unused space.

---

## Verification Plan

### Automated Tests
- Run `cargo test` in `crates/tokscale-cli` and `crates/tokscale-core` to verify that there are no regressions.

### Manual Verification
- Run `cargo run -- usage` (or `cargo run -- usage --light`) and observe the new fields under the Claude section.
- Open the TUI via `cargo run` and navigate to the `Usage` tab to inspect the detailed lines under the Claude section.
