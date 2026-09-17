/**
 * Exercises the wasm surface the Web binding had none of: glyphs, sprites, the
 * split ground/label layers, and the credits a style's sources ask for.
 *
 * Runs against the bundled MapLibre demo style and a real vector tile, so it
 * needs no network. The glyph range is a generated fixture carrying a solid
 * square per printable ASCII codepoint, which is enough for the renderer to
 * place and draw the demo style's country labels.
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
 * is correctly nothing to label. z2 has both, plus the ring of neighbours the
 * label pass asks for.
 */
const target = { z: 2, x: 1, y: 1 };

describe('wasm renderer', () => {
    let wasm;
    let renderer;
    let tileBytes;
    let glyphBytes;
    let styleText;

    /** One entry per plan request, the same bundled tile answering each. */
    const tiles = () => {
        const plan = JSON.parse(renderer.plan(target.z, target.x, target.y));
        const lengths = new Uint32Array(plan.length).fill(tileBytes.byteLength);
        const data = new Uint8Array(tileBytes.byteLength * plan.length);
        plan.forEach((_, i) => data.set(tileBytes, i * tileBytes.byteLength));
        return { data, lengths };
    };

    before(async () => {
        const bytes = await readFile(
            fileURLToPath(new URL('../pkg/mvt_render_wasm_bg.wasm', here)),
        );
        wasm = await import('../pkg/mvt_render_wasm.js');
        // Node's fetch will not take a file:// URL, so the module is handed
        // over as bytes rather than a path.
        await wasm.default({ module_or_path: bytes });

        styleText = await readFile(
            fileURLToPath(new URL('fixtures/demo-style.json', here)),
            'utf8',
        );
        tileBytes = new Uint8Array(await read('tile-0-0-0.pbf'));
        glyphBytes = new Uint8Array(await read('glyphs-0-255.pbf'));
        renderer = new wasm.VectorTileRenderer(styleText);
    });

    after(() => renderer?.free());

    it('reports the style glyph template', () => {
        assert.equal(
            renderer.glyphsUrlTemplate(),
            'https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf',
        );
    });

    it('asks for the glyph ranges its labels need', () => {
        const { data, lengths } = tiles();
        const needed = JSON.parse(
            renderer.neededGlyphs(target.z, target.x, target.y, data, lengths),
        );
        assert.ok(needed.length > 0, 'the demo style labels countries');
        assert.ok(needed[0].includes('Open Sans Semibold'));
        assert.ok(needed[0].endsWith('.pbf'));
    });

    it('takes a glyph range and stops asking for it', () => {
        assert.equal(renderer.hasGlyphs(), false);
        assert.equal(renderer.addGlyphs(glyphBytes), 95);
        assert.equal(renderer.hasGlyphs(), true);

        const { data, lengths } = tiles();
        const still = JSON.parse(
            renderer.neededGlyphs(target.z, target.x, target.y, data, lengths),
        );
        assert.ok(
            !still.some((url) => url.includes('0-255')),
            '0-255 is loaded; asking again would refetch it forever',
        );
    });

    it('rejects a malformed range instead of taking the page down', () => {
        assert.throws(() => renderer.addGlyphs(new Uint8Array([0xff, 0xff, 0xff, 0xff])));
    });

    it('asks for no sprite when the style names none', () => {
        assert.deepEqual(JSON.parse(renderer.spriteUrls(2)), []);
        assert.equal(renderer.needsSprite(), false);
    });

    it('derives both sprite URLs from the style', () => {
        const styled = new wasm.VectorTileRenderer(
            '{"version": 8, "sprite": "https://example.com/sprite", "sources": {}, "layers": []}',
        );
        try {
            assert.deepEqual(JSON.parse(styled.spriteUrls(2)), [
                'https://example.com/sprite@2x.json',
                'https://example.com/sprite@2x.png',
            ]);
        } finally {
            styled.free();
        }
    });

    it('draws geometry that fonts arriving cannot change', () => {
        const bare = new wasm.VectorTileRenderer(styleText);
        try {
            const { data, lengths } = tiles();
            const before = bare.renderGeometry(target.z, target.x, target.y, 256, data, lengths);
            assert.deepEqual([...before.slice(0, 4)], [0x89, 0x50, 0x4e, 0x47], 'not a PNG');

            bare.addGlyphs(glyphBytes);
            const after = bare.renderGeometry(target.z, target.x, target.y, 256, data, lengths);
            assert.deepEqual(before, after, 'geometry must not depend on fonts');
        } finally {
            bare.free();
        }
    });

    it('carries no pixels for a tile with nothing on it', () => {
        const bare = new wasm.VectorTileRenderer(styleText);
        try {
            const { data, lengths } = tiles();
            const tile = bare.renderLabels(target.z, target.x, target.y, 256, data, lengths);
            try {
                assert.equal(tile.placed, 0, 'no glyphs, no labels');
                assert.equal(tile.takePixels().byteLength, 0);
            } finally {
                tile.free();
            }
        } finally {
            bare.free();
        }
    });

    it('draws premultiplied labels once its glyphs are in', () => {
        const { data, lengths } = tiles();
        const size = 256;
        const tile = renderer.renderLabels(target.z, target.x, target.y, size, data, lengths);
        try {
            assert.ok(tile.placed > 0, 'the demo style labels countries');
            const pixels = tile.takePixels();
            assert.equal(pixels.byteLength, size * size * 4);
            assert.ok(pixels.some((byte) => byte !== 0), 'the tile is blank');
            // Premultiplied: no pixel may carry more colour than alpha.
            for (let i = 0; i < pixels.length; i += 4) {
                assert.ok(pixels[i] <= pixels[i + 3], `colour exceeds alpha at ${i}`);
            }
        } finally {
            tile.free();
        }
    });

    it('draws labels over pixels the caller already has', () => {
        const { data, lengths } = tiles();
        const size = 256;
        const readback = new Uint8Array(size * size * 4);
        for (let i = 0; i < readback.length; i += 4) {
            readback[i] = 255;
            readback[i + 3] = 255;
        }
        const before = readback.slice();
        const placed = renderer.drawLabels(
            target.z, target.x, target.y, size, readback, data, lengths,
        );
        assert.ok(placed > 0);
        assert.notDeepEqual(readback, before, 'nothing was drawn onto the readback');
    });

    it('carries the credit out of the style, HTML and all', () => {
        const credited = new wasm.VectorTileRenderer(
            JSON.stringify({
                version: 8,
                sources: {
                    osm: {
                        type: 'vector',
                        tiles: ['https://example.com/{z}/{x}/{y}.pbf'],
                        attribution: '&copy; <a href="https://osm.org/copyright">OSM</a>',
                    },
                },
                layers: [{ id: 'w', type: 'fill', source: 'osm', 'source-layer': 'water' }],
            }),
        );
        try {
            assert.deepEqual(JSON.parse(credited.attributions()), [
                '&copy; <a href="https://osm.org/copyright">OSM</a>',
            ]);
        } finally {
            credited.free();
        }
    });

    it('invents no credit when the style asks for none', () => {
        assert.deepEqual(JSON.parse(renderer.attributions()), []);
    });

    it('says the GPU path can draw a tile with no pattern in it', () => {
        const { data, lengths } = tiles();
        assert.equal(renderer.needsCpu(target.z, data, lengths), false);
    });
});
