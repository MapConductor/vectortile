/**
 * End-to-end harness: MapLibre style -> Rust/wasm renderer -> LocalTileServer
 * -> raster layer.
 *
 * The map here is maplibre-gl consuming a *raster* source, deliberately: it
 * stands in for any backend that cannot render a vector style itself. The tile
 * bytes it receives are exactly what Google Maps, MapKit or Cesium would get
 * through the SDK's raster layer.
 */
import { TileServerRegistry } from '@mapconductor/js-sdk-core';
import { VectorTileProvider } from '@mapconductor/vectortile';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';

const STYLE_URL = 'https://demotiles.maplibre.org/style.json';
const ROUTE_ID = 'vectortile-demo';
const TILE_SIZE = 512;

const statusEl = document.getElementById('status') as HTMLPreElement;
const lines: string[] = [];
function log(message: string): void {
    lines.push(message);
    statusEl.textContent = lines.join('\n');
}

/** Test hook: Playwright waits on this instead of racing the map. */
declare global {
    interface Window {
        __harness: {
            ready: Promise<{ template: string; renderMs: number }>;
            template?: string;
        };
    }
}

async function boot(): Promise<{ template: string; renderMs: number }> {
    const tileServer = TileServerRegistry.get();
    tileServer.startServiceWorker('/tile-sw.js');
    await tileServer.waitForController();
    log('service worker ready');

    const provider = await VectorTileProvider.create({
        style: STYLE_URL,
        tileSize: TILE_SIZE,
        onWarning: (message) => log(`warning: ${message}`),
    });
    log(`wasm renderer ready (tileSize=${TILE_SIZE})`);

    // Render one tile directly, before any map is involved. This isolates the
    // renderer from the tile-server and map plumbing when something breaks.
    const started = performance.now();
    const png = await provider.renderTile({ z: 0, x: 0, y: 0 });
    const renderMs = performance.now() - started;
    if (!png) throw new Error('provider returned no tile for 0/0/0');
    log(`direct render of 0/0/0: ${png.byteLength} bytes in ${renderMs.toFixed(1)} ms`);

    const bitmap = await createImageBitmap(new Blob([png as BlobPart], { type: 'image/png' }));
    const probe = document.getElementById('probe') as HTMLCanvasElement;
    probe.getContext('2d')?.drawImage(bitmap, 0, 0, probe.width, probe.height);

    const template = provider.attachTo(tileServer, ROUTE_ID);
    window.__harness.template = template;
    log(`serving: ${template}`);

    new maplibregl.Map({
        container: 'map',
        style: {
            version: 8,
            sources: {
                rendered: {
                    type: 'raster',
                    tiles: [template],
                    tileSize: TILE_SIZE,
                    attribution: 'MapLibre demo tiles',
                },
            },
            layers: [{ id: 'rendered', type: 'raster', source: 'rendered' }],
        },
        center: [10, 30],
        zoom: 2,
    });
    log('raster layer mounted');

    return { template, renderMs };
}

window.__harness = {
    ready: boot().catch((error) => {
        log(`FAILED: ${error}`);
        throw error;
    }),
};
