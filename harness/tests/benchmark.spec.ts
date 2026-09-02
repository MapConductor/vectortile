import { expect, test } from '@playwright/test';

/**
 * Records warm render cost — source tiles already cached, so this measures
 * decode plus rasterise alone. It is the figure that decides whether the same
 * core is fast enough on an Android or iOS device.
 */
test('records warm single-tile render cost', async ({ page }) => {
    await page.goto('/');
    await page.evaluate(() => window.__harness.ready);

    const result = await page.evaluate(() => window.__harness.benchmark!(2, 16));
    console.log(
        `warm 512px tile render — median ${result.medianMs.toFixed(1)} ms, ` +
            `min ${result.minMs.toFixed(1)} ms, max ${result.maxMs.toFixed(1)} ms ` +
            `(n=${result.count})`,
    );

    // Loose bound: this is a regression tripwire, not a performance target.
    expect(result.medianMs).toBeLessThan(2000);

    await page.waitForTimeout(2500);
    await page.screenshot({ path: 'test-results/raster-layer.png' });
});
