export type RepositoryFilter =
  | 'all'
  | 'has_updates'
  | 'synced'
  | 'unreachable'
  | 'no_remote'
  | 'dirty';

export type RepositorySort = 'name' | 'status' | 'behind';

export type IndexedRepository = {
  repoId: string;
  name: string;
  path: string;
  branch: string | null;
  freshness: 'has_updates' | 'synced' | 'unreachable' | 'no_remote';
  dirty: boolean;
  behindCount: number;
};

type RepositoryQuery = {
  query: string;
  filter: RepositoryFilter;
  sort: RepositorySort;
};

const statusOrder: Record<IndexedRepository['freshness'], number> = {
  has_updates: 0,
  unreachable: 1,
  no_remote: 2,
  synced: 3,
};

/** 在 Worker 内完成搜索、筛选与排序，主线程只接收当前结果 ID。 */
export function queryRepositories(
  repositories: readonly IndexedRepository[],
  request: RepositoryQuery,
): string[] {
  const needle = request.query.trim().toLocaleLowerCase('zh-CN');
  const filtered = repositories.filter((repository) => {
    if (request.filter === 'dirty' && !repository.dirty) return false;
    if (
      request.filter !== 'all' &&
      request.filter !== 'dirty' &&
      repository.freshness !== request.filter
    ) {
      return false;
    }
    if (needle.length === 0) return true;
    return `${repository.name}\n${repository.path}\n${repository.branch ?? ''}`
      .toLocaleLowerCase('zh-CN')
      .includes(needle);
  });

  filtered.sort((left, right) => {
    if (request.sort === 'behind') {
      return right.behindCount - left.behindCount ||
        left.name.localeCompare(right.name, 'zh-CN');
    }
    if (request.sort === 'status') {
      return statusOrder[left.freshness] - statusOrder[right.freshness] ||
        left.name.localeCompare(right.name, 'zh-CN');
    }
    return left.name.localeCompare(right.name, 'zh-CN');
  });

  return filtered.map((repository) => repository.repoId);
}
