# CLAUDE.md

<!-- ACTIONS-SPEND-GUARD -->
## GitHub Actions spending — NEVER spend without explicit approval

**The owner pays real money for GitHub Actions minutes on this private repo. A July 2026
overrun cost 140 USD.** Treat every Actions minute as billed cash.

Hard rules — no exceptions unless the owner approves in the current conversation:

1. **Do NOT enable, re-enable, or un-disable any workflow.** Workflows marked
   `disabled_manually` are disabled on purpose. Never call `.../actions/workflows/<id>/enable`.
2. **Do NOT add `on: push`, `on: pull_request`, `on: pull_request_target`, or `on: schedule`.**
   New workflows are `on: workflow_dispatch` only.
3. **Do NOT trigger runs** — no `gh workflow run`, no `gh run rerun`, and do not push branches
   or open/update PRs for the purpose of making CI run.
4. **Do NOT add or widen a job matrix.** Fan-out caused the 140 USD bill (6 parallel jobs
   x 238 runs in four days).
5. **Never use `windows-*` (2x cost) or `macos-*` (10x cost) runners.** Linux only.
6. **Verify locally instead** — run the equivalent commands on the dev machine. Local
   verification is the primary gate by owner directive.

If a run seems genuinely necessary: stop, state what you want to run and the estimated
minutes, and ask. Do not run it and apologise afterwards.

Deploy/release workflows (`deploy_*`, `deploy-*`, `cd.yml`, `release.yml`, `docker.yml`) stay
live so shipping works. Do not disable them and do not bolt checks onto them.
<!-- /ACTIONS-SPEND-GUARD -->
