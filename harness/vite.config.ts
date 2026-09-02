import { defineConfig } from 'vite';
import { fileURLToPath } from 'node:url';

const repoRoot = fileURLToPath(new URL('..', import.meta.url));

export default defineConfig({
    server: {
        port: 5175,
        // `@mapconductor/vectortile` is a file: link, and its generated `pkg/`
        // sits outside this app's root.
        fs: { allow: [repoRoot] },
    },
});
