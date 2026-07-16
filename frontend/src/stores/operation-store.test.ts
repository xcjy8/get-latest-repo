import { describe, expect, it } from 'vitest';
import type { Operation } from '../api/contracts';
import { operationStore } from './operation-store';

const operation = (overrides: Partial<Operation>): Operation => ({
  operationId: 'batch-1',
  kind: 'fetch',
  state: 'succeeded',
  message: '完成',
  details: [],
  counters: {
    succeeded: 0,
    failed: 0,
    partial: 0,
    noAction: 0,
    skipped: 0,
  },
  completed: 0,
  total: 0,
  requestId: 'request-1',
  sourceBatchId: null,
  startedAt: null,
  finishedAt: new Date(Date.now() + 60_000).toISOString(),
  ...overrides,
});

describe('操作批次状态', () => {
  it('Fetch 部分失败仍只按真实成功数解锁第二步', () => {
    operationStore.set(operation({
      state: 'partial_failed',
      counters: {
        succeeded: 402,
        failed: 5,
        partial: 0,
        noAction: 0,
        skipped: 0,
      },
      completed: 407,
      total: 407,
    }));

    expect(operationStore.getFetchReadinessSnapshot()).toMatchObject({
      batchId: 'batch-1',
      succeeded: 402,
      failed: 5,
      ready: true,
    });
  });

  it('新 Fetch 开始时清除旧批次就绪状态', () => {
    operationStore.set(operation({ state: 'running' }));

    expect(operationStore.getFetchReadinessSnapshot()).toMatchObject({
      batchId: null,
      succeeded: 0,
      ready: false,
    });
  });

  it('选择第二步不会伪造 Fetch 已完成', () => {
    operationStore.setFetchReadiness({
      batchId: null,
      succeeded: 0,
      failed: 0,
      ready: false,
      expiresAt: null,
    });
    operationStore.set(operation({
      kind: 'pull-backup',
      state: 'running',
      sourceBatchId: 'fetch-batch',
    }));

    expect(operationStore.getFetchReadinessSnapshot().ready).toBe(false);
  });

  it('过期批次立即锁定第二步', () => {
    operationStore.setFetchReadiness({
      batchId: 'expired-batch',
      succeeded: 407,
      failed: 0,
      ready: true,
      expiresAt: new Date(Date.now() - 1_000).toISOString(),
    });

    expect(operationStore.getFetchReadinessSnapshot().ready).toBe(false);
  });
});
