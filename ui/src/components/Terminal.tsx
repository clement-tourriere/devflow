import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { Restty, parseGhosttyTheme } from "restty";
import { writeTerminal, resizeTerminal } from "../utils/invoke";
import type {
  TerminalExitEvent,
  TerminalOutputEvent,
  TerminalRenderer,
} from "../types";

/** Encode a string to base64, handling UTF-8 bytes safely. */
function encodeBase64(str: string): string {
  const bytes = new TextEncoder().encode(str);
  let binary = "";
  for (let i = 0; i < bytes.length; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary);
}

/** Decode base64 into raw bytes from backend terminal stream. */
function decodeBase64(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

interface TerminalProps {
  sessionId: string;
  isActive: boolean;
  renderer: TerminalRenderer;
  fontSize: number;
}

function Terminal({ sessionId, isActive, renderer, fontSize }: TerminalProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const resttyRef = useRef<Restty | null>(null);
  const [initError, setInitError] = useState<string | null>(null);

  const theme = useMemo(
    () =>
      parseGhosttyTheme(`
background = #0d1117
foreground = #e6edf3
cursor-color = #58a6ff
selection-background = #264f78
selection-foreground = #ffffff
palette = 0=#0d1117
palette = 1=#f85149
palette = 2=#3fb950
palette = 3=#d29922
palette = 4=#58a6ff
palette = 5=#bc8cff
palette = 6=#76e3ea
palette = 7=#e6edf3
palette = 8=#6e7681
palette = 9=#f85149
palette = 10=#3fb950
palette = 11=#d29922
palette = 12=#58a6ff
palette = 13=#bc8cff
palette = 14=#76e3ea
palette = 15=#ffffff
`),
    []
  );

  useEffect(() => {
    if (!containerRef.current) return;

    let disposed = false;
    let restty: Restty | null = null;
    const pendingData: Uint8Array[] = [];
    const decoder = new TextDecoder();
    const lastSize = { cols: 0, rows: 0 };

    const writeOutput = (bytes: Uint8Array) => {
      if (!restty) {
        pendingData.push(bytes);
        return;
      }

      const text = decoder.decode(bytes, { stream: true });
      if (text.length > 0) {
        restty.sendInput(text, "pty");
      }
    };

    // Register event listeners before initializing renderer.
    const unlistenOutputPromise = listen<TerminalOutputEvent>(
      "terminal-output",
      (event) => {
        if (event.payload.session_id !== sessionId) return;
        try {
          writeOutput(decodeBase64(event.payload.data));
        } catch {
          // Ignore malformed chunks.
        }
      }
    );

    const unlistenExitPromise = listen<TerminalExitEvent>("terminal-exit", (event) => {
      if (event.payload.session_id === sessionId && restty) {
        restty.sendInput("\r\n\x1b[90m[Process exited]\x1b[0m\r\n", "pty");
      }
    });

    const setup = () => {
      if (disposed || !containerRef.current) return;

      try {
        setInitError(null);

        restty = new Restty({
          root: containerRef.current,
          createInitialPane: { focus: isActive },
          shortcuts: false,
          defaultContextMenu: false,
          appOptions: {
            renderer,
            autoResize: true,
            fontSize,
            callbacks: {
              onTermSize: (cols: number, rows: number) => {
                if (cols <= 0 || rows <= 0) return;
                if (cols === lastSize.cols && rows === lastSize.rows) return;
                lastSize.cols = cols;
                lastSize.rows = rows;
                resizeTerminal(sessionId, rows, cols).catch(() => {});
              },
            },
            beforeInput: ({ text, source }: { text: string; source: string }) => {
              if (source === "pty" || text.length === 0) {
                return text;
              }

              writeTerminal(sessionId, encodeBase64(text)).catch(() => {});
              return null;
            },
          },
        });

        restty.applyTheme(theme, "devflow");
        resttyRef.current = restty;

        for (const chunk of pendingData) {
          writeOutput(chunk);
        }
        pendingData.length = 0;

        requestAnimationFrame(() => {
          if (!restty || disposed) return;
          restty.updateSize(true);
          if (isActive) {
            restty.focus();
          }
        });
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        setInitError(message);
      }
    };

    setup();

    return () => {
      disposed = true;
      unlistenOutputPromise.then((fn) => fn());
      unlistenExitPromise.then((fn) => fn());

      const tail = decoder.decode();
      if (tail.length > 0 && restty) {
        restty.sendInput(tail, "pty");
      }

      restty?.destroy();
      resttyRef.current = null;
    };
  }, [sessionId, renderer, fontSize, theme]);

  useEffect(() => {
    const restty = resttyRef.current;
    if (!isActive || !restty) return;

    const timer = window.setTimeout(() => {
      restty.updateSize(true);
      restty.focus();
    }, 40);

    return () => {
      window.clearTimeout(timer);
    };
  }, [isActive]);

  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        display: isActive ? "block" : "none",
        position: "relative",
        overflow: "hidden",
      }}
    >
      <div
        ref={containerRef}
        style={{
          width: "100%",
          height: "100%",
          position: "absolute",
          inset: 0,
        }}
      />
      {initError && (
        <div
          style={{
            position: "absolute",
            left: 12,
            right: 12,
            bottom: 12,
            padding: "8px 10px",
            borderRadius: 6,
            border: "1px solid var(--danger)",
            background: "rgba(248, 81, 73, 0.12)",
            color: "var(--danger)",
            fontSize: 12,
            fontFamily: "monospace",
            zIndex: 20,
          }}
        >
          Terminal initialization failed: {initError}
        </div>
      )}
    </div>
  );
}

export default Terminal;
