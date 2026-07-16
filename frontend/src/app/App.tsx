import { useCallback, useEffect, useState } from 'react';
import { loadBootstrap } from '../api/client';
import { connectEventStream } from '../api/events';
import { OperationBar } from '../components/OperationBar';
import { ConfigPanel } from '../components/ConfigPanel';
import { StatisticsGrid } from '../components/StatisticsGrid';
import { RepositoryTable } from '../features/repositories/RepositoryTable';
import { connectionStore, useConnectionState } from '../stores/connection-store';
import { operationStore, useFetchReadiness } from '../stores/operation-store';
import { repositoryStore } from '../stores/repository-store';

const connectionLabels = {
  connecting: '连接中',
  live: '实时',
  stale: '连接不稳定',
  resyncing: '重新同步',
  offline: '离线',
} as const;

export function App() {
  const [version, setVersion] = useState('');
  const [initializing, setInitializing] = useState(true);
  const [fatalError, setFatalError] = useState<string | null>(null);
  const connection = useConnectionState();
  const fetchReadiness = useFetchReadiness();

  const refresh = useCallback(async (): Promise<string | null> => {
    connectionStore.set('resyncing');
    try {
      const bootstrap = await loadBootstrap();
      repositoryStore.replace(bootstrap.repositories, bootstrap.statistics);
      operationStore.set(bootstrap.activeOperation);
      operationStore.setFetchReadiness(bootstrap.fetchReadiness);
      setVersion(bootstrap.version);
      setFatalError(null);
      connectionStore.set('live');
      return bootstrap.eventSequence;
    } catch (reason) {
      connectionStore.set('offline');
      setFatalError(reason instanceof Error ? reason.message : '控制台初始化失败');
      return null;
    } finally {
      setInitializing(false);
    }
  }, []);

  useEffect(() => {
    let active = true;
    let disconnect = (): void => undefined;
    const connect = async (): Promise<void> => {
      disconnect();
      const afterSequence = await refresh();
      if (!active || afterSequence === null) return;
      disconnect = connectEventStream(afterSequence, () => void connect());
    };
    void connect();
    return () => {
      active = false;
      disconnect();
    };
  }, [refresh]);

  if (initializing) {
    return (
      <main className="center-state">
        <div className="brand-mark" aria-hidden="true">G</div>
        <p>正在建立本地实时连接……</p>
      </main>
    );
  }

  if (fatalError !== null) {
    return (
      <main className="center-state error-state">
        <div className="brand-mark" aria-hidden="true">!</div>
        <h1>无法打开控制台</h1>
        <p>{fatalError}</p>
        <button className="button primary" onClick={() => globalThis.location.reload()}>重新连接</button>
      </main>
    );
  }

  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="brand">
          <div className="brand-mark" aria-hidden="true">G</div>
          <div>
            <strong>GetLatestRepo</strong>
            <span>本地仓库控制台</span>
          </div>
        </div>
        <div className="topbar-meta">
          <span className={`connection-state ${connection}`}>
            <i aria-hidden="true" />
            {connectionLabels[connection]}
          </span>
          <span className="version">v{version}</span>
        </div>
      </header>

      <main className="workspace">
        <section className="overview-strip" aria-label="仓库概览">
          <div className="workspace-title">
            <h1>仓库信息流</h1>
            <span>{fetchReadiness.ready ? '远程已校准' : '本机快照'}</span>
          </div>
          <StatisticsGrid />
          <ConfigPanel />
        </section>
        <OperationBar />
        <RepositoryTable />
      </main>
    </div>
  );
}
