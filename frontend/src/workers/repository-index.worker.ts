import {
  queryRepositories,
  type IndexedRepository,
  type RepositoryFilter,
  type RepositorySort,
} from './repository-index';

type Request =
  | { type: 'replace'; repositories: IndexedRepository[] }
  | {
      type: 'query';
      requestId: number;
      query: string;
      filter: RepositoryFilter;
      sort: RepositorySort;
    };

let repositories: IndexedRepository[] = [];

self.onmessage = (message: MessageEvent<Request>) => {
  const request = message.data;
  if (request.type === 'replace') {
    repositories = request.repositories;
    return;
  }

  self.postMessage({
    type: 'result',
    requestId: request.requestId,
    repoIds: queryRepositories(repositories, request),
  });
};

export {};
