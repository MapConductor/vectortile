import { expect, test } from '@playwright/test';

/**
 * Proves the whole chain: style.json -> wasm renderer -> LocalTileServer route
 * -> raster tile a map SDK can consume.
 */
test.describe('vector tile renderer served as a raster layer', () => {
    test('renders a tile through the provider and serves it over the tile server', async ({
        page,
    }) => {
        const tileResponses: { url: string; status: number; type: string }[] = [];
        page.on('response', (response) => {
            if (response.url().includes('/__tiles/')) {
                tileResponses.push({
                    url: response.url(),
                    status: response.status(),
                    type: response.headers()['content-type'] ?? '',
                });
            }
        });

        await page.goto('/');

        const { template, renderMs } = await page.evaluate(() => window.__harness.ready);
        expect(template).toContain('/__tiles/vectortile-demo/512/');
        expect(template).toContain('{z}/{x}/{y}.png');

        // The direct render must produce actual map content, not a blank tile.
        // Sampling the probe canvas checks the pixels, not just the byte count.
        const probe = await page.evaluate(() => {
            const canvas = document.getElementById('probe') as HTMLCanvasElement;
            const ctx = canvas.getContext('2d');
            if (!ctx) return null;
            const { data } = ctx.getImageData(0, 0, canvas.width, canvas.height);
            const seen = new Set<string>();
            let opaque = 0;
            for (let i = 0; i < data.length; i += 4) {
                if (data[i + 3]! > 0) opaque += 1;
                seen.add(`${data[i]},${data[i + 1]},${data[i + 2]}`);
            }
            return { opaque, total: data.length / 4, distinctColors: seen.size };
        });

        expect(probe).not.toBeNull();
        // A world tile from the demo style is fully covered by the ocean
        // background, then painted over with many country colours.
        expect(probe!.opaque).toBe(probe!.total);
        expect(probe!.distinctColors).toBeGreaterThan(10);

        // The map must actually pull tiles through the service worker route.
        await expect
            .poll(() => tileResponses.length, { timeout: 30_000 })
            .toBeGreaterThan(0);
        for (const response of tileResponses) {
            expect(response.status).toBe(200);
            expect(response.type).toContain('image/png');
        }

        // Not an assertion so much as a recorded number: this is the figure
        // that decides whether on-device rendering is viable at all.
        console.log(`single 512px tile render: ${renderMs.toFixed(1)} ms`);
    });

    test('reports symbol layers as undrawn rather than failing', async ({ page }) => {
        await page.goto('/');
        await page.evaluate(() => window.__harness.ready);
        const status = await page.locator('#status').textContent();
        expect(status).toContain('symbol');
    });
});
