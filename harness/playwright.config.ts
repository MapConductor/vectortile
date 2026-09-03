import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
    testDir: './tests',
    // Two of these tests measure render time and main-thread stalls. Run them
    // one at a time: concurrent workers contend for CPU and for the OSM tile
    // service, which makes the numbers meaningless and the timeouts flaky.
    fullyParallel: false,
    workers: 1,
    timeout: 120_000,
    use: { baseURL: 'http://localhost:5175', ...devices['Desktop Chrome'] },
    webServer: {
        command: 'npm run dev',
        url: 'http://localhost:5175',
        reuseExistingServer: true,
        timeout: 60_000,
    },
});
