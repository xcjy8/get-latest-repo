import { useState } from 'react';
import { cancelOperation, startOperation } from '../api/client';
import {
  operationStore,
  useActiveOperation,
  useFetchReadiness,
} from '../stores/operation-store';
import { useConnectionState } from '../stores/connection-store';
import { useDialog, type DialogDetailItem } from './DialogProvider';

const labels = {
  fetch: '获取远程状态',
  'pull-backup': '安全更新且备份',
} as const;

function readableIssue(kind: keyof typeof labels, detail: string): DialogDetailItem {
  const separator = detail.indexOf('：');
  const context = separator >= 0 ? detail.slice(0, separator).trim() : '';
  const technicalDetail = separator >= 0 ? detail.slice(separator + 1).trim() : detail.trim();
  const normalizedContext = context.replace(/^\.\.\//, '');
  const title = normalizedContext.split('/').filter(Boolean).at(-1) ?? '仓库操作';
  const lowerDetail = technicalDetail.toLowerCase();
  let summary = technicalDetail.split('\n')[0] ?? '操作未完成。';

  if (lowerDetail.includes('repository not found') || lowerDetail.includes('(404)')) {
    summary = '远程仓库不存在，或当前凭据没有访问权限。';
  } else if (lowerDetail.includes('authentication') || lowerDetail.includes('认证')) {
    summary = '远程仓库需要重新登录或授权。';
  } else if (
    lowerDetail.includes('network')
    || lowerDetail.includes('timed out')
    || lowerDetail.includes('could not resolve')
  ) {
    summary = '网络连接失败，请检查网络或代理设置。';
  } else if (kind === 'pull-backup' && lowerDetail.includes('子模块')) {
    summary = '父仓库已经更新，但其中一个子模块无法下载；父仓库结果已保留。';
  } else if (summary.length > 110) {
    summary = `${summary.slice(0, 107)}…`;
  }

  return {
    title,
    summary,
    ...(normalizedContext === '' ? {} : { context: normalizedContext }),
    ...(technicalDetail === summary ? {} : { technicalDetail }),
  };
}

export function OperationBar() {
  const operation = useActiveOperation();
  const readiness = useFetchReadiness();
  const dialog = useDialog();
  const connection = useConnectionState();
  const [error, setError] = useState<string | null>(null);
  const busy = operation?.state === 'queued' || operation?.state === 'running';
  const unsafeConnection = connection !== 'live';
  const remoteStateReady = readiness.ready;
  const updating = busy && operation?.kind === 'pull-backup';
  const secondStepSelected = remoteStateReady || updating;
  const issueCount = operation === null
    ? 0
    : operation.counters.failed + operation.counters.partial;
  const displayedIssueCount = issueCount > 0 ? issueCount : (operation?.details.length ?? 0);

  const start = async (kind: keyof typeof labels): Promise<void> => {
    if (kind === 'pull-backup') {
      const confirmed = await dialog.confirm({
        title: `更新 ${readiness.succeeded} 个已验证仓库`,
        message: readiness.failed > 0
          ? `远程检查已完成：${readiness.succeeded} 个成功，${readiness.failed} 个失败。失败仓库不会执行任何更新。`
          : `已准确检查 ${readiness.succeeded} 个仓库。确认后将同步需要更新的仓库，其余仓库只做状态核对。`,
        detail: readiness.failed > 0
          ? '更新前会创建恢复点。Web 模式会继续处理安全扫描提示；安全扫描自身失败的仓库仍会跳过。'
          : '更新前会创建恢复点；本地修改和被改写的历史均可恢复。安全扫描自身失败的仓库仍会跳过。',
        confirmLabel: '开始更新',
        tone: 'warning',
      });
      if (!confirmed) return;
    }
    setError(null);
    try {
      const next = await startOperation(kind, kind.startsWith('pull'));
      operationStore.set(next);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '操作启动失败');
    }
  };

  const cancel = async (): Promise<void> => {
    if (operation === null) return;
    setError(null);
    try {
      await cancelOperation(operation.operationId);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '取消失败');
    }
  };

  const progress = operation === null || operation.total === 0
    ? 0
    : Math.min(100, Math.round((operation.completed / operation.total) * 100));
  const indeterminate = busy && operation?.completed === 0;

  const showOperationDetails = async (): Promise<void> => {
    if (operation === null || operation.details.length === 0) return;
    const kind = operation.kind as keyof typeof labels;
    const fetchOperation = kind === 'fetch';
    await dialog.alert({
      title: fetchOperation
        ? `${displayedIssueCount} 个仓库未获取远程状态`
        : `${displayedIssueCount} 个仓库需要处理`,
      message: fetchOperation
        ? `已检查 ${operation.total} 个仓库：${operation.counters.succeeded} 个成功，${operation.counters.failed} 个失败。失败仓库未进入更新范围。`
        : `批量更新已结束：${operation.counters.succeeded} 个成功，${operation.counters.partial} 个部分成功，${operation.counters.failed} 个失败，${operation.counters.noAction} 个无需更新。`,
      items: operation.details.map((detail) => readableIssue(kind, detail)),
      tone: operation.counters.failed > 0 ? 'danger' : 'warning',
      confirmLabel: '关闭',
    });
  };

  return (
    <section className="operation-panel" aria-label="仓库操作">
      <div className="operation-actions" aria-label="仓库更新步骤">
        <div className="operation-step">
          <span className={`step-number remote ${secondStepSelected ? 'completed' : ''}`}>1</span>
          <button
            className={`action-tag remote-action ${secondStepSelected ? 'completed' : ''}`}
            disabled={busy}
            onClick={() => void start('fetch')}
          >
            获取远程状态
          </button>
        </div>
        <span className={`step-connector ${secondStepSelected ? 'ready' : ''}`} aria-hidden="true" />
        <div className="operation-step">
          <span className={`step-number backup ${secondStepSelected ? 'ready' : ''}`}>2</span>
          <button
            className="action-tag backup-action"
            disabled={busy || unsafeConnection || !remoteStateReady}
            onClick={() => void start('pull-backup')}
            aria-label={remoteStateReady
              ? '执行安全更新并保留恢复点'
              : '请先完成第一步：获取远程状态'}
          >
            安全更新且备份
          </button>
        </div>
        {busy && <button className="action-tag danger-button" onClick={() => void cancel()}>取消</button>}
      </div>
      {operation !== null && (
        <div className="operation-progress" aria-live="polite">
          <div>
            <strong>{operation.message}</strong>
            <span>
              {operation.details.length > 0 && (
                <button className="operation-details" onClick={() => void showOperationDetails()}>
                  {displayedIssueCount > 0 ? `查看 ${displayedIssueCount} 个问题` : '查看原因'}
                </button>
              )}
              {operation.completed}/{operation.total}
            </span>
          </div>
          <div className={`progress-track ${indeterminate ? 'indeterminate' : ''}`}>
            <i style={indeterminate ? undefined : { width: `${progress}%` }} />
          </div>
        </div>
      )}
      {error !== null && <p className="inline-error" role="alert">{error}</p>}
    </section>
  );
}
