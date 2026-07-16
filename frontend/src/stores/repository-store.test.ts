import { describe, expect, it, vi } from 'vitest';
import type { RepositorySummary } from '../api/contracts';
import { repositoryStore } from './repository-store';

const repository = (repoId: string, entityVersion: number): RepositorySummary => ({
  repoId,
  entityVersion,
  name: repoId,
  path: `/repos/${repoId}`,
  branch: 'main',
  dirty: false,
  aheadCount: 0,
  behindCount: 0,
  freshness: 'synced',
  lastCommitAt: null,
  lastCommitMessage: null,
  lastFetchAt: null,
  lastPullAt: null,
});

describe('repositoryStore', () => {
  it('只通知发生变化的仓库订阅者', () => {
    repositoryStore.replace([repository('a', 1), repository('b', 1)], {
      total: 2,
      hasUpdates: 0,
      synced: 2,
      unreachable: 0,
      noRemote: 0,
      dirty: 0,
    });
    const listenerA = vi.fn();
    const listenerB = vi.fn();
    const unsubscribeA = repositoryStore.subscribeRepository('a', listenerA);
    const unsubscribeB = repositoryStore.subscribeRepository('b', listenerB);

    repositoryStore.applyPatch({ upserts: [repository('a', 2)], removes: [] });

    expect(listenerA).toHaveBeenCalledOnce();
    expect(listenerB).not.toHaveBeenCalled();
    unsubscribeA();
    unsubscribeB();
  });

  it('忽略重复实体版本', () => {
    repositoryStore.replace([repository('a', 3)], {
      total: 1,
      hasUpdates: 0,
      synced: 1,
      unreachable: 0,
      noRemote: 0,
      dirty: 0,
    });
    const listener = vi.fn();
    const unsubscribe = repositoryStore.subscribeRepository('a', listener);

    repositoryStore.applyPatch({ upserts: [repository('a', 3)], removes: [] });

    expect(listener).not.toHaveBeenCalled();
    unsubscribe();
  });

  it('拒绝旧事件覆盖新快照', () => {
    repositoryStore.replace([repository('a', 3)], {
      total: 1,
      hasUpdates: 0,
      synced: 1,
      unreachable: 0,
      noRemote: 0,
      dirty: 0,
    });
    const listener = vi.fn();
    const unsubscribe = repositoryStore.subscribeRepository('a', listener);

    repositoryStore.applyPatch({
      upserts: [{ ...repository('a', 2), name: '旧名称' }],
      removes: [],
    });

    expect(repositoryStore.getRepository('a')?.name).toBe('a');
    expect(listener).not.toHaveBeenCalled();
    unsubscribe();
  });
});
