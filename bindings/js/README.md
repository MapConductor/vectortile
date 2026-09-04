# @mapconductor/vectortile

Renders a MapLibre vector style to raster tiles in the browser, so map backends
that cannot display a vector style — Google Maps, MapKit, HERE, ArcGIS, Cesium —
can show one through an ordinary raster layer.

**No framework dependency, and no runtime dependencies at all.** The renderer is
a Rust core compiled to wasm, shared with the Android and iOS modules; this
package is the browser binding around it. React bindings live in
`@mapconductor/react-vectortile`, which is a thin component on top of this.

```bash
npm install @mapconductor/vectortile
```

## Use

```js
import { VectorTileProvider } from '@mapconductor/vectortile';

const provider = await VectorTileProvider.create({
    style: 'https://tiles.versatiles.org/assets/styles/colorful/style.json',
    tileSize: 512,
});

// Any object with register / unregister / urlTemplate will do — the provider
// is not coupled to a particular tile server.
const template = provider.attachTo(tileServer, 'my-route');
// -> "https://…/my-route/{z}/{x}/{y}.png", ready for a raster layer
```

Rendering runs in a Web Worker by default, so a burst of tiles does not block
the main thread.

## Restyling

`setStyle` replaces the paint without refetching anything: the vector tiles
already in memory are unchanged, only the colours applied to them, so a
recolour costs a re-rasterise and no network traffic.

```js
await provider.setStyle(recolouredStyle);
const template = provider.attachTo(tileServer, 'my-route', String(version));
```

The `cacheKey` third argument matters. Without it the new template is identical
to the old one, so the map serves the raster tiles it already has and the map
keeps its old colours — which looks exactly like a renderer that ignored
`setStyle`.

## Bundler setup

Rendering happens in a module Worker. Vite's default `worker.format` is `iife`,
which cannot code-split, so a **production** build fails with
`Invalid value "iife" for option "worker.format"`. Dev serves module workers
natively, so this only shows up at build time:

```js
// vite.config.ts
export default defineConfig({
    worker: { format: 'es' },
});
```

## Limits

Symbol layers are not drawn. Labels need cross-tile collision detection, and a
tile rendered on its own cannot know what its neighbours placed. `diagnostics()`
reports this and anything else about the current style that will not render as
intended — unresolvable sources, layers pointing at undefined sources, Mapbox
`imports`.

## Building

Built from the `mapconductor-vectortile` repo; `dist/` and `pkg/` here are
generated output. Do not edit them in the SDK — run `scripts/sync-sdk.sh`.
