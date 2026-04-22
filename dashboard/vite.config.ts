import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      '/stream': {
        target: 'ws://localhost:8080',
        ws: true,
        changeOrigin: true,
      },
      '/health': 'http://localhost:8080',
      '/status': 'http://localhost:8080',
      '/reset': 'http://localhost:8080',
    },
  },
});
