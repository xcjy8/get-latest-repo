import { useEffect, useRef, useState } from 'react';
import {
  addScanSource,
  loadConfig,
  removeScanSource,
  selectScanSourceDirectory,
} from '../api/client';
import type { AppConfig } from '../api/contracts';
import { useDialog } from './DialogProvider';

export function ConfigPanel() {
  const dialog = useDialog();
  const [open, setOpen] = useState(false);
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const panelRef = useRef<HTMLElement>(null);

  useEffect(() => {
    if (!open) return;
    const closeOnOutsidePointer = (event: PointerEvent): void => {
      if (!panelRef.current?.contains(event.target as Node)) setOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') setOpen(false);
    };
    document.addEventListener('pointerdown', closeOnOutsidePointer);
    document.addEventListener('keydown', closeOnEscape);
    return () => {
      document.removeEventListener('pointerdown', closeOnOutsidePointer);
      document.removeEventListener('keydown', closeOnEscape);
    };
  }, [open]);

  const toggle = async (): Promise<void> => {
    const next = !open;
    setOpen(next);
    if (next && config === null) {
      try {
        setConfig(await loadConfig());
      } catch (reason) {
        setError(reason instanceof Error ? reason.message : '读取配置失败');
      }
    }
  };

  const chooseAndAdd = async (): Promise<void> => {
    setSaving(true);
    setError(null);
    try {
      const selectedPath = await selectScanSourceDirectory();
      if (selectedPath !== null) setConfig(await addScanSource(selectedPath));
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '选择扫描目录失败');
    } finally {
      setSaving(false);
    }
  };

  const remove = async (index: number, rootPath: string): Promise<void> => {
    const confirmed = await dialog.confirm({
      title: '移除扫描源',
      message: '该目录将不再参与后续扫描，仓库文件不会被删除。',
      detail: rootPath,
      confirmLabel: '确认移除',
      tone: 'danger',
    });
    if (!confirmed) return;
    setSaving(true);
    setError(null);
    try {
      setConfig(await removeScanSource(index));
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '移除扫描源失败');
    } finally {
      setSaving(false);
    }
  };

  return (
    <section ref={panelRef} className={`config-panel ${open ? 'open' : ''}`}>
      <button
        className="config-toggle"
        onClick={() => void toggle()}
        aria-expanded={open}
        aria-controls="scan-source-popover"
      >
        扫描配置
        {config !== null && <span>{config.scanSources.length}</span>}
      </button>
      {open && (
        <div className="config-content" id="scan-source-popover">
          <div className="config-heading">
            <strong>扫描源</strong>
            <button className="action-tag primary" disabled={saving} onClick={() => void chooseAndAdd()}>
              {saving ? '处理中' : '选择并添加文件夹'}
            </button>
          </div>
          <div className="config-summary">
            <span>获取并发 <b>{config?.effectiveFetchJobs ?? '—'}</b></span>
            <span>扫描并发 <b>{config?.effectiveIoJobs ?? '—'}</b></span>
            <span>
              资源{' '}
              <b>
                {config === null
                  ? '—'
                  : `${config.logicalCpus} 核 / ${
                      config.memoryMib === null
                        ? '未知内存'
                        : `${Math.round(config.memoryMib / 1024)} GiB`
                    }`}
              </b>
            </span>
            <span>超时 <b>{config?.defaultTimeout ?? '—'}s</b></span>
            <span>默认深度 <b>{config?.defaultDepth ?? '—'}</b></span>
          </div>
          <div className="source-list">
            {config?.scanSources.map((source) => (
              <div className="source-item" key={source.rootPath}>
                <span><b>{source.rootPath}</b><small>最大深度 {source.maxDepth}</small></span>
                <button disabled={saving} onClick={() => void remove(source.index, source.rootPath)}>移除</button>
              </div>
            ))}
          </div>
          {error !== null && <p className="inline-error" role="alert">{error}</p>}
        </div>
      )}
    </section>
  );
}
