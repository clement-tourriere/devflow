import { useCallback, useEffect, useRef, useState } from "react";
import ConfirmDialog from "./ConfirmDialog";
import { __registerConfirm, type ConfirmOptions } from "../utils/notify";

interface PendingConfirm extends ConfirmOptions {
  resolve: (ok: boolean) => void;
}

/**
 * Registers the imperative `confirmDialog()` API backed by the real
 * ConfirmDialog modal (window.confirm is a no-op in WKWebView). Mount once.
 */
export function ConfirmProvider({ children }: { children: React.ReactNode }) {
  const [pending, setPending] = useState<PendingConfirm | null>(null);
  const pendingRef = useRef<PendingConfirm | null>(null);
  pendingRef.current = pending;

  useEffect(() => {
    __registerConfirm(
      (opts: ConfirmOptions) =>
        new Promise<boolean>((resolve) => {
          setPending({ ...opts, resolve });
        }),
    );
    return () => __registerConfirm(null);
  }, []);

  const close = useCallback((ok: boolean) => {
    setPending((prev) => {
      prev?.resolve(ok);
      return null;
    });
  }, []);

  return (
    <>
      {children}
      <ConfirmDialog
        open={pending !== null}
        title={pending?.title ?? "Confirm"}
        message={pending?.message ?? ""}
        confirmLabel={pending?.confirmLabel}
        danger={pending?.danger}
        onClose={() => close(false)}
        onConfirm={() => close(true)}
      />
    </>
  );
}
