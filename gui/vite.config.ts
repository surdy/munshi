import react from "@vitejs/plugin-react";
// `vitest/config` rather than `vite`, so the `test` block below is typed.
import { defineConfig } from "vitest/config";

// Tauri serves the dev build from a fixed port and fails rather than silently moving, so a port
// clash surfaces as an error instead of a window pointed at nothing. 1421 keeps out of Madari's way.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1421,
    strictPort: true,
  },
  build: {
    // Tauri 2's floor is macOS 13 (Safari 16.1) and webkit2gtk-4.1 (2022+); both are fully
    // ES2022, so there is nothing to gain by transpiling further down.
    target: "es2022",
    sourcemap: false,
  },
  test: {
    environment: "jsdom",
    globals: true,
  },
});
