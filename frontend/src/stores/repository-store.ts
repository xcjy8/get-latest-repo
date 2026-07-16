import { useSyncExternalStore } from 'react';
import type { RepositoryPatch, RepositorySummary, Statistics } from '../api/contracts';

type Listener = () => void;

const emptyStatistics: Statistics = {
  total: 0,
  hasUpdates: 0,
  synced: 0,
  unreachable: 0,
  noRemote: 0,
  dirty: 0,
};

class RepositoryStore {
  readonly #entities = new Map<string, RepositorySummary>();
  readonly #entityListeners = new Map<string, Set<Listener>>();
  readonly #orderListeners = new Set<Listener>();
  readonly #statisticsListeners = new Set<Listener>();
  #order: readonly string[] = [];
  #statistics: Statistics = emptyStatistics;

  replace(repositories: readonly RepositorySummary[], statistics: Statistics): void {
    this.#entities.clear();
    for (const repository of repositories) {
      this.#entities.set(repository.repoId, repository);
    }
    this.#order = repositories.map((repository) => repository.repoId);
    this.#statistics = statistics;
    this.#emit(this.#orderListeners);
    this.#emit(this.#statisticsListeners);
    for (const listeners of this.#entityListeners.values()) this.#emit(listeners);
  }

  applyPatch(patch: RepositoryPatch): void {
    let orderChanged = false;
    const changedIds = new Set<string>();
    for (const repository of patch.upserts) {
      const current = this.#entities.get(repository.repoId);
      // 断线重放可能携带旧实体；版本只能单调前进，禁止旧事件覆盖新快照。
      if (current !== undefined && current.entityVersion >= repository.entityVersion) continue;
      if (current === undefined) orderChanged = true;
      this.#entities.set(repository.repoId, repository);
      changedIds.add(repository.repoId);
    }
    for (const repoId of patch.removes) {
      if (this.#entities.delete(repoId)) {
        orderChanged = true;
        changedIds.add(repoId);
      }
    }
    if (orderChanged) {
      this.#order = [...this.#entities.keys()];
      this.#emit(this.#orderListeners);
    }
    for (const repoId of changedIds) {
      const listeners = this.#entityListeners.get(repoId);
      if (listeners !== undefined) this.#emit(listeners);
    }
  }

  replaceStatistics(statistics: Statistics): void {
    this.#statistics = statistics;
    this.#emit(this.#statisticsListeners);
  }

  getRepository = (repoId: string): RepositorySummary | undefined =>
    this.#entities.get(repoId);

  getOrder = (): readonly string[] => this.#order;

  getStatistics = (): Statistics => this.#statistics;

  subscribeRepository(repoId: string, listener: Listener): () => void {
    let listeners = this.#entityListeners.get(repoId);
    if (listeners === undefined) {
      listeners = new Set();
      this.#entityListeners.set(repoId, listeners);
    }
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
      if (listeners.size === 0) this.#entityListeners.delete(repoId);
    };
  }

  subscribeOrder = (listener: Listener): (() => void) => {
    this.#orderListeners.add(listener);
    return () => this.#orderListeners.delete(listener);
  };

  subscribeStatistics = (listener: Listener): (() => void) => {
    this.#statisticsListeners.add(listener);
    return () => this.#statisticsListeners.delete(listener);
  };

  #emit(listeners: Set<Listener>): void {
    for (const listener of listeners) listener();
  }
}

export const repositoryStore = new RepositoryStore();

export function useRepository(repoId: string): RepositorySummary | undefined {
  return useSyncExternalStore(
    (listener) => repositoryStore.subscribeRepository(repoId, listener),
    () => repositoryStore.getRepository(repoId),
  );
}

export function useRepositoryOrder(): readonly string[] {
  return useSyncExternalStore(repositoryStore.subscribeOrder, repositoryStore.getOrder);
}

export function useStatistics(): Statistics {
  return useSyncExternalStore(
    repositoryStore.subscribeStatistics,
    repositoryStore.getStatistics,
  );
}
