import { createContext, useContext, useState, useCallback } from "react";

interface TerminalRequest {
  projectPath?: string;
  branchName?: string;
  serviceName?: string;
}

interface TerminalContextValue {
  isVisible: boolean;
  toggle: () => void;
  show: () => void;
  openTerminal: (req: TerminalRequest) => void;
  pendingTerminal: TerminalRequest | null;
  clearPending: () => void;
}

const TerminalContext = createContext<TerminalContextValue | null>(null);

export function TerminalProvider({ children }: { children: React.ReactNode }) {
  const [isVisible, setIsVisible] = useState(false);
  const [pendingTerminal, setPendingTerminal] =
    useState<TerminalRequest | null>(null);

  const toggle = useCallback(() => setIsVisible((v) => !v), []);
  const show = useCallback(() => setIsVisible(true), []);

  const openTerminal = useCallback((req: TerminalRequest) => {
    setIsVisible(true);
    setPendingTerminal(req);
  }, []);

  const clearPending = useCallback(() => setPendingTerminal(null), []);

  return (
    <TerminalContext.Provider
      value={{ isVisible, toggle, show, openTerminal, pendingTerminal, clearPending }}
    >
      {children}
    </TerminalContext.Provider>
  );
}

export function useTerminal() {
  const ctx = useContext(TerminalContext);
  if (!ctx) throw new Error("useTerminal must be used within TerminalProvider");
  return ctx;
}
