# Azur Lane Viewer Shell

This directory contains the isolated PixiJS shell for the Azur Lane Live2D/Spine viewer.

The shell is intentionally static for this step:

- PixiJS is loaded from a pinned CDN URL.
- Pixi Sound, Live2D Cubism Core, and a PixiJS v8 Live2D runtime are loaded from pinned CDN URLs.
- A PixiJS v8 Spine 3.8-compatible runtime is loaded from a pinned CDN URL, with local Pixi v8 compatibility shims for current l2d.su Spine assets.
- Controls are reserved in DOM under `#viewer-controls`.
- Pixi owns the runtime canvas layers only.
- The runtime content root uses a fixed 1600x900 logical stage and scales into the browser viewport.
- Live2D entries can be loaded through `window.azurLaneViewerShell.loadLive2DEntry(entry)`.
- Loaded Live2D models are added only to `live2dLayer` and auto-fit after their dimensions are stable.
- Spine entries can be loaded through `window.azurLaneViewerShell.loadSpineEntry(entry)`.
- Loaded Spine models are added only to `spineLayer`, use `.skel`, `.atlas`, and texture assets derived from the catalog base path, and auto-fit from runtime bounds.
- The DOM controls fetch and normalize the l2d.su Azur Lane catalog, then provide a utilitarian model picker with search and Live2D/Spine filters.
- Loaded models can be dragged and zoomed on the fixed logical stage; per-entry transforms are saved in browser storage and can be cleared with Reset.
- Exceptional model framing can be corrected in `model-overrides.js` by entry id without editing upstream catalog records.
- Add `?debugStage=1` to show the logical stage rectangle and center cross.
- Broken catalog entries are hidden by default; add `?debugCatalog=1` to include them.

Open `index.html` directly in a browser, or serve the directory with:

```bash
python -m http.server 5174 --directory viewer/azurlane
```

## Visual Regression Checklist

The fixed visual smoke set lives in `visual-regression-set.js` and is checked by `tests/visual-regression.browser.test.mjs`.

Model set:

- `azurlane:live2d:xingdengbao:xingdengbao_2`: shared older Nagami/l2d.su Live2D model.
- `azurlane:live2d:yuanchou:yuanchou_3`: newer l2d.su Live2D-only model.
- `azurlane:spine:aerbien:aerbien_4`: Spine/Dynamic model with visible background attachments.
- `azurlane:spine:yilisi:yilisi_2_doa`: large wide aspect-ratio model.

Viewport matrix:

- `1600x900`
- `1920x1080`
- `1366x768`
- `390x844`

Run the deterministic browser check with the local Playwright install used by the viewer tests:

```bash
NODE_PATH=/tmp/fav-playwright-GGKkpk/node_modules node --test viewer/azurlane/tests/visual-regression.browser.test.mjs
```

The test uses fake Live2D and Spine runtimes, captures the viewer canvas at every model/viewport pair, and checks:

- the canvas is not blank by counting deterministic marker pixels;
- Spine background attachment markers are visible;
- model logical center and scale do not drift across viewport resizes;
- projected model bounds stay inside the fixed `1600x900` logical stage.

To save local screenshots and a JSON report for manual comparison, set `AZURLANE_VISUAL_OUTPUT_DIR`:

```bash
AZURLANE_VISUAL_OUTPUT_DIR=/tmp/azurlane-visual \
NODE_PATH=/tmp/fav-playwright-GGKkpk/node_modules \
node --test viewer/azurlane/tests/visual-regression.browser.test.mjs
```

Generated screenshots are local artifacts only and should not be committed unless a future review explicitly needs image baselines.
