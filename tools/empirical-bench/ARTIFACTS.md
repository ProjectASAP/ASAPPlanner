# Archived experiment results

Full raw measurements and expanded query plans are distributed as an
[experiment artifact](https://github.com/ProjectASAP/ASAPPlanner/releases/tag/offline-evidence-2026-09-08),
not checked into the tool source tree. This prerelease is an experiment archive,
not a product release. The concise conclusions remain in
[the final report](../../docs/offline-o11y-final-2026-09-08.md).

Download and verify from the repository root:

```sh
gh release download offline-evidence-2026-09-08 --repo ProjectASAP/ASAPPlanner \
  --pattern offline-evidence-2026-09-08.tar.gz --dir /tmp
echo '180f3768203635c31188d19b500455290b96e19a4df4fdac7245af0944bd1b34  /tmp/offline-evidence-2026-09-08.tar.gz' | sha256sum --check
tar -xzf /tmp/offline-evidence-2026-09-08.tar.gz
```

Extract only into a checkout without existing result files; extraction restores
`tools/empirical-bench/results/` and `results-sweep/`. These generated directories
are ignored by Git. The archive is 460,710 bytes and contains 43 text files.
It preserves the original source snapshot
`152ca6cb71fc11a8c38d93e69c9fd7dca682f03b`, per-run commands, environment metadata,
raw timing/error reports, and measurement-source checksums.

- `results/`: historical partial frequency measurements and initial planner/control-plane replay; also the later `o11y-exact-snapshot.json` reference evaluation.
- `results-sweep/`: final frequency matrix, disjoint CPU and heap evidence, six recommendation scenarios, four frequency binding reports, and final o11y control-plane replay.
- Each directory contains `MEASUREMENTS.md` explaining its measurement scope. Do not mix costs from the two runs.

To produce new results instead of downloading these measurements, follow
[the benchmark guide](README.md) and [the replay guide](../../docs/user-guide/o11y-replay.md).
The archive preserves offline evidence, not production speedups or real-time
accuracy guarantees.
