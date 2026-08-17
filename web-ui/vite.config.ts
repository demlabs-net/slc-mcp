import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [svelte()],
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: process.env.SLC_WEBUI_URL || 'http://127.0.0.1:3002',
        changeOrigin: true,
      },
    },
  },
})
