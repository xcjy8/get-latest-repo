import { useDeferredValue, useEffect, useRef, useState } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import {
  repositoryStore,
  useRepositoryOrder,
  useStatistics,
} from '../../stores/repository-store';
import { RepositoryRow } from './RepositoryRow';
import type {
  RepositoryFilter as Filter,
  RepositorySort as Sort,
} from '../../workers/repository-index';

const PAGE_SIZE = 15;

export function RepositoryTable() {
  const order = useRepositoryOrder();
  const statistics = useStatistics();
  const [query, setQuery] = useState('');
  const deferredQuery = useDeferredValue(query);
  const [filter, setFilter] = useState<Filter>('all');
  const [sort, setSort] = useState<Sort>('name');
  const [visibleIds, setVisibleIds] = useState<readonly string[]>(order);
  const [page, setPage] = useState(1);
  const parentRef = useRef<HTMLDivElement>(null);
  const workerRef = useRef<Worker | null>(null);
  const requestIdRef = useRef(0);

  useEffect(() => {
    const worker = new Worker(
      new URL('../../workers/repository-index.worker.ts', import.meta.url),
      { type: 'module' },
    );
    workerRef.current = worker;
    worker.onmessage = (message: MessageEvent<{ requestId: number; repoIds: string[] }>) => {
      if (message.data.requestId === requestIdRef.current) {
        setVisibleIds(message.data.repoIds);
        setPage((current) => Math.min(current, Math.max(1, Math.ceil(message.data.repoIds.length / PAGE_SIZE))));
      }
    };
    return () => worker.terminate();
  }, []);

  useEffect(() => {
    const repositories = order.flatMap((repoId) => {
      const repository = repositoryStore.getRepository(repoId);
      return repository === undefined
        ? []
        : [{
            repoId,
            name: repository.name,
            path: repository.path,
            branch: repository.branch,
            freshness: repository.freshness,
            dirty: repository.dirty,
            behindCount: repository.behindCount,
          }];
    });
    workerRef.current?.postMessage({ type: 'replace', repositories });
    const requestId = ++requestIdRef.current;
    workerRef.current?.postMessage({
      type: 'query',
      requestId,
      query: deferredQuery,
      filter,
      sort,
    });
  }, [order, statistics, deferredQuery, filter, sort]);

  const pageCount = Math.max(1, Math.ceil(visibleIds.length / PAGE_SIZE));
  const pageStart = (page - 1) * PAGE_SIZE;
  const pageIds = visibleIds.slice(pageStart, pageStart + PAGE_SIZE);
  const pageEnd = Math.min(pageStart + PAGE_SIZE, visibleIds.length);

  const virtualizer = useVirtualizer({
    count: pageIds.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 42,
    overscan: 5,
  });

  const filters: Array<[Filter, string, number]> = [
    ['all', '全部', statistics.total],
    ['has_updates', '待更新', statistics.hasUpdates],
    ['dirty', '本地修改', statistics.dirty],
    ['synced', '已同步', statistics.synced],
    ['unreachable', '远程异常', statistics.unreachable],
    ['no_remote', '无远程', statistics.noRemote],
  ];

  const selectFilter = (next: Filter): void => {
    setFilter(next);
    setPage(1);
    parentRef.current?.scrollTo({ top: 0 });
  };

  const selectPage = (next: number): void => {
    setPage(Math.min(pageCount, Math.max(1, next)));
    parentRef.current?.scrollTo({ top: 0 });
  };

  return (
    <section className="repository-panel" aria-labelledby="repository-title">
      <div className="repository-toolbar">
        <div className="repository-title">
          <h2 id="repository-title">仓库</h2>
          <span>{visibleIds.length}</span>
        </div>
        <div className="filter-tags" aria-label="仓库状态筛选">
          {filters.map(([value, label, count]) => (
            <button
              className={`filter-tag ${filter === value ? 'active' : ''}`}
              onClick={() => selectFilter(value)}
              aria-pressed={filter === value}
              key={value}
            >
              {label}<span>{count}</span>
            </button>
          ))}
        </div>
        <div className="filters" role="search">
          <label className="search-field">
            <span className="sr-only">搜索仓库</span>
            <input
              value={query}
              onChange={(event) => {
                setQuery(event.target.value);
                setPage(1);
              }}
              placeholder="搜索名称、路径或分支"
              spellCheck={false}
            />
          </label>
          <select
            value={sort}
            onChange={(event) => {
              setSort(event.target.value as Sort);
              setPage(1);
            }}
            aria-label="排序方式"
          >
            <option value="name">按名称</option>
            <option value="status">按状态</option>
            <option value="behind">按落后提交</option>
          </select>
        </div>
      </div>

      <div className="repository-grid" role="grid" aria-rowcount={pageIds.length + 1}>
        <div className="repository-header" role="row">
          <span role="columnheader">仓库</span>
          <span role="columnheader">分支</span>
          <span role="columnheader">状态</span>
          <span role="columnheader">同步</span>
          <span role="columnheader">工作区</span>
          <span role="columnheader">最近提交</span>
          <span role="columnheader">操作</span>
        </div>
        <div ref={parentRef} className="repository-viewport" tabIndex={0}>
          <div className="repository-canvas" style={{ height: virtualizer.getTotalSize() }}>
            {virtualizer.getVirtualItems().map((item) => {
              const repoId = pageIds[item.index];
              return repoId === undefined ? null : (
                <RepositoryRow
                  key={repoId}
                  repoId={repoId}
                  index={pageStart + item.index}
                  top={item.start}
                  height={item.size}
                />
              );
            })}
          </div>
        </div>
        <footer className="repository-footer">
          <span>
            {visibleIds.length === 0 ? '0 条结果' : `${pageStart + 1}–${pageEnd} / ${visibleIds.length}`}
          </span>
          <nav aria-label="仓库分页">
            <button
              disabled={page === 1}
              onClick={() => selectPage(page - 1)}
            >
              上一页
            </button>
            <b>{page} / {pageCount}</b>
            <button
              disabled={page === pageCount}
              onClick={() => selectPage(page + 1)}
            >
              下一页
            </button>
          </nav>
          <span>每页 {PAGE_SIZE} 条</span>
        </footer>
      </div>
    </section>
  );
}
