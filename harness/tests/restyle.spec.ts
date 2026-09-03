import { expect, test } from '@playwright/test';

/** Average RGB of a PNG served by the tile server, sampled in the page. */
async function tileAverage(page: import('@playwright/test').Page, url: string) {
    return page.evaluate(async (tileUrl: string) => {
        const response = await fetch(tileUrl);
        const bitmap = await createImageBitmap(await response.blob());
        const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
        const ctx = canvas.getContext('2d')!;
        ctx.drawImage(bitmap, 0, 0);
        const { data } = ctx.getImageData(0, 0, canvas.width, canvas.height);
        let r = 0, g = 0, b = 0;
        for (let i = 0; i < data.length; i += 4) {
            r += data[i]!; g += data[i + 1]!; b += data[i + 2]!;
        }
        const n = data.length / 4;
        return { r: r / n, g: g / n, b: b / n };
    }, url);
}

const tileUrl = (template: string) =>
    template.replace('{z}', '0').replace('{x}', '0').replace('{y}', '0');

test.describe('live restyling', () => {
    test('exposes a colour control per drawable, vector-backed layer', async ({ page }) => {
        await page.goto('/');
        await page.evaluate(() => window.__harness.ready);

        const controls = await page.evaluate(() => window.__harness.listColors!());
        const ids = controls.map((c) => `${c.layerId}/${c.property}`);

        expect(ids).toContain('background/background-color');
        expect(ids).toContain('countries-fill/fill-color');
        // Symbol layers are never drawn, so they must not get a dead control.
        expect(ids.some((id) => id.startsWith('countries-label'))).toBe(false);
        // Neither should the geojson-backed layer, whose source this renderer
        // cannot read.
        expect(ids.some((id) => id.startsWith('crimea-fill'))).toBe(false);

        // The per-country fill is a `match` expression, not a flat colour.
        const countries = controls.find((c) => c.layerId === 'countries-fill');
        expect(countries?.isExpression).toBe(true);

        // The UI should render one row per control.
        await expect(page.locator('#colors input[type="color"]')).toHaveCount(controls.length);
    });

    test('recolouring changes the tiles without refetching vector data', async ({ page }) => {
        await page.goto('/');
        const { template } = await page.evaluate(() => window.__harness.ready);

        const before = await tileAverage(page, tileUrl(template));

        // Only start counting once the initial load has settled.
        const pbfRequests: string[] = [];
        page.on('request', (request) => {
            if (request.url().includes('.pbf')) pbfRequests.push(request.url());
        });

        const elapsedMs = await page.evaluate(() =>
            window.__harness.setColor!('background', 'background-color', '#ff0000'),
        );
        console.log(`restyle applied in ${elapsedMs.toFixed(1)} ms`);

        const nextTemplate = await page.evaluate(() => window.__harness.template!);
        expect(nextTemplate).not.toBe(template);
        const after = await tileAverage(page, tileUrl(nextTemplate));

        // The ocean background dominates tile 0/0/0, so turning it red must
        // move the average strongly toward red.
        expect(after.r).toBeGreaterThan(before.r + 20);
        expect(after.b).toBeLessThan(before.b - 20);

        // The whole point: geometry is unchanged, so nothing is refetched.
        expect(pbfRequests).toEqual([]);
    });

    test('reset restores the original colours', async ({ page }) => {
        await page.goto('/');
        const { template } = await page.evaluate(() => window.__harness.ready);
        const original = await tileAverage(page, tileUrl(template));

        await page.evaluate(() =>
            window.__harness.setColor!('background', 'background-color', '#ff0000'),
        );
        await page.evaluate(() => window.__harness.resetColors!());

        const restoredTemplate = await page.evaluate(() => window.__harness.template!);
        const restored = await tileAverage(page, tileUrl(restoredTemplate));

        expect(Math.abs(restored.r - original.r)).toBeLessThan(2);
        expect(Math.abs(restored.b - original.b)).toBeLessThan(2);
    });

    test('a flat colour can override a per-feature expression', async ({ page }) => {
        await page.goto('/');
        const { template } = await page.evaluate(() => window.__harness.ready);
        const before = await tileAverage(page, tileUrl(template));

        await page.evaluate(() =>
            window.__harness.setColor!('countries-fill', 'fill-color', '#00ff00'),
        );
        const nextTemplate = await page.evaluate(() => window.__harness.template!);
        const after = await tileAverage(page, tileUrl(nextTemplate));

        // Every country turns the same green, so green rises and red falls.
        expect(after.g).toBeGreaterThan(before.g);
        expect(after.r).toBeLessThan(before.r - 10);
    });
});
