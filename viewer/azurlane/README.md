# Azur Lane Viewer Shell

This directory contains the isolated PixiJS shell for the Azur Lane Live2D/Spine viewer.

The shell is intentionally static for this step:

- PixiJS is loaded from a pinned CDN URL.
- Controls are reserved in DOM under `#viewer-controls`.
- Pixi owns the runtime canvas layers only.
- The runtime content root uses a fixed 1600x900 logical stage and scales into the browser viewport.
- Add `?debugStage=1` to show the logical stage rectangle and center cross.
- No model catalog, Live2D, or Spine resources are loaded.

Open `index.html` directly in a browser, or serve the directory with:

```bash
python -m http.server 5174 --directory viewer/azurlane
```
