# Architecture Verification Result

Status: Rejected

Recommendation summary:
The additive multi-source entrypoint compiles and preserves the existing single-source path shape, but it is not architecturally complete as a multi-source downloader. The current integration validates/probes multiple lanes, then discards the MultiRuntime and runs all chunk, split, and retry requests through one primary lane only.

Architecture problem:
The new lane/scheduler boundary is not connected to the existing monitor/chunk orchestration boundary. This creates a public multi-source API whose runtime behavior is effectively single-source after probing, so the added source/proxy model is mostly dormant state rather than an active execution strategy.

Tradeoffs:
- Option A, current additive wrapper: low implementation cost and low regression risk to single-source behavior, but high future change cost because public lane APIs imply behavior that the downloader does not perform.
- Option B, integrate MultiRuntime into DownloadMonitor/chunk creation: higher implementation and test cost, but correct ownership of source/proxy scheduling, retries, and failover.

Major risks:
1. Multi-source semantics are misleading: Downloader::new_multi selects primary_lane once and passes a single client/url to orchestrate_downloads; monitor bisection and retry paths also reuse that same client/url.
2. Failover and blacklist behavior are not wired to download failures. LaneScheduler records failures/successes, but no ChunkFailed/DownloadComplete path maps chunks to lanes or calls record_failure/record_success/release_chunk.
3. PerSource + proxied sources can collapse duplicate lane IDs in MultiRuntime.runtimes HashMap, while candidates can contain duplicate lane IDs, making source/proxy accounting ambiguous.
4. Probe behavior is sequential and only checks matching Content-Length plus range support; no checksum/ETag/Last-Modified/content identity validation exists for mirror equivalence.

Invariants preserved:
- Existing Downloader::new single-source entrypoint and public run shape remain intact.
- Single writer task remains the only file mutation path.
- Existing range-download monitor/chunk architecture is not structurally disrupted.
- NoAvailableSources is explicit when no range-capable valid source is found.

Invariants broken or not yet established:
- New multi-source API does not preserve the expected invariant that chunks may be assigned across available sources/proxies.
- Scheduler active capacity/health invariants are isolated unit behavior, not an invariant of Downloader::new_multi execution.
- Blacklisting invariant is misleading because best_lane falls back to blacklisted lanes when all healthy lanes are unavailable; that may be intentional emergency fallback, but the test name says blacklisted until released and there is no end-to-end release/failure loop.

Validation steps run:
- rtk cargo test --test multi_source: 3 passed.
- rtk cargo test: 24 passed, 1 ignored.

Missing verification that still matters:
- Multi-worker end-to-end test proving chunks are served by more than one source/proxy.
- Failure-after-probe test proving a bad primary lane fails over to another valid lane.
- Duplicate lane-id/proxy behavior test for LaneModel::PerSource with multiple proxies.
- Mismatched mirror-content identity validation beyond equal Content-Length.
- Documentation/API contract test or example clarifying whether this is only probe fallback or true additive multi-source scheduling.

Artifacts created:
- .agents/results/result-architecture.md
- .agents/results/architecture/multi-source-entrypoint-verification.md
