# Multi-Source Test Server Phase 1 Review Notes

This note documents the review lane for the Phase 1 multi-source test-server work described by:

- PRD: `.omx/plans/prd-multi-source-test-server-phase1.md`
- Test spec: `.omx/plans/test-spec-multi-source-test-server-phase1.md`

Phase 1 is intentionally **correctness-first**. It should prove that the existing multi-source
runtime can download byte-correct output from multiple repo-native `test_server` instances with
different rate limits. It must not become a throughput-acceptance project or a broad
failure-recovery matrix.

## Review Scope

Expected implementation/review surfaces:

- `test_server/server.py` for additive multi-instance configuration.
- `tests/multi_source.rs` plus any small Rust helper module for spawning and coordinating local
  test-server processes.
- Narrow, additive testability seams in `src/downloader.rs`, `src/monitor.rs`, `src/lane.rs`, or
  `src/types.rs` only if the tests cannot otherwise verify the real runtime path.

Surfaces that should remain out of scope:

- README rewrites or user-facing documentation churn.
- Throughput-target assertions such as `>=80%` aggregate speed.
- Comprehensive demotion/recovery/blacklist policy matrices.
- Scheduler redesign in `src/concurrency.rs` or `src/lane.rs`.
- Removal or weakening of the existing `mockito`-based multi-source tests.

## Code Quality Review Checklist

Use this checklist when reviewing the implementation diff for Phase 1.

### Server configurability

- Multi-instance controls are additive and default-compatible with `python test_server/server.py`.
- Tests do not mutate the checked-in `test_server/config.ini` as shared state.
- Per-instance port, served directory, and throttling settings are isolated per process.
- Range responses still include `Accept-Ranges`, `Content-Length`, and `Content-Range` as
  appropriate.
- Readiness is observable before Rust tests start downloading.
- Child process shutdown is bounded and does not leave Python servers running after tests finish.

### Rust harness

- Tests construct `MultiSourceConfig` and call `Downloader::new_multi(...)`; they do not use a
  fake downloader path.
- Temporary files/directories are owned by the test and are cleaned up by RAII or explicit teardown.
- Port selection avoids fixed-port collisions where feasible.
- Readiness polling uses bounded timeouts instead of sleeps alone.
- Helpers are small and local to tests unless multiple integration suites reuse them.

### Scenario coverage

- Fast + slow sources: two differently throttled local servers serve the same bytes and produce an
  exact byte match.
- Three heterogeneous sources: three local servers with distinct throttles complete with an exact
  byte match.
- Invalid + valid sources: one unusable source is skipped and remaining valid throttled sources
  still complete the output exactly.
- Existing fast tests in `tests/multi_source.rs` continue to pass.

### Phase-boundary guardrails

- Metrics, if logged, are supporting diagnostics only.
- No test accepts or rejects behavior based on a throughput threshold.
- No broad recovery-policy matrix is added in this phase.
- No public status payload shape is changed solely to make tests easier.

## Acceptance Mapping

| Test spec check | Review evidence to look for |
| --- | --- |
| AC1 - multiple differently throttled `test_server` sources | Rust tests spawn two or more Python servers with distinct per-instance throttles. |
| AC2 - real multi-source runtime path | Scenarios instantiate `MultiSourceConfig` and run `Downloader::new_multi(...)`. |
| AC3 - byte-correct output | Tests compare downloaded bytes exactly against the seeded source payload. |
| AC4 - invalid source skipped | At least one scenario includes an invalid/unusable source and still completes via valid servers. |
| AC5 - existing protection remains | Current `tests/multi_source.rs` tests are preserved and still pass. |
| AC6 - correctness-first | No `>=80%` or equivalent speed assertion; no broad recovery matrix. |
| AC7 - server defaults preserved | Manual `test_server/server.py` behavior remains config-compatible by default. |

## Verification Commands

Run these from the repository root after the implementation lane is integrated:

```bash
rtk cargo fmt --check
rtk cargo check
rtk cargo test multi_source -- --nocapture
rtk cargo test -q
```

If the implementation changes Python server behavior, also run at least one manual smoke check:

```bash
rtk python test_server/server.py
```

Then request a small Range response from the server and confirm the process stops cleanly. The exact
port and URL may vary if the implementation adds CLI/env overrides.

## Current Baseline Notes

- The existing Rust multi-source tests cover lane capacity, lane blacklisting, invalid-source
  skipping, and two-source initial chunk fan-out with `mockito`.
- `Downloader::new_multi(...)` already routes through `MultiRuntime::from_config(...)`, validates
  available sources, then hands the runtime to `DownloadMonitor`.
- `DownloadMonitor` already records lane success/failure and builds split/retry requests through
  the lane-aware request path.
- `test_server/server.py` currently provides Range support and throttling, but its initial config
  loading is centered on `config.ini`. Phase 1 should avoid shared config mutation when spawning
  multiple instances.

## Reviewer Notes

The best Phase 1 outcome is a small, deterministic test harness that raises confidence in the real
multi-source runtime while preserving the fast mock/unit tests. If a proposed change adds public API
telemetry, broad scheduler policy, or performance acceptance thresholds, treat it as Phase 2 scope
unless the leader explicitly widens the task.
