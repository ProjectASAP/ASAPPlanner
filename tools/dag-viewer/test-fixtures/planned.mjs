// The Node tests' stage documents, written by the built asap_planner through
// Stage 3 (`planned`); `planner` is null when it is not built, and the tests
// that need a document are skipped. CARGO_TARGET_DIR is honored.
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = fileURLToPath(new URL('../../../', import.meta.url));
const binary = join(process.env.CARGO_TARGET_DIR || join(repo, 'target'), 'debug', 'asap_planner');
export const planner = existsSync(binary) ? binary : null;

// The document for the PlanningWorkload file `workload` (relative to
// tools/dag-viewer), under `capabilities` (null: the defaults).
export function planned(workload, capabilities = null) {
  const dir = mkdtempSync(join(tmpdir(), 'asap-viewer-'));
  try {
    const out = join(dir, 'doc.json');
    const args = ['--workload', fileURLToPath(new URL(`../${workload}`, import.meta.url)),
      '--run-until', 'stage3', '--format', 'json', '--out', out];
    if (capabilities) {
      writeFileSync(join(dir, 'capabilities.json'), JSON.stringify(capabilities));
      args.push('--deployment-capabilities', join(dir, 'capabilities.json'));
    }
    execFileSync(planner, args, { stdio: ['ignore', 'ignore', 'pipe'] });
    return JSON.parse(readFileSync(out, 'utf8'));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}
