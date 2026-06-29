import { toast } from "./notify";
import type { OrchestrationResult, HookRunResult, ProcessResult } from "../types";

interface WorkspaceOpResult {
  services?: OrchestrationResult[];
  hooks?: HookRunResult[];
  processes?: ProcessResult[];
  worktree_path?: string | null;
}

/**
 * Turn the rich result of a create/switch/delete into a single, honest toast.
 *
 * Previously the GUI discarded these objects entirely, so a workspace whose
 * service provisioning or post-create hooks failed still showed as a success.
 */
export function reportWorkspaceResult(action: string, result: WorkspaceOpResult) {
  const failedServices = (result.services ?? []).filter((s) => !s.success);
  const failedProcesses = (result.processes ?? []).filter((p) => !p.success && p.required);
  const optionalProcessIssues = (result.processes ?? []).filter((p) => !p.success && !p.required);

  const hookErrors: string[] = [];
  let hooksFailed = 0;
  let hooksSkipped = 0;
  for (const h of result.hooks ?? []) {
    hooksFailed += h.failed;
    hooksSkipped += h.skipped;
    hookErrors.push(...h.errors);
  }

  const problems: string[] = [];
  for (const s of failedServices) {
    problems.push(`service "${s.service_name}": ${s.message}`);
  }
  for (const p of failedProcesses) {
    problems.push(`process "${p.process}": ${p.message}`);
  }
  for (const p of optionalProcessIssues) {
    problems.push(`optional process "${p.process}": ${p.message}`);
  }
  if (hooksFailed > 0) {
    problems.push(
      `${hooksFailed} hook${hooksFailed > 1 ? "s" : ""} failed${
        hookErrors.length ? `: ${hookErrors.slice(0, 3).join("; ")}` : ""
      }`,
    );
  }
  if (hooksSkipped > 0) {
    problems.push(
      `${hooksSkipped} hook${hooksSkipped > 1 ? "s" : ""} skipped (needs approval — run from a terminal once, or set DEVFLOW_APPROVE_HOOKS=1)`,
    );
  }

  if (problems.length > 0) {
    toast.warning(problems.join("\n"), {
      title: `${action} completed with issues`,
      duration: 0,
    });
  } else {
    toast.success(`${action} succeeded`);
  }
}
