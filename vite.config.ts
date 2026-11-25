import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    rollupOptions: {
      input: {
        main: 'index.html',
        // Standalone Agent playground; harmless in the packaged renderer and
        // available for `file://.../dist/agent.html?demo=1` UI debugging.
        agent: 'agent.html',
      },
    },
  },
  server: {
    port: 5173,
    strictPort: true,
  },
});
