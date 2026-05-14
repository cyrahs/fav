# Azur Lane Viewer Shell

This directory contains the isolated PixiJS shell for the Azur Lane Live2D/Spine viewer.

The shell is intentionally static for this step:

- PixiJS is loaded from a pinned CDN URL.
- Pixi Sound, Live2D Cubism Core, and a PixiJS v8 Live2D runtime are loaded from pinned CDN URLs.
- Controls are reserved in DOM under `#viewer-controls`.
- Pixi owns the runtime canvas layers only.
- The runtime content root uses a fixed 1600x900 logical stage and scales into the browser viewport.
- Live2D entries can be loaded through `window.azurLaneViewerShell.loadLive2DEntry(entry)`.
- Loaded Live2D models are added only to `live2dLayer` and auto-fit after their dimensions are stable.
- Add `?debugStage=1` to show the logical stage rectangle and center cross.
- No model catalog, picker UI, or Spine resources are loaded.

Open `index.html` directly in a browser, or serve the directory with:

```bash
python -m http.server 5174 --directory viewer/azurlane
```
