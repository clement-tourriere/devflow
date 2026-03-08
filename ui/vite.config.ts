import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    open: false,
    watch: {
      ignored: ["**/src-tauri/**", "**/.agents/**", "**/.claude/**", "**/.devflow/**"],
    },
  },
});
