import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// In dev, the UI runs on Vite (5173) and proxies the API to `foxygnal serve`.
export default defineConfig({
  plugins: [svelte()],
  server: {
    host: '127.0.0.1',
    proxy: {
      '/api': { target: 'http://127.0.0.1:7355', ws: true },
    },
  },
});
