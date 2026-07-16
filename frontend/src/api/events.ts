import { z } from 'zod/mini';
import {
  operationSchema,
  repositoryPatchSchema,
  serverEventSchema,
  statisticsSchema,
  type Operation,
  type RepositorySummary,
  type ServerEvent,
  type Statistics,
} from './contracts';
import { connectionStore } from '../stores/connection-store';
import { operationStore } from '../stores/operation-store';
import { repositoryStore } from '../stores/repository-store';

/**
 * 高频仓库事件先按 ID 去重，再于下一绘制帧一次提交。
 * 网络接收保持实时，React 提交频率最多为屏幕刷新率。
 */
export function connectEventStream(
  afterSequence: string,
  onResyncRequired: () => void,
): () => void {
  const source = new EventSource(`/api/v1/events?after=${encodeURIComponent(afterSequence)}`);
  const pendingUpserts = new Map<string, RepositorySummary>();
  const pendingRemoves = new Set<string>();
  let pendingStatistics: Statistics | null = null;
  let pendingOperation: Operation | null | undefined;
  let frameId: number | null = null;
  let latestSequence = BigInt(afterSequence);
  let resyncRequested = false;

  const flush = (): void => {
    frameId = null;
    if (pendingUpserts.size > 0 || pendingRemoves.size > 0) {
      repositoryStore.applyPatch({
        upserts: [...pendingUpserts.values()],
        removes: [...pendingRemoves],
      });
      pendingUpserts.clear();
      pendingRemoves.clear();
    }
    if (pendingStatistics !== null) {
      repositoryStore.replaceStatistics(pendingStatistics);
      pendingStatistics = null;
    }
    if (pendingOperation !== undefined) {
      operationStore.set(pendingOperation);
      pendingOperation = undefined;
    }
  };

  const scheduleFlush = (): void => {
    frameId ??= requestAnimationFrame(flush);
  };

  const requestResync = (): void => {
    if (resyncRequested) return;
    resyncRequested = true;
    source.close();
    connectionStore.set('resyncing');
    onResyncRequired();
  };

  source.onopen = () => connectionStore.set('live');
  source.onerror = () => connectionStore.set('stale');
  source.onmessage = (message) => {
    let event: ServerEvent;
    let sequence: bigint;
    try {
      const parsed = serverEventSchema.safeParse(JSON.parse(message.data));
      if (!parsed.success) {
        requestResync();
        return;
      }
      event = parsed.data;
      sequence = BigInt(event.sequence);
      if (sequence <= latestSequence) return;
    } catch {
      requestResync();
      return;
    }

    switch (event.type) {
      case 'repositories.patch': {
        const parsed = repositoryPatchSchema.safeParse(event.payload);
        if (!parsed.success) {
          requestResync();
          return;
        }
        const patch = parsed.data;
        for (const repository of patch.upserts) {
          pendingRemoves.delete(repository.repoId);
          pendingUpserts.set(repository.repoId, repository);
        }
        for (const repoId of patch.removes) {
          pendingUpserts.delete(repoId);
          pendingRemoves.add(repoId);
        }
        scheduleFlush();
        break;
      }
      case 'statistics.replace': {
        const parsed = statisticsSchema.safeParse(event.payload);
        if (!parsed.success) {
          requestResync();
          return;
        }
        pendingStatistics = parsed.data;
        scheduleFlush();
        break;
      }
      case 'operation.patch': {
        const parsed = z.nullable(operationSchema).safeParse(event.payload);
        if (!parsed.success) {
          requestResync();
          return;
        }
        pendingOperation = parsed.data;
        scheduleFlush();
        break;
      }
      case 'resync.required':
        requestResync();
        break;
      case 'heartbeat':
        connectionStore.set('live');
        break;
    }
    latestSequence = sequence;
  };

  return () => {
    source.close();
    if (frameId !== null) cancelAnimationFrame(frameId);
  };
}
