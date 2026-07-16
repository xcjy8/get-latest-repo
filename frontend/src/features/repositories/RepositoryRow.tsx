import { useId, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { discardRepositoryChanges } from '../../api/client';
import { useRepository } from '../../stores/repository-store';
import { useDialog } from '../../components/DialogProvider';

const freshnessLabels = {
  has_updates: ['需要更新', 'danger'],
  synced: ['已同步', 'success'],
  unreachable: ['远程不可达', 'muted'],
  no_remote: ['无远程', 'neutral'],
} as const;

type Props = {
  repoId: string;
  index: number;
  top: number;
  height: number;
};

type FieldDetailProps = {
  value: string;
};

function FieldDetail({ value }: FieldDetailProps) {
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState({ left: 12, top: 12, width: 320 });
  const triggerRef = useRef<HTMLButtonElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
  const popoverId = useId();

  useLayoutEffect(() => {
    if (!open) return;
    const updatePosition = (): void => {
      const rect = triggerRef.current?.getBoundingClientRect();
      if (rect === undefined) return;
      const width = Math.min(560, window.innerWidth - 24);
      const left = Math.min(
        Math.max(12, rect.left),
        Math.max(12, window.innerWidth - width - 12),
      );
      const below = window.innerHeight - rect.bottom;
      const top = below >= 286 ? rect.bottom + 6 : Math.max(12, rect.top - 280);
      setPosition({ left, top, width });
    };
    const closeOnOutsidePointer = (event: PointerEvent): void => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (triggerRef.current?.contains(target) || popoverRef.current?.contains(target)) return;
      setOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent): void => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      setOpen(false);
      triggerRef.current?.focus();
    };
    updatePosition();
    window.addEventListener('resize', updatePosition);
    window.addEventListener('scroll', updatePosition, true);
    document.addEventListener('pointerdown', closeOnOutsidePointer);
    document.addEventListener('keydown', closeOnEscape);
    return () => {
      window.removeEventListener('resize', updatePosition);
      window.removeEventListener('scroll', updatePosition, true);
      document.removeEventListener('pointerdown', closeOnOutsidePointer);
      document.removeEventListener('keydown', closeOnEscape);
    };
  }, [open]);

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        className="field-detail-trigger"
        aria-expanded={open}
        aria-controls={open ? popoverId : undefined}
        onClick={() => setOpen((current) => !current)}
      >
        {value}
      </button>
      {open && createPortal(
        <div
          ref={popoverRef}
          id={popoverId}
          className="field-detail-popover"
          role="dialog"
          aria-label="完整提交信息"
          style={position}
        >
          <header>
            <strong>完整提交信息</strong>
            <button
              type="button"
              aria-label="关闭完整提交信息"
              onClick={() => {
                setOpen(false);
                triggerRef.current?.focus();
              }}
            >
              ×
            </button>
          </header>
          <div>{value}</div>
        </div>,
        document.body,
      )}
    </>
  );
}

export function RepositoryRow({ repoId, index, top, height }: Props) {
  const dialog = useDialog();
  const repository = useRepository(repoId);
  const [discarding, setDiscarding] = useState(false);
  if (repository === undefined) return null;
  const [statusLabel, statusTone] = freshnessLabels[repository.freshness];
  const discard = async (): Promise<void> => {
    const confirmed = await dialog.confirm({
      title: `丢弃 ${repository.name} 的本地修改`,
      message: '全部已跟踪及未跟踪修改都会被永久丢弃，此操作无法撤销。',
      detail: repository.path,
      confirmLabel: '永久丢弃',
      tone: 'danger',
    });
    if (!confirmed) return;
    setDiscarding(true);
    try {
      await discardRepositoryChanges(repoId);
    } catch (reason) {
      await dialog.alert({
        title: '丢弃修改失败',
        message: reason instanceof Error ? reason.message : '无法丢弃本地修改。',
        tone: 'danger',
      });
    } finally {
      setDiscarding(false);
    }
  };

  return (
    <div
      className="repository-row"
      role="row"
      aria-rowindex={index + 2}
      style={{ height, transform: `translateY(${top}px)` }}
    >
      <div className="repository-primary" role="gridcell">
        <strong title={repository.name}>{repository.name}</strong>
        <span title={repository.path}>{repository.path}</span>
      </div>
      <div className="branch-cell mono" role="gridcell" title={repository.branch ?? '无分支'}>
        {repository.branch ?? '—'}
      </div>
      <div role="gridcell">
        <span className={`status-pill ${statusTone}`}>
          <i aria-hidden="true" />
          {statusLabel}
        </span>
      </div>
      <div className="sync-cell" role="gridcell">
        <span className={repository.behindCount > 0 ? 'danger-text' : ''}>
          ↓ {repository.behindCount}
        </span>
        <span>↑ {repository.aheadCount}</span>
      </div>
      <div role="gridcell">
        {repository.dirty ? <span className="dirty-badge">有修改</span> : <span className="dim">干净</span>}
      </div>
      <div className="commit-cell" role="gridcell">
        <FieldDetail value={repository.lastCommitMessage ?? '暂无提交信息'} />
      </div>
      <div role="gridcell">
        <button className="row-action" disabled={!repository.dirty || discarding} onClick={() => void discard()}>
          {discarding ? '处理中' : '丢弃修改'}
        </button>
      </div>
    </div>
  );
}
