import { useSyncExternalStore } from 'react';
import type { FetchReadiness, Operation } from '../api/contracts';

type Listener = () => void;
let activeOperation: Operation | null = null;
let fetchReadiness: FetchReadiness = {
  batchId: null,
  succeeded: 0,
  failed: 0,
  ready: false,
  expiresAt: null,
};
let expiryTimer: ReturnType<typeof setTimeout> | null = null;
const listeners = new Set<Listener>();

const notify = (): void => {
  for (const listener of listeners) listener();
};

const replaceFetchReadiness = (readiness: FetchReadiness): void => {
  if (expiryTimer !== null) clearTimeout(expiryTimer);
  const expiresAt = readiness.expiresAt === null ? Number.NaN : Date.parse(readiness.expiresAt);
  const remaining = expiresAt - Date.now();
  fetchReadiness = readiness.ready && Number.isFinite(remaining) && remaining <= 0
    ? { ...readiness, ready: false }
    : readiness;
  if (fetchReadiness.ready && Number.isFinite(remaining)) {
    expiryTimer = setTimeout(() => {
      fetchReadiness = { ...fetchReadiness, ready: false };
      expiryTimer = null;
      notify();
    }, Math.min(remaining, 2_147_483_647));
  } else {
    expiryTimer = null;
  }
};

export const operationStore = {
  getSnapshot: (): Operation | null => activeOperation,
  getFetchReadinessSnapshot: (): FetchReadiness => fetchReadiness,
  subscribe(listener: Listener): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  set(operation: Operation | null): void {
    activeOperation = operation;
    if (operation?.kind === 'fetch') {
      if (operation.state === 'queued' || operation.state === 'running') {
        replaceFetchReadiness({
          batchId: null,
          succeeded: 0,
          failed: 0,
          ready: false,
          expiresAt: null,
        });
      } else if (operation.state === 'succeeded' || operation.state === 'partial_failed') {
        const finishedAt = operation.finishedAt === null
          ? Number.NaN
          : Date.parse(operation.finishedAt);
        const expiresAt = Number.isFinite(finishedAt)
          ? new Date(finishedAt + 30 * 60 * 1_000).toISOString()
          : null;
        replaceFetchReadiness({
          batchId: operation.operationId,
          succeeded: operation.counters.succeeded,
          failed: operation.counters.failed,
          ready: operation.counters.succeeded > 0 && expiresAt !== null,
          expiresAt,
        });
      }
    }
    notify();
  },
  setFetchReadiness(readiness: FetchReadiness): void {
    replaceFetchReadiness(readiness);
    notify();
  },
};

export function useActiveOperation(): Operation | null {
  return useSyncExternalStore(operationStore.subscribe, operationStore.getSnapshot);
}

export function useFetchReadiness(): FetchReadiness {
  return useSyncExternalStore(
    operationStore.subscribe,
    operationStore.getFetchReadinessSnapshot,
  );
}
