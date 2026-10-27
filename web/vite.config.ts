import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// The built bundle is served by FastAPI from web/dist, so dev proxies the API
// to the local worker to keep everything same-origin in both modes.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://127.0.0.1:8091',
      '/healthz': 'http://127.0.0.1:8091',
      '/readyz': 'http://127.0.0.1:8091',
    },
  },
  // noVNC (the 登录态 page's remote screen) uses top-level await, which the
  // default target predates; every browser that can open this UI has it.
  optimizeDeps: {
    esbuildOptions: { target: 'es2022' },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
  },
});
