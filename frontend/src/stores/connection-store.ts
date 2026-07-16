import { useSyncExternalStore } from 'react';

export type ConnectionState = 'connecting' | 'live' | 'stale' | 'resyncing' | 'offline';
type Listener = () => void;
let state: ConnectionState = 'connecting';
const listeners = new Set<Listener>();

export const connectionStore = {
  getSnapshot: (): ConnectionState => state,
  subscribe(listener: Listener): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  set(next: ConnectionState): void {
    if (next === state) return;
    state = next;
    for (const listener of listeners) listener();
  },
};

export function useConnectionState(): ConnectionState {
  return useSyncExternalStore(connectionStore.subscribe, connectionStore.getSnapshot);
}
