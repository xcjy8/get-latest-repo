import { z } from 'zod/mini';

export const freshnessSchema = z.enum([
  'has_updates',
  'synced',
  'unreachable',
  'no_remote',
]);

export type Freshness = z.infer<typeof freshnessSchema>;

export const repositorySummarySchema = z.object({
  repoId: z.string(),
  entityVersion: z.number(),
  name: z.string(),
  path: z.string(),
  branch: z.nullable(z.string()),
  dirty: z.boolean(),
  aheadCount: z.number(),
  behindCount: z.number(),
  freshness: freshnessSchema,
  lastCommitAt: z.nullable(z.string()),
  lastCommitMessage: z.nullable(z.string()),
  lastFetchAt: z.nullable(z.string()),
  lastPullAt: z.nullable(z.string()),
});

export type RepositorySummary = z.infer<typeof repositorySummarySchema>;

export const statisticsSchema = z.object({
  total: z.number(),
  hasUpdates: z.number(),
  synced: z.number(),
  unreachable: z.number(),
  noRemote: z.number(),
  dirty: z.number(),
});

export type Statistics = z.infer<typeof statisticsSchema>;

export const operationKindSchema = z.enum([
  'scan',
  'fetch',
  'check',
  'daily',
  'pull-safe',
  'pull-force',
  'pull-backup',
]);

export const operationSchema = z.object({
  operationId: z.string(),
  kind: operationKindSchema,
  state: z.enum([
    'queued',
    'running',
    'succeeded',
    'partial_failed',
    'failed',
    'cancelled',
    'interrupted',
  ]),
  message: z.string(),
  details: z.array(z.string()),
  counters: z.object({
    succeeded: z.number(),
    failed: z.number(),
    partial: z.number(),
    noAction: z.number(),
    skipped: z.number(),
  }),
  completed: z.number(),
  total: z.number(),
  requestId: z.string(),
  sourceBatchId: z.nullable(z.string()),
  startedAt: z.nullable(z.string()),
  finishedAt: z.nullable(z.string()),
});

export type Operation = z.infer<typeof operationSchema>;

export const fetchReadinessSchema = z.object({
  batchId: z.nullable(z.string()),
  succeeded: z.number(),
  failed: z.number(),
  ready: z.boolean(),
  expiresAt: z.nullable(z.string()),
});

export type FetchReadiness = z.infer<typeof fetchReadinessSchema>;

export const bootstrapSchema = z.object({
  version: z.string(),
  revision: z.string(),
  eventSequence: z.string(),
  csrfToken: z.string(),
  repositories: z.array(repositorySummarySchema),
  statistics: statisticsSchema,
  activeOperation: z.nullable(operationSchema),
  fetchReadiness: fetchReadinessSchema,
});

export type Bootstrap = z.infer<typeof bootstrapSchema>;

export const repositoryPatchSchema = z.object({
  upserts: z.array(repositorySummarySchema),
  removes: z.array(z.string()),
});

export type RepositoryPatch = z.infer<typeof repositoryPatchSchema>;

export const serverEventSchema = z.object({
  schemaVersion: z.literal(1),
  serverInstanceId: z.string(),
  sequence: z.string(),
  occurredAt: z.string(),
  type: z.enum([
    'repositories.patch',
    'statistics.replace',
    'operation.patch',
    'resync.required',
    'heartbeat',
  ]),
  payload: z.unknown(),
});

export type ServerEvent = z.infer<typeof serverEventSchema>;

export type ScanSource = {
  index: number;
  rootPath: string;
  maxDepth: number;
  enabled: boolean;
};

export type AppConfig = {
  defaultJobs: number;
  effectiveFetchJobs: number;
  effectiveIoJobs: number;
  logicalCpus: number;
  memoryMib: number | null;
  defaultTimeout: number;
  defaultDepth: number;
  ignorePatterns: string[];
  scanSources: ScanSource[];
};

export const folderSelectionSchema = z.object({
  path: z.string(),
});
