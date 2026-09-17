/**
 * Covers what the Web binding gained when it caught up with Android: the split
 * ground/label layers, glyph warming, and the credits a style asks for.
 *
 * Drives `TileRenderCore` rather than `VectorTileProvider`: the provider's only
 * extra job in Node is choosing between a Worker and this, and there is no
 * Worker here. Every fetch is answered from a fixture and counted, because half
 * of what this layer does is decide what *not* to ask for.
 */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { after, before, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

const here = new URL('./', import.meta.url);
const read = (name) => readFile(fileURLToPath(new URL(`fixtures/${name}`, here)));

/**
 * Not z0: the demo style gives `geolines-label` a minzoom of 1 and
 * `countries-label` a minzoom of 2, so at z0 no symbol layer applies and there
 * is correctly nothing to label.
 */
const target = { z: 2, x: 1, y: 1 };

const isPng = (bytes) =>
    !!bytes && bytes.length > 4 && [...bytes.slice(0, 4)].join() === '137,80,78,71';

describe('split layers', () => {
    let TileRenderCore;
    let styleText;
    let tile;
    let glyphs;
    const created = [];

    /** Answers source tiles and glyph ranges from the fixtures. */
    const core = async (options = {}) => {
        const made = await TileRenderCore.create({
            style: styleText,
            tileSize: 256,
            wasmUrl: { module_or_path: await read('../../pkg/mvt_render_wasm_bg.wasm') },
            fetchTile: async (url) => {
                options.onFetch?.(url);
                // Source tiles and glyph ranges are both .pbf on the same
                // host; only the path separates them.
                if (url.includes('/font/')) return options.starve ? null : glyphs;
                return tile;
            },
        });
        created.push(made);
        return made;
    };

    before(async () => {
        ({ TileRenderCore } = await import('../dist/index.js'));
        styleText = await readFile(
            fileURLToPath(new URL('fixtures/demo-style.json', here)), 'utf8',
        );
        tile = new Uint8Array(await read('tile-0-0-0.pbf'));
        glyphs = new Uint8Array(await read('glyphs-0-255.pbf'));
    });

    after(() => created.forEach((c) => c.dispose()));

    /** Waits for something the core does on its own. */
    const eventually = async (what, condition, timeout = 10_000) => {
        const deadline = Date.now() + timeout;
        while (!condition() && Date.now() < deadline) {
            await new Promise((done) => setTimeout(done, 25));
        }
        assert.ok(condition(), what);
    };

    it('draws both halves and the two-in-one tile', async () => {
        const subject = await core();
        assert.ok(isPng(await subject.renderTile(target, 'ground')), 'ground');
        assert.ok(isPng(await subject.renderTile(target, 'labels')), 'labels');
        assert.ok(isPng(await subject.renderTile(target, 'full')), 'both in one');
    });

    /**
     * The point of the split: the ground is drawn once and a font landing must
     * not change it. If it did, every glyph range would redraw the whole map
     * rather than the transparent half.
     */
    it('leaves the ground alone when glyphs arrive', async () => {
        const subject = await core();
        const before = await subject.renderTile(target, 'ground');
        await subject.renderTile(target, 'labels');
        await eventually('glyphs never arrived', () => subject.glyphGeneration > 0);
        assert.deepEqual(await subject.renderTile(target, 'ground'), before);
    });

    /**
     * Compared against a core whose glyph fetches fail rather than an earlier
     * render of the same one: the fixture answers instantly, so the range lands
     * before the first tile is drawn and there is no "before" to catch.
     */
    it('needs its glyphs before it draws a label', async () => {
        const fed = await core();
        await fed.renderTile(target, 'labels');
        await eventually('glyphs never arrived', () => fed.glyphGeneration > 0);
        const labelled = await fed.renderTile(target, 'labels');

        const starved = await core({ starve: true });
        const bare = await starved.renderTile(target, 'labels');

        assert.ok(isPng(labelled) && isPng(bare));
        assert.ok(
            labelled.length > bare.length,
            `labelled ${labelled.length} vs starved ${bare.length}`,
        );
    });

    /**
     * The ground never draws a label, so the ring of neighbours the label pass
     * asks for is nine fetches it has no use for.
     */
    it('skips the label-only neighbours when drawing the ground', async () => {
        const ground = new Set();
        await (await core({ onFetch: (u) => ground.add(u) })).renderTile(target, 'ground');

        const labels = new Set();
        await (await core({ onFetch: (u) => labels.add(u) })).renderTile(target, 'labels');

        const only = (set) => [...set].filter((u) => !u.includes('/font/'));
        assert.ok(
            only(ground).length < only(labels).length,
            `ground fetched ${only(ground).length}, labels ${only(labels).length}`,
        );
    });

    /** A range claimed by one tile must not be fetched again by the next. */
    it('claims each glyph range once', async () => {
        const counts = new Map();
        const subject = await core({
            onFetch: (u) => counts.set(u, (counts.get(u) ?? 0) + 1),
        });
        await subject.renderTile(target, 'labels');
        await eventually('glyphs never arrived', () => subject.glyphGeneration > 0);
        await subject.renderTile(target, 'labels');

        const twice = [...counts].filter(([u, n]) => u.includes('/font/') && n > 1);
        assert.deepEqual(twice, [], 'a glyph range was fetched more than once');
    });

    /** The host is told once for a burst, not once per range. */
    it('reports glyph arrival to the host', async () => {
        const subject = await core();
        let told = 0;
        subject.onGlyphsLoaded = () => { told += 1; };
        await subject.renderTile(target, 'labels');
        await eventually('the host was never told', () => told > 0, 15_000);
        assert.equal(told, 1, 'a burst of ranges must hand over once');
    });

    it('carries the credit out of the style', async () => {
        const credited = await TileRenderCore.create({
            style: {
                version: 8,
                sources: {
                    osm: {
                        type: 'vector',
                        tiles: ['https://example.com/{z}/{x}/{y}.pbf'],
                        attribution: '&copy; OpenStreetMap',
                    },
                },
                layers: [{ id: 'w', type: 'fill', source: 'osm', 'source-layer': 'water' }],
            },
            wasmUrl: { module_or_path: await read('../../pkg/mvt_render_wasm_bg.wasm') },
            fetchTile: async () => null,
        });
        created.push(credited);
        assert.deepEqual(credited.attributions(), ['&copy; OpenStreetMap']);
    });

    it('invents no credit when the style asks for none', async () => {
        assert.deepEqual((await core()).attributions(), []);
    });
});

describe('giving up and remembering', () => {
    let TileRenderCore;
    let RenderedTileCache;
    let styleText;
    let tile;
    const created = [];

    before(async () => {
        ({ TileRenderCore, RenderedTileCache } = await import('../dist/index.js'));
        styleText = await readFile(
            fileURLToPath(new URL('fixtures/demo-style.json', here)), 'utf8',
        );
        tile = new Uint8Array(await read('tile-0-0-0.pbf'));
    });

    after(() => created.forEach((c) => c.dispose()));

    const core = async () => {
        const made = await TileRenderCore.create({
            style: styleText,
            tileSize: 256,
            wasmUrl: { module_or_path: await read('../../pkg/mvt_render_wasm_bg.wasm') },
            fetchTile: async () => tile,
        });
        created.push(made);
        return made;
    };

    /**
     * A map that has panned away is not owed the tile it stopped waiting for.
     * The renderer is a fixed resource, so a doomed tile is drawn *instead of*
     * one still on screen.
     */
    it('gives up on a tile aborted before it starts', async () => {
        const subject = await core();
        const abort = new AbortController();
        abort.abort();
        assert.equal(await subject.renderTile(target, 'full', abort.signal), null);
    });

    /** Aborting mid-fetch has to be caught too, not only at the door. */
    it('gives up on a tile aborted while its sources are in flight', async () => {
        const abort = new AbortController();
        const made = await TileRenderCore.create({
            style: styleText,
            tileSize: 256,
            wasmUrl: { module_or_path: await read('../../pkg/mvt_render_wasm_bg.wasm') },
            fetchTile: async () => {
                abort.abort();
                return tile;
            },
        });
        created.push(made);
        assert.equal(await made.renderTile(target, 'full', abort.signal), null);
    });

    /** A signal nobody trips must not change anything. */
    it('draws normally when the signal is never aborted', async () => {
        const subject = await core();
        const abort = new AbortController();
        assert.ok(isPng(await subject.renderTile(target, 'full', abort.signal)));
    });

    /**
     * Cache Storage does not exist in Node, and a map that renders everything
     * fresh is slower rather than broken.
     */
    it('does without Cache Storage rather than failing', async () => {
        assert.equal(typeof caches, 'undefined', 'Node grew a Cache Storage');
        assert.equal(await RenderedTileCache.open(), null);
        const subject = await TileRenderCore.create({
            style: styleText,
            tileSize: 256,
            persistRenderedTiles: true,
            wasmUrl: { module_or_path: await read('../../pkg/mvt_render_wasm_bg.wasm') },
            fetchTile: async () => tile,
        });
        created.push(subject);
        assert.ok(isPng(await subject.renderTile(target, 'ground')));
    });
});
