# devflow Unified Environment Roadmap

## Objective

Build devflow into a service-first environment orchestrator that can branch, switch, and clean up full developer environments (VCS, databases, caches, containers, and plugins) with deterministic automation semantics.

The target is "Worktrunk + first-class services + CoW policy engine", with room to integrate proxy-style workflows from `devproxy` as a native service capability.

## Product Direction

devflow should be:

1. **Environment-centric**: a branch is an environment, not just a VCS ref.
2. **Capability-driven**: every provider advertises what it can do (CoW, reset, logs, seed, destroy, snapshots).
3. **Deterministic for automation**: JSON-first, explicit failure modes, non-interactive guarantees.
4. **Fast by default**: CoW where available, predictable fallback where not.
5. **Provider-agnostic**: local, cloud, and plugin backends should follow one lifecycle contract.

## Architectural Pillars

### 1) Unified Lifecycle Contract

Define a single lifecycle model for all services:

- `create`, `switch`, `delete`, `exists`, `list`
- optional: `start`, `stop`, `reset`, `seed`, `logs`, `destroy`
- structured `doctor` + `capabilities`

Acceptance criteria:

- All built-in providers return a capability descriptor.
- CLI/TUI use capability checks instead of provider-name branching.

### 2) CoW Policy Engine

Treat CoW as an execution policy, not provider-specific behavior.

- Inputs: OS, filesystem, provider capability, branch lineage, user overrides.
- Outputs: selected strategy (`apfs_clone`, `reflink`, `zfs_clone`, `copy`) + reason.
- Record decision in state for observability and retries.

Acceptance criteria:

- `devflow capabilities` reports selected CoW strategy and fallback path.
- Local providers and worktree creation share the same decision logic.

### 3) State as Source of Truth

Use local state to represent devflow branch identity independently from VCS HEAD.

- branch registry (name, parent, worktree path, created_at)
- active branch pointer
- service membership and health snapshots

Acceptance criteria:

- branch metadata survives branch switches and does not drift.
- CLI and TUI use the same active branch semantics.

### 4) Orchestration Decomposition

Move command orchestration out of `src/cli.rs` into reusable application services:

- `SwitchUseCase`
- `RemoveUseCase`
- `DestroyProjectUseCase`
- `ListUseCase`

Acceptance criteria:

- CLI handlers become thin adapters.
- TUI background actions call the same use-case layer.

### 5) Extensibility and devproxy Convergence

Add a first-class model for runtime sidecars/proxies:

- service kind: `proxy`
- lifecycle hooks for routing/bootstrap
- per-branch endpoint publication in connection info

Acceptance criteria:

- proxy-like providers can be added without CLI changes.
- branch switch can atomically update data service + proxy service.

## Delivery Plan

## Phase 0 - Baseline Hardening (short)

Scope:

- stabilize branch registry persistence and active branch semantics
- ensure init/destroy/switch/remove flows preserve state correctly
- align TUI operations with state updates

Exit criteria:

- no metadata loss for existing branches during switch
- TUI and CLI display the same active branch in multi-worktree setups

## Phase 1 - Capability Contracts

Scope:

- formal provider capability schema
- add capability reporting to core providers
- update doctor/capabilities output to include provider-level capabilities

Exit criteria:

- feature checks are capability-based, not provider-name-based
- machine-readable capability matrix available in JSON mode

## Phase 2 - Shared Runtime for Local Providers

Scope:

- extract shared Docker/container lifecycle primitives
- unify branch naming, labels, and cleanup semantics across postgres/mysql/clickhouse/generic

Exit criteria:

- duplicated runtime code reduced substantially
- consistent status and logs behavior across local providers

## Phase 3 - Use-Case Layer Extraction

Scope:

- create orchestrator modules for switch/list/remove/destroy
- keep CLI/TUI as adapters + presentation

Exit criteria:

- orchestration logic no longer centralized in `src/cli.rs`
- TUI and CLI call same domain flows

## Phase 4 - CoW Policy Unification

Scope:

- unify CoW strategy selection for worktrees + local service data
- capture policy decisions and fallback reasons in status/capabilities

Exit criteria:

- deterministic strategy selection in every environment
- clear operator visibility into fallback paths

## Phase 5 - Unified Dev Environment Platform

Scope:

- add proxy/runtime sidecar model (devproxy-style integration)
- support multi-service environment templates and composition
- expose environment graph in CLI/TUI/JSON

Exit criteria:

- branch == environment is fully represented and automatable
- plugin/provider ecosystem can extend environment graph safely

## First Implementation Slices (recommended PR order)

1. **State consistency pass**
   - preserve branch metadata on switch
   - synchronize active branch updates across CLI and TUI
2. **Capability schema introduction**
   - add provider capability descriptor types
   - expose in `doctor` and `capabilities`
3. **Switch use-case extraction**
   - extract switch orchestration from `cli.rs`
   - route both CLI and TUI through shared flow

## Risks and Mitigations

- **Risk**: drift between VCS and local state.
  - **Mitigation**: reconcile on startup and after every lifecycle mutation.
- **Risk**: provider behavior inconsistency.
  - **Mitigation**: capability conformance checks + shared runtime layer.
- **Risk**: refactor stalls due broad scope.
  - **Mitigation**: phase gates with explicit acceptance criteria and small PR slices.

## Success Metrics

- switch/create/remove median latency (local CoW and fallback paths)
- orchestration success rate with partial-failure diagnostics
- number of provider-specific conditionals in CLI/TUI (should trend down)
- branch-state reconciliation warnings per week (should trend to near zero)
