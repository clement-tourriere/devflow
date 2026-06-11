import { useCallback, useEffect, useRef, useState } from "react";
import { __registerToast, type ToastKind, type ToastOptions } from "../utils/notify";

interface ToastItem {
  id: number;
  kind: ToastKind;
  message: string;
  title?: string;
}

const DEFAULT_DURATION: Record<ToastKind, number> = {
  success: 3500,
  info: 4000,
  warning: 6000,
  error: 0, // sticky until dismissed
};

const ICON: Record<ToastKind, string> = {
  success: "✓",
  error: "✕",
  warning: "!",
  info: "i",
};

/**
 * Renders a stacked toast region and registers the imperative `toast` API.
 * Mount once near the app root.
 */
export function ToastProvider({ children }: { children: React.ReactNode }) {
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const nextId = useRef(1);
  const timers = useRef(new Map<number, ReturnType<typeof setTimeout>>());

  const dismiss = useCallback((id: number) => {
    setToasts((prev) => prev.filter((t) => t.id !== id));
    const timer = timers.current.get(id);
    if (timer) {
      clearTimeout(timer);
      timers.current.delete(id);
    }
  }, []);

  useEffect(() => {
    __registerToast((kind: ToastKind, message: string, opts?: ToastOptions) => {
      const id = nextId.current++;
      setToasts((prev) => [...prev, { id, kind, message, title: opts?.title }]);
      const duration = opts?.duration ?? DEFAULT_DURATION[kind];
      if (duration > 0) {
        timers.current.set(
          id,
          setTimeout(() => dismiss(id), duration),
        );
      }
    });
    return () => __registerToast(null);
  }, [dismiss]);

  // Capture the timers map for cleanup without tripping the exhaustive-deps lint
  const timersRef = timers;
  useEffect(() => {
    const map = timersRef.current;
    return () => map.forEach((t) => clearTimeout(t));
  }, [timersRef]);

  return (
    <>
      {children}
      <div className="toast-region" role="region" aria-label="Notifications">
        {toasts.map((t) => (
          <div key={t.id} className={`toast toast-${t.kind}`} role="alert">
            <span className="toast-icon">{ICON[t.kind]}</span>
            <div className="toast-body">
              {t.title && <div className="toast-title">{t.title}</div>}
              <div className="toast-message">{t.message}</div>
            </div>
            <button
              className="toast-close"
              onClick={() => dismiss(t.id)}
              aria-label="Dismiss"
            >
              ✕
            </button>
          </div>
        ))}
      </div>
    </>
  );
}
