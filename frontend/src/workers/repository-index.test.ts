import { describe, expect, it } from 'vitest';
import {
  queryRepositories,
  type IndexedRepository,
  type RepositoryFilter,
} from './repository-index';

const repositories: IndexedRepository[] = [
  { repoId: 'updates', name: 'Alpha', path: '/repos/alpha', branch: 'main', freshness: 'has_updates', dirty: false, behindCount: 3 },
  { repoId: 'dirty', name: 'Beta', path: '/repos/beta', branch: 'feature/local', freshness: 'synced', dirty: true, behindCount: 0 },
  { repoId: 'synced', name: 'Gamma', path: '/repos/gamma', branch: 'main', freshness: 'synced', dirty: false, behindCount: 0 },
  { repoId: 'unreachable', name: 'Delta', path: '/repos/delta', branch: 'main', freshness: 'unreachable', dirty: false, behindCount: 0 },
  { repoId: 'no-remote', name: 'Epsilon', path: '/repos/epsilon', branch: null, freshness: 'no_remote', dirty: false, behindCount: 0 },
];

describe('仓库筛选索引', () => {
  const cases: Array<[RepositoryFilter, string[]]> = [
    ['all', ['updates', 'dirty', 'unreachable', 'no-remote', 'synced']],
    ['has_updates', ['updates']],
    ['dirty', ['dirty']],
    ['synced', ['dirty', 'synced']],
    ['unreachable', ['unreachable']],
    ['no_remote', ['no-remote']],
  ];

  it.each(cases)('%s 筛选只返回匹配仓库', (filter, expected) => {
    expect(queryRepositories(repositories, { query: '', filter, sort: 'name' })).toEqual(expected);
  });

  it('搜索同时覆盖名称、路径与分支', () => {
    expect(queryRepositories(repositories, { query: 'FEATURE/LOCAL', filter: 'all', sort: 'name' }))
      .toEqual(['dirty']);
  });

  it('状态排序优先展示待处理仓库', () => {
    expect(queryRepositories(repositories, { query: '', filter: 'all', sort: 'status' }))
      .toEqual(['updates', 'unreachable', 'no-remote', 'dirty', 'synced']);
  });
});
