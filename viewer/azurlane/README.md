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
- Add `?debugStage=1` to show the logical stage rectangle and center cross.
- Broken catalog entries are hidden by default; add `?debugCatalog=1` to include them.

Open `index.html` directly in a browser, or serve the directory with:

```bash
python -m http.server 5174 --directory viewer/azurlane
```
