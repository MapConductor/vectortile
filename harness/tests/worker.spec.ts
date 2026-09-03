import { expect, test } from '@playwright/test';

test.describe('off-thread rendering', () => {
    test('uses a worker by default', async ({ page }) => {
        await page.goto('/');
        await page.evaluate(() => window.__harness.ready);
        expect(await page.evaluate(() => window.__harness.mode)).toBe('worker');
    });

    test('keeps the main thread responsive under a burst of tiles', async ({ page }) => {
        await page.goto('/');
        await page.evaluate(() => window.__harness.ready);

        const inline = await page.evaluate(() => window.__harness.stallTest!('inline', 12));
        const worker = await page.evaluate(() => window.__harness.stallTest!('worker', 12));

        console.log(
            `12-tile burst — inline: longest main-thread stall ` +
                `${inline.longestStallMs.toFixed(1)} ms, wall ${inline.totalMs.toFixed(1)} ms\n` +
                `                worker: longest main-thread stall ` +
                `${worker.longestStallMs.toFixed(1)} ms, wall ${worker.totalMs.toFixed(1)} ms`,
        );

        expect(inline.mode).toBe('inline');
        expect(worker.mode).toBe('worker');

        // The worker must leave the main thread far freer than inline mode.
        // Deliberately a ratio rather than an absolute bound, so the assertion
        // survives running on slower CI hardware.
        expect(worker.longestStallMs).toBeLessThan(inline.longestStallMs / 2);
    });

    test('falls back to the main thread when a fetch hook is supplied', async ({ page }) => {
        await page.goto('/');
        await page.evaluate(() => window.__harness.ready);
        const result = await page.evaluate(() => window.__harness.fetchHookTest!());

        expect(result.mode).toBe('inline');
        expect(result.warnings.join(' ')).toContain('fetchTile');
        // Still renders — the fallback is a real path, not a stub.
        expect(result.bytes).toBeGreaterThan(1000);
    });
});
