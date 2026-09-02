import { defineConfig } from 'vite';

export default defineConfig({
    server: { port: 5175 },
    // The wasm binary is a linked workspace asset; let Vite serve it directly.
    optimizeDeps: { exclude: ['@mapconductor/vectortile'] },
});
