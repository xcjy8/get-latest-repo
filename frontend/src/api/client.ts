import {
  bootstrapSchema,
  folderSelectionSchema,
  type AppConfig,
  type Bootstrap,
  type Operation,
} from './contracts';

let csrfToken = '';

export async function loadBootstrap(signal?: AbortSignal): Promise<Bootstrap> {
  const response = await fetch('/api/v1/bootstrap', {
    ...(signal === undefined ? {} : { signal }),
    cache: 'no-store',
    headers: { Accept: 'application/json' },
  });
  if (!response.ok) {
    throw new Error(`加载控制台数据失败（HTTP ${response.status}）`);
  }
  const bootstrap = bootstrapSchema.parse(await response.json());
  csrfToken = bootstrap.csrfToken;
  return bootstrap;
}

export async function startOperation(
  kind: 'scan' | 'fetch' | 'check' | 'daily' | 'pull-safe' | 'pull-force' | 'pull-backup',
  confirmed: boolean,
): Promise<Operation> {
  const requestId = globalThis.crypto.randomUUID();
  const response = await fetch('/api/v1/operations', {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/json',
      'X-GetLatestRepo-CSRF': csrfToken,
    },
    body: JSON.stringify({ kind, confirmed, requestId }),
  });
  if (!response.ok) {
    const body = (await response.json().catch(() => null)) as { message?: string } | null;
    throw new Error(body?.message ?? `启动操作失败（HTTP ${response.status}）`);
  }
  return (await response.json()) as Operation;
}

export async function cancelOperation(operationId: string): Promise<void> {
  const response = await fetch(`/api/v1/operations/${operationId}`, {
    method: 'DELETE',
    headers: { 'X-GetLatestRepo-CSRF': csrfToken },
  });
  if (!response.ok) {
    throw new Error(`取消操作失败（HTTP ${response.status}）`);
  }
}

export async function loadConfig(): Promise<AppConfig> {
  const response = await fetch('/api/v1/config', { cache: 'no-store' });
  if (!response.ok) throw new Error(`读取配置失败（HTTP ${response.status}）`);
  return (await response.json()) as AppConfig;
}

export async function selectScanSourceDirectory(): Promise<string | null> {
  const response = await fetch('/api/v1/dialogs/folder', {
    method: 'POST',
    headers: { 'X-GetLatestRepo-CSRF': csrfToken },
  });
  if (response.status === 204) return null;
  if (!response.ok) {
    const body = (await response.json().catch(() => null)) as { message?: string } | null;
    throw new Error(body?.message ?? `打开文件夹选择器失败（HTTP ${response.status}）`);
  }
  const body = folderSelectionSchema.parse(await response.json());
  return body.path;
}

export async function addScanSource(path: string): Promise<AppConfig> {
  const response = await fetch('/api/v1/sources', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'X-GetLatestRepo-CSRF': csrfToken,
    },
    body: JSON.stringify({ path }),
  });
  if (!response.ok) {
    const body = (await response.json().catch(() => null)) as { message?: string } | null;
    throw new Error(body?.message ?? `新增扫描源失败（HTTP ${response.status}）`);
  }
  return (await response.json()) as AppConfig;
}

export async function removeScanSource(index: number): Promise<AppConfig> {
  const response = await fetch(`/api/v1/sources/${index}`, {
    method: 'DELETE',
    headers: { 'X-GetLatestRepo-CSRF': csrfToken },
  });
  if (!response.ok) {
    const body = (await response.json().catch(() => null)) as { message?: string } | null;
    throw new Error(body?.message ?? `移除扫描源失败（HTTP ${response.status}）`);
  }
  return (await response.json()) as AppConfig;
}

export async function discardRepositoryChanges(repoId: string): Promise<number> {
  const response = await fetch(`/api/v1/repositories/${repoId}/discard`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'X-GetLatestRepo-CSRF': csrfToken,
    },
    body: JSON.stringify({ confirmed: true }),
  });
  if (!response.ok) {
    const body = (await response.json().catch(() => null)) as { message?: string } | null;
    throw new Error(body?.message ?? `丢弃修改失败（HTTP ${response.status}）`);
  }
  const body = (await response.json()) as { discardedFiles: string[] };
  return body.discardedFiles.length;
}
