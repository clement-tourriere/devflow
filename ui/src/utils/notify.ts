// Imperative bridge to the toast + confirm UI.
//
// macOS WKWebView makes window.alert()/confirm() silent no-ops, so the app
// used to swallow every error and confirmation. These helpers are backed by
// real in-app UI (registered by ToastProvider / ConfirmProvider at mount) and
// can be called from anywhere — event handlers, utils, async flows — without
// threading React context through every component.

export type ToastKind = "success" | "error" | "info" | "warning";

export interface ToastOptions {
  /** Heading line. */
  title?: string;
  /** Auto-dismiss after N ms. 0 = sticky (default for errors). */
  duration?: number;
}

export interface ConfirmOptions {
  title?: string;
  message: string;
  confirmLabel?: string;
  danger?: boolean;
}

type ToastHandler = (kind: ToastKind, message: string, opts?: ToastOptions) => void;
type ConfirmHandler = (opts: ConfirmOptions) => Promise<boolean>;

let toastHandler: ToastHandler | null = null;
let confirmHandler: ConfirmHandler | null = null;

export function __registerToast(fn: ToastHandler | null) {
  toastHandler = fn;
}
export function __registerConfirm(fn: ConfirmHandler | null) {
  confirmHandler = fn;
}

function emit(kind: ToastKind, message: string, opts?: ToastOptions) {
  if (toastHandler) {
    toastHandler(kind, message, opts);
  } else {
    // Provider not mounted yet (very early startup) — fall back to console.
    // eslint-disable-next-line no-console
    console[kind === "error" ? "error" : "log"](`[${kind}] ${message}`);
  }
}

export const toast = {
  success: (message: string, opts?: ToastOptions) => emit("success", message, opts),
  error: (message: string, opts?: ToastOptions) =>
    emit("error", message, { duration: 0, ...opts }),
  info: (message: string, opts?: ToastOptions) => emit("info", message, opts),
  warning: (message: string, opts?: ToastOptions) => emit("warning", message, opts),
};

/** Promise-based confirmation. Resolves true if the user confirms. */
export function confirmDialog(opts: ConfirmOptions): Promise<boolean> {
  if (confirmHandler) return confirmHandler(opts);
  // Fallback: assume cancel rather than performing a destructive action.
  // eslint-disable-next-line no-console
  console.warn("confirmDialog called before ConfirmProvider mounted");
  return Promise.resolve(false);
}
