/**
 * The wasm surface, against the same style and rules the Rust golden tests
 * use. What is being checked is not the compiler -- that has its own tests
 * in Rust -- but that the binding carries the answer across unchanged.
 */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { before, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

import { compileStyle, describeStyle, loadStyleWasm, rulesSchemaVersion, VectorStyleError } from '../dist/index.js';

const here = new URL('./', import.meta.url);
const fixture = (name) =>
    readFile(fileURLToPath(new URL(`../../../crates/mvt-style/tests/fixtures/${name}`, here)), 'utf8');

const ROAD_EMPHASIS = {
    schemaVersion: 1,
    rules: [
        { selector: 'all', patch: { color: '#000000' } },
        { selector: { role: 'label' }, patch: { visible: false } },
        { selector: { role: 'road-casing' }, patch: { color: '#303030' } },
        { selector: { role: 'road' }, patch: { color: '#ffffff', widthScale: 1.3 } },
    ],
};

describe('@mapconductor/vectorstyle', () => {
    // Node cannot `fetch` a local file, which is how the browser build finds
    // its binary, so the bytes are handed over directly.
    before(async () => {
        await loadStyleWasm(
            await readFile(fileURLToPath(new URL('../pkg/mvt_style_wasm_bg.wasm', here))),
        );
    });

    it('adjusts a real style and says what it did', async () => {
        const out = await compileStyle(await fixture('versatiles-colorful.json'), ROAD_EMPHASIS);

        assert.equal(out.affects, 'both');
        assert.equal(out.patchable, true);
        // The same counts the Rust golden records, reached through wasm.
        assert.ok(
            out.diagnostics.some((d) => d.includes('schema: shortbread')),
            out.diagnostics.join('\n'),
        );
        assert.ok(out.diagnostics.some((d) => d.includes('rule 3 matched 77 layers')));

        const mutations = JSON.parse(out.mutationsJson);
        assert.ok(mutations.length > 400, `only ${mutations.length} mutations`);
        // Every mutation carries what was there before, or nothing could be
        // taken back.
        for (const mutation of mutations.slice(0, 50)) {
            assert.ok('previous' in mutation || mutation.op === 'addLayer', JSON.stringify(mutation));
        }

        const layers = out.style.layers;
        assert.equal(layers.length, 280, 'no layer was added or lost');
    });

    it('leaves alone what no rule named', async () => {
        const text = await fixture('versatiles-colorful.json');
        const before = JSON.parse(text);
        const out = await compileStyle(text, {
            schemaVersion: 1,
            rules: [{ selector: { role: 'water' }, patch: { color: '#000' } }],
        });
        assert.deepEqual(out.style.sources, before.sources);
        assert.deepEqual(out.style.metadata, before.metadata);
    });

    it('describes what is in a style', async () => {
        const layers = await describeStyle(await fixture('versatiles-colorful.json'));
        assert.equal(layers.length, 280);
        const casing = layers.find((l) => l.id.endsWith(':outline') && l.roles.includes('road'));
        assert.ok(casing, 'no road casing found');
        assert.deepEqual(casing.roles, ['road', 'road-casing']);
        assert.equal(casing.evidence[0], 'shortbread:streets');
    });

    // A rule set written for a newer build must be refused loudly, not
    // applied in part -- this is the one failure the web can actually hit,
    // because the wrapper and the wasm are versioned separately.
    it('refuses a rules document it does not understand', async () => {
        await assert.rejects(
            () => compileStyle('{"version":8,"sources":{},"layers":[]}', { schemaVersion: 99, rules: [] }),
            (error) => error instanceof VectorStyleError && /schemaVersion/.test(error.message),
        );
    });

    it('refuses a style it cannot read', async () => {
        await assert.rejects(
            () => compileStyle('{ not json', { schemaVersion: 1, rules: [] }),
            VectorStyleError,
        );
    });

    it('reports the rules version it was built with', async () => {
        assert.equal(await rulesSchemaVersion(), 1);
    });
});
