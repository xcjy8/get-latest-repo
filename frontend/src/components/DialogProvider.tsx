import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import { createPortal } from 'react-dom';

type DialogTone = 'default' | 'warning' | 'danger';

export type DialogDetailItem = {
  title: string;
  summary: string;
  context?: string;
  technicalDetail?: string;
};

export type DialogOptions = {
  title: string;
  message: string;
  detail?: string;
  items?: DialogDetailItem[];
  confirmLabel?: string;
  cancelLabel?: string;
  tone?: DialogTone;
};

type DialogRequest = {
  kind: 'confirm' | 'alert';
  options: DialogOptions;
  resolve: (confirmed: boolean) => void;
};

type DialogActions = {
  confirm: (options: DialogOptions) => Promise<boolean>;
  alert: (options: DialogOptions) => Promise<void>;
};

const DialogContext = createContext<DialogActions | null>(null);

export function DialogProvider({ children }: { children: ReactNode }) {
  const [current, setCurrent] = useState<DialogRequest | null>(null);
  const currentRef = useRef<DialogRequest | null>(null);
  const queueRef = useRef<DialogRequest[]>([]);
  const cardRef = useRef<HTMLElement>(null);
  const initialFocusRef = useRef<HTMLButtonElement>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  const enqueue = useCallback((kind: DialogRequest['kind'], options: DialogOptions) => {
    return new Promise<boolean>((resolve) => {
      const request = { kind, options, resolve } satisfies DialogRequest;
      if (currentRef.current === null) {
        currentRef.current = request;
        setCurrent(request);
      } else {
        queueRef.current.push(request);
      }
    });
  }, []);

  const close = useCallback((confirmed: boolean) => {
    const active = currentRef.current;
    if (active === null) return;
    active.resolve(confirmed);
    const next = queueRef.current.shift() ?? null;
    currentRef.current = next;
    setCurrent(next);
    if (next === null) previousFocusRef.current?.focus();
  }, []);

  const actions = useMemo<DialogActions>(() => ({
    confirm: (options) => enqueue('confirm', options),
    alert: async (options) => {
      await enqueue('alert', options);
    },
  }), [enqueue]);

  useEffect(() => {
    if (current === null) return;
    previousFocusRef.current = document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
    initialFocusRef.current?.focus();

    const handleKeyboard = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        event.preventDefault();
        close(false);
        return;
      }
      if (event.key !== 'Tab' || cardRef.current === null) return;
      const focusable = [...cardRef.current.querySelectorAll<HTMLElement>('button:not(:disabled)')];
      const first = focusable[0];
      const last = focusable.at(-1);
      if (first === undefined || last === undefined) return;
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener('keydown', handleKeyboard);
    return () => document.removeEventListener('keydown', handleKeyboard);
  }, [close, current]);

  return (
    <DialogContext.Provider value={actions}>
      {children}
      {current !== null && createPortal(
        <div
          className="dialog-backdrop"
          onPointerDown={(event) => {
            if (event.target === event.currentTarget) close(false);
          }}
        >
          <section
            ref={cardRef}
            className={`dialog-card ${current.options.tone ?? 'default'} ${
              current.options.items !== undefined && current.options.items.length > 0
                ? 'with-issues'
                : ''
            }`}
            role="dialog"
            aria-modal="true"
            aria-labelledby="app-dialog-title"
            aria-describedby="app-dialog-message"
          >
            <header>
              <span>{current.kind === 'alert' ? '操作提示' : '安全确认'}</span>
              <h2 id="app-dialog-title">{current.options.title}</h2>
            </header>
            <p id="app-dialog-message">{current.options.message}</p>
            {current.options.detail !== undefined && (
              <div className="dialog-detail">{current.options.detail}</div>
            )}
            {current.options.items !== undefined && current.options.items.length > 0 && (
              <div className="dialog-issue-list">
                {current.options.items.map((item, index) => (
                  <article key={`${item.title}-${index}`}>
                    <div>
                      <span>{index + 1}</span>
                      <h3>{item.title}</h3>
                    </div>
                    <p>{item.summary}</p>
                    {item.context !== undefined && <small>{item.context}</small>}
                    {item.technicalDetail !== undefined && (
                      <details>
                        <summary>查看技术详情</summary>
                        <pre>{item.technicalDetail}</pre>
                      </details>
                    )}
                  </article>
                ))}
              </div>
            )}
            <footer>
              {current.kind === 'confirm' && (
                <button
                  ref={initialFocusRef}
                  className="dialog-button secondary"
                  onClick={() => close(false)}
                >
                  {current.options.cancelLabel ?? '取消'}
                </button>
              )}
              <button
                ref={current.kind === 'alert' ? initialFocusRef : undefined}
                className={`dialog-button confirm ${current.options.tone ?? 'default'}`}
                onClick={() => close(true)}
              >
                {current.options.confirmLabel ?? (current.kind === 'alert' ? '知道了' : '确认')}
              </button>
            </footer>
          </section>
        </div>,
        document.body,
      )}
    </DialogContext.Provider>
  );
}

export function useDialog(): DialogActions {
  const context = useContext(DialogContext);
  if (context === null) throw new Error('useDialog 必须在 DialogProvider 内使用');
  return context;
}
