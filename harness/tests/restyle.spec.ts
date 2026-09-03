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
    test('derives colour controls from the style, skipping undrawable layers', async ({
        page,
    }) => {
        await page.goto('/');
        await page.evaluate(() => window.__harness.ready);

        const controls = await page.evaluate(() => window.__harness.listColors!());
        const ids = controls.map((c) => `${c.layerId}/${c.property}`);

        // A real basemap style has hundreds of colour properties.
        expect(controls.length).toBeGreaterThan(100);
        expect(ids).toContain('background/background-color');

        // Symbol layers are never drawn, so they must not get a dead control.
        const symbolish = ids.filter((id) => /label|-name|text/.test(id));
        expect(symbolish).toEqual([]);

        // One row per control, and the filter narrows them.
        await expect(page.locator('#colors .row')).toHaveCount(controls.length);
        await page.locator('#colour-filter').fill('water');
        const visible = page.locator('#colors .row:visible');
        await expect(visible).not.toHaveCount(controls.length);
        expect(await visible.count()).toBeGreaterThan(0);
    });

    test('recolouring changes the tiles without refetching vector data', async ({ page }) => {
        await page.goto('/');
        const { template } = await page.evaluate(() => window.__harness.ready);

        const before = await tileAverage(page, tileUrl(template));

        // The map is still pulling its first viewport at this zoom. Wait for it
        // to settle, or the counter below measures the map's loading rather
        // than what the restyle caused.
        await page.evaluate(() => window.__harness.mapIdle);

        const vectorRequests: string[] = [];
        page.on('request', (request) => {
            const url = request.url();
            if (url.includes('.mvt') || url.includes('.pbf')) vectorRequests.push(url);
        });

        const elapsedMs = await page.evaluate(() =>
            window.__harness.setColor!('background', 'background-color', '#ff0000'),
        );
        console.log(`restyle applied in ${elapsedMs.toFixed(1)} ms`);

        const nextTemplate = await page.evaluate(() => window.__harness.template!);
        expect(nextTemplate).not.toBe(template);
        const after = await tileAverage(page, tileUrl(nextTemplate));

        // Background shows through wherever nothing else covers it, so turning
        // it red must move the average toward red and away from the others.
        expect(after.r).toBeGreaterThan(before.r);
        expect(after.g).toBeLessThan(before.g);

        // The whole point: geometry is unchanged, so nothing is refetched.
        expect(vectorRequests).toEqual([]);
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
        // Shortbread's style has no expression-valued colours, so this case
        // needs the MapLibre demo style, whose country fill is a `match`.
        await page.goto('/?style=demo');
        const { template } = await page.evaluate(() => window.__harness.ready);

        const controls = await page.evaluate(() => window.__harness.listColors!());
        const expression = controls.find((c) => c.isExpression);
        expect(expression, 'demo style should have an expression-valued colour').toBeTruthy();
        expect(expression!.layerId).toBe('countries-fill');

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
