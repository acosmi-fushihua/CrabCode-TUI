import { registerCleanupFinalizer } from "../../utils/cleanupRegistry.js";
import { getMainLoopModel } from "../../utils/model/model.js";
import {
  ACCOUNT_BRIDGE_REFERENCE_PREFIX,
  parseAccountBridgeReference,
} from "../../utils/model/accountBridgeReference.js";
import {
  parseAccountBridgeRuntimeAccess,
  type AccountBridgeRuntimeAccess,
} from "./types.js";
import { isShuttingDown } from "../../utils/gracefulShutdown.js";

export type DirectAccountBridgeTurnErrorCode =
  | "invalid-account-route-reference"
  | "account-route-access-mismatch"
  | "account-route-capability-denied"
  | "runtime-process-shutting-down";

export class DirectAccountBridgeTurnError extends Error {
  constructor(public readonly code: DirectAccountBridgeTurnErrorCode) {
    super(code);
    this.name = "DirectAccountBridgeTurnError";
  }
}

export interface DirectAccountBridgeTurnAccessDeps {
  resolveDefaultModel(): string | Promise<string>;
  turnAccess(routeId: string): Promise<unknown>;
  isProcessShuttingDown?(): boolean;
}

export interface DirectAccountBridgeTurnAccess {
  /**
   * Exact model passed into QueryEngine. A settings-resolved account route is
   * pinned for this turn; an ordinary omitted model remains omitted.
   */
  modelForQuery: string | undefined;
  runtimeAccess: AccountBridgeRuntimeAccess | undefined;
  /**
   * Mark the caller's synchronous turn scope complete. This deliberately does
   * not mutate the capability object: QueryEngine may have handed the same
   * object to detached background work before its foreground generator
   * finishes. The process-owned manager remains authoritative and is stopped
   * by the cleanup registry, which clears runtime keys and files.
   */
  release(): void;
}

let runtimeModulePromise:
  | Promise<typeof import("./runtimeManager.js")>
  | undefined;
let runtimeCleanupRegistered = false;
let unregisterRuntimeCleanup: (() => void) | undefined;

function loadRuntimeManager(): Promise<typeof import("./runtimeManager.js")> {
  runtimeModulePromise ??= import("./runtimeManager.js");
  if (!runtimeCleanupRegistered) {
    runtimeCleanupRegistered = true;
    unregisterRuntimeCleanup = registerCleanupFinalizer(async () => {
      // Registration happens only after an account route requests access, so
      // awaiting this promise can never create a manager for an ordinary model.
      const runtime = await runtimeModulePromise!;
      runtime.beginAccountBridgeManagerProcessShutdown();
      await runtime.shutdownAccountBridgeManagerForProcess();
    });
  }
  return runtimeModulePromise;
}

const productionDeps: DirectAccountBridgeTurnAccessDeps = {
  resolveDefaultModel: getMainLoopModel,
  isProcessShuttingDown: isShuttingDown,
  async turnAccess(routeId) {
    const runtime = await loadRuntimeManager();
    return runtime.getAccountBridgeManager().turnAccess(routeId);
  },
};

/**
 * Resolve and validate the exact private Account Bridge capability immediately
 * before one QueryEngine turn. No failure is converted to a gateway fallback:
 * manager errors (eligibility, lifecycle, lock, account, quota, artifact)
 * retain their original typed error/code.
 */
export async function acquireDirectAccountBridgeTurnAccess(
  requestedModel: string | null | undefined,
  deps: DirectAccountBridgeTurnAccessDeps = productionDeps,
): Promise<DirectAccountBridgeTurnAccess> {
  // Once graceful shutdown has captured whether a provider finalizer exists,
  // a new process-owned sidecar may not appear behind that decision. If a
  // manager was already requested, loadRuntimeManager registered its finalizer
  // synchronously before the first dynamic-import await.
  if (deps.isProcessShuttingDown?.()) {
    throw new DirectAccountBridgeTurnError(
      "runtime-process-shutting-down",
    );
  }
  const effectiveModel =
    requestedModel === null || requestedModel === undefined
      ? await deps.resolveDefaultModel()
      : requestedModel;
  // Default-model resolution may be asynchronous. Shutdown can begin while
  // it is pending, before a provider finalizer exists. Recheck after that
  // await so a sidecar cannot appear behind gracefulShutdown's already
  // captured "no provider" decision.
  if (deps.isProcessShuttingDown?.()) {
    throw new DirectAccountBridgeTurnError(
      "runtime-process-shutting-down",
    );
  }
  const trimmedModel = effectiveModel.trim();
  const routeId = parseAccountBridgeReference(trimmedModel);
  if (
    trimmedModel.startsWith(ACCOUNT_BRIDGE_REFERENCE_PREFIX) &&
    routeId === null
  ) {
    throw new DirectAccountBridgeTurnError(
      "invalid-account-route-reference",
    );
  }
  if (routeId === null) {
    return {
      modelForQuery: requestedModel ?? undefined,
      runtimeAccess: undefined,
      release() {},
    };
  }

  const runtimeAccess = parseAccountBridgeRuntimeAccess(
    await deps.turnAccess(routeId),
  );
  if (runtimeAccess.route.routeId !== routeId) {
    throw new DirectAccountBridgeTurnError(
      "account-route-access-mismatch",
    );
  }
  if (
    runtimeAccess.route.chatRuntimeSupported !== true ||
    runtimeAccess.route.supportsTools !== true
  ) {
    throw new DirectAccountBridgeTurnError(
      "account-route-capability-denied",
    );
  }

  return {
    modelForQuery: trimmedModel,
    runtimeAccess,
    // JavaScript strings cannot be cryptographically zeroed, and mutating the
    // shared object would revoke detached work prematurely. Authoritative
    // teardown belongs to the process-owned manager finalizer.
    release() {},
  };
}

/** Test-only reset for module-local lazy-load bookkeeping. */
export function _resetDirectAccountBridgeRuntimeForTest(): void {
  unregisterRuntimeCleanup?.();
  unregisterRuntimeCleanup = undefined;
  runtimeModulePromise = undefined;
  runtimeCleanupRegistered = false;
}
