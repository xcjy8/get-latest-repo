import { describe, expect, it } from 'vitest';
import {
  operationSchema,
  repositoryPatchSchema,
  serverEventSchema,
} from './contracts';

describe('实时接口契约', () => {
  it('拒绝未知操作类型', () => {
    const result = operationSchema.safeParse({
      operationId: 'operation-1',
      kind: 'unknown',
      state: 'running',
      message: '运行中',
      details: [],
      completed: 0,
      total: 1,
      startedAt: null,
      finishedAt: null,
    });

    expect(result.success).toBe(false);
  });

  it('拒绝字段残缺的仓库增量', () => {
    const result = repositoryPatchSchema.safeParse({
      upserts: [{ repoId: 'repo-1' }],
      removes: [],
    });

    expect(result.success).toBe(false);
  });

  it('拒绝不兼容的事件协议版本', () => {
    const result = serverEventSchema.safeParse({
      schemaVersion: 2,
      serverInstanceId: 'server-1',
      sequence: '1',
      occurredAt: '2026-07-15T00:00:00Z',
      type: 'heartbeat',
      payload: {},
    });

    expect(result.success).toBe(false);
  });
});
