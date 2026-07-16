import babel from '@rolldown/plugin-babel';
import react, { reactCompilerPreset } from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  plugins: [
    react(),
    babel({ presets: [reactCompilerPreset()] }),
  ],
  build: {
    target: 'es2024',
    sourcemap: false,
    cssMinify: 'lightningcss',
    reportCompressedSize: true,
  },
  server: {
    host: '127.0.0.1',
    port: 9215,
    strictPort: true,
    proxy: {
      '/api': 'http://127.0.0.1:8615',
    },
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/tests/setup.ts'],
  },
});
