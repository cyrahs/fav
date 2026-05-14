(function attachAzurLaneViewerShell(globalScope) {
  'use strict';

  const BACKGROUND_COLOR = 0x151815;
  const BACKGROUND_LABEL = 'canvasBackgroundLayer';
  const CONTENT_ROOT_LABEL = 'contentRoot';
  const SPINE_LAYER_LABEL = 'spineLayer';
  const LIVE2D_LAYER_LABEL = 'live2dLayer';
  const STAGE_DEBUG_LAYER_LABEL = 'stageDebugLayer';
  const OVERLAY_LAYER_LABEL = 'overlayLayer';
  const DEBUG_LINE_COLOR = 0x7ed0b2;
  const DEBUG_CENTER_COLOR = 0xf5d76e;
  const TRANSFORM_STORAGE_PREFIX = 'azurlane-viewer-transform:';
  const MIN_MODEL_SCALE = 0.02;
  const MAX_MODEL_SCALE = 12;

  function setLayerLabel(container, label) {
    container.label = label;
    container.name = label;
    return container;
  }

  function resolveElement(elementOrSelector, fallbackSelector) {
    if (typeof elementOrSelector === 'string') {
      return document.querySelector(elementOrSelector);
    }

    return elementOrSelector ?? document.querySelector(fallbackSelector);
  }

  function setStatusText(controlsRoot, text) {
    const status = controlsRoot?.querySelector?.('#viewer-state');
    if (status) {
      status.textContent = text;
    }
  }

  function setResetButtonEnabled(controlsRoot, enabled) {
    const button = controlsRoot?.querySelector?.('#reset-transform');
    if (button) {
      button.disabled = !enabled;
    }
  }

  function setPoint(point, x, y) {
    if (typeof point?.set === 'function') {
      point.set(x, y);
      return;
    }
    if (!point) {
      return;
    }
    point.x = x;
    point.y = y;
  }

  function setUniformScale(scale, value) {
    if (typeof scale?.set === 'function') {
      scale.set(value, value);
      return;
    }
    if (!scale) {
      return;
    }
    scale.x = value;
    scale.y = value;
  }

  function finiteNumberOr(value, fallback) {
    return Number.isFinite(value) ? value : fallback;
  }

  function clamp(value, minimum, maximum) {
    return Math.min(maximum, Math.max(minimum, value));
  }

  function storageKeyForEntry(entry) {
    const id = String(entry?.id ?? '').trim();
    return id ? `${TRANSFORM_STORAGE_PREFIX}${encodeURIComponent(id)}` : '';
  }

  function normalizeTransform(transform) {
    if (!transform || typeof transform !== 'object') {
      return null;
    }

    const x = Number(transform.x);
    const y = Number(transform.y);
    const scale = Number(transform.scale);
    const rotation = Number(transform.rotation);
    if (!Number.isFinite(x) || !Number.isFinite(y) || !Number.isFinite(scale) || scale <= 0) {
      return null;
    }

    return {
      x,
      y,
      scale: clamp(scale, MIN_MODEL_SCALE, MAX_MODEL_SCALE),
      ...(Number.isFinite(rotation) ? { rotation } : {}),
    };
  }

  function resolveTransformStorage(globalScope, options) {
    if (options.transformStorage !== undefined) {
      return options.transformStorage;
    }

    try {
      return globalScope.localStorage ?? null;
    } catch {
      return null;
    }
  }

  function resolveDebugStageVisible(options) {
    if (typeof options.debugStage === 'boolean') {
      return options.debugStage;
    }

    try {
      return new URLSearchParams(globalScope.location?.search ?? '').has('debugStage');
    } catch {
      return false;
    }
  }

  function drawStageDebug(graphics, stageLayout) {
    const { DESIGN_WIDTH, DESIGN_HEIGHT, DESIGN_CENTER_X, DESIGN_CENTER_Y } = stageLayout;
    const edge = 3;
    const crossArm = 42;
    const crossThickness = 3;

    graphics.clear();
    graphics.rect(0, 0, DESIGN_WIDTH, edge).fill({ color: DEBUG_LINE_COLOR, alpha: 0.76 });
    graphics.rect(0, DESIGN_HEIGHT - edge, DESIGN_WIDTH, edge).fill({ color: DEBUG_LINE_COLOR, alpha: 0.76 });
    graphics.rect(0, 0, edge, DESIGN_HEIGHT).fill({ color: DEBUG_LINE_COLOR, alpha: 0.76 });
    graphics.rect(DESIGN_WIDTH - edge, 0, edge, DESIGN_HEIGHT).fill({ color: DEBUG_LINE_COLOR, alpha: 0.76 });
    graphics.rect(DESIGN_CENTER_X - crossArm, DESIGN_CENTER_Y - crossThickness / 2, crossArm * 2, crossThickness).fill({
      color: DEBUG_CENTER_COLOR,
      alpha: 0.88,
    });
    graphics.rect(DESIGN_CENTER_X - crossThickness / 2, DESIGN_CENTER_Y - crossArm, crossThickness, crossArm * 2).fill({
      color: DEBUG_CENTER_COLOR,
      alpha: 0.88,
    });
  }

  function nextAnimationFrame() {
    return new Promise((resolve) => {
      globalScope.requestAnimationFrame(resolve);
    });
  }

  async function createAzurLaneViewerShell(options = {}) {
    if (!globalScope.PIXI) {
      throw new Error('PixiJS is required before viewer-shell.js is loaded');
    }

    if (!globalScope.AzurLaneStageLayout) {
      throw new Error('stage-layout.js is required before viewer-shell.js is loaded');
    }

    const mount = resolveElement(options.mount, '#pixi-root');
    if (!mount) {
      throw new Error('Pixi mount element was not found');
    }

    const controlsRoot = resolveElement(options.controlsRoot, '#viewer-controls');
    const app = new globalScope.PIXI.Application();

    await app.init({
      resizeTo: mount,
      preference: 'webgl',
      autoDensity: true,
      resolution: Math.min(globalScope.devicePixelRatio || 1, 2),
      antialias: true,
      background: BACKGROUND_COLOR,
      backgroundColor: BACKGROUND_COLOR,
    });
    globalScope.AzurLaneSpineRuntimeCompat?.patchApplication?.(app);

    app.canvas.dataset.viewerCanvas = 'azurlane';
    app.canvas.setAttribute('aria-label', 'Azur Lane viewer canvas');
    app.canvas.setAttribute('role', 'img');
    mount.appendChild(app.canvas);

    const canvasBackgroundLayer = setLayerLabel(new globalScope.PIXI.Container(), BACKGROUND_LABEL);
    const contentRoot = setLayerLabel(new globalScope.PIXI.Container(), CONTENT_ROOT_LABEL);
    const spineLayer = setLayerLabel(new globalScope.PIXI.Container(), SPINE_LAYER_LABEL);
    const live2dLayer = setLayerLabel(new globalScope.PIXI.Container(), LIVE2D_LAYER_LABEL);
    const stageDebugLayer = setLayerLabel(new globalScope.PIXI.Container(), STAGE_DEBUG_LAYER_LABEL);
    const overlayLayer = setLayerLabel(new globalScope.PIXI.Container(), OVERLAY_LAYER_LABEL);
    const backgroundFill = new globalScope.PIXI.Graphics();
    const stageDebugGraphics = new globalScope.PIXI.Graphics();
    let stageDebugVisible = resolveDebugStageVisible(options);

    canvasBackgroundLayer.addChild(backgroundFill);
    drawStageDebug(stageDebugGraphics, globalScope.AzurLaneStageLayout);
    stageDebugLayer.visible = stageDebugVisible;
    stageDebugLayer.eventMode = 'none';
    stageDebugLayer.addChild(stageDebugGraphics);
    contentRoot.addChild(spineLayer, live2dLayer, stageDebugLayer);
    app.stage.addChild(canvasBackgroundLayer, contentRoot, overlayLayer);

    let lastLayout = null;
    let resizeFrame = 0;
    let live2dLoadCount = 0;
    let spineLoadCount = 0;
    let catalogLoadSequence = 0;
    let activeLive2D = null;
    let activeSpine = null;
    let interactionState = null;
    const activePointers = new Map();
    const transformStorage = resolveTransformStorage(globalScope, options);

    function getActiveResult() {
      return activeLive2D ?? activeSpine ?? null;
    }

    function getActiveDisplayObject(result = getActiveResult()) {
      return result?.model ?? result?.spine ?? null;
    }

    function updateResetButton() {
      setResetButtonEnabled(controlsRoot, Boolean(getActiveResult()?.userTransformed));
    }

    function cancelInteractions() {
      for (const pointerId of activePointers.keys()) {
        app.canvas.releasePointerCapture?.(pointerId);
      }
      activePointers.clear();
      interactionState = null;
    }

    function readSavedTransform(entry) {
      const key = storageKeyForEntry(entry);
      if (!key || !transformStorage || typeof transformStorage.getItem !== 'function') {
        return null;
      }

      try {
        return normalizeTransform(JSON.parse(transformStorage.getItem(key)));
      } catch {
        return null;
      }
    }

    function writeSavedTransform(entry, transform) {
      const normalized = normalizeTransform(transform);
      const key = storageKeyForEntry(entry);
      if (!normalized || !key || !transformStorage || typeof transformStorage.setItem !== 'function') {
        return normalized;
      }

      try {
        transformStorage.setItem(key, JSON.stringify(normalized));
      } catch {
        // Browser storage can be disabled or full; the in-memory transform still applies.
      }
      return normalized;
    }

    function clearSavedTransform(entry) {
      const key = storageKeyForEntry(entry);
      if (!key || !transformStorage || typeof transformStorage.removeItem !== 'function') {
        return;
      }

      try {
        transformStorage.removeItem(key);
      } catch {
        // Storage errors should not block restoring the visible auto-fit.
      }
    }

    function readDisplayTransform(displayObject) {
      if (!displayObject) {
        return null;
      }

      const scale = finiteNumberOr(displayObject.scale?.x, 1);
      const rotation = finiteNumberOr(displayObject.rotation, 0);
      return {
        x: finiteNumberOr(displayObject.position?.x, 0),
        y: finiteNumberOr(displayObject.position?.y, 0),
        scale,
        ...(rotation ? { rotation } : {}),
      };
    }

    function applyDisplayTransform(displayObject, transform) {
      const normalized = normalizeTransform(transform);
      if (!displayObject || !normalized) {
        return null;
      }

      setPoint(displayObject.position, normalized.x, normalized.y);
      setUniformScale(displayObject.scale, normalized.scale);
      if (Number.isFinite(normalized.rotation)) {
        displayObject.rotation = normalized.rotation;
      }
      return normalized;
    }

    function markActiveTransformChanged() {
      const active = getActiveResult();
      const displayObject = getActiveDisplayObject(active);
      const transform = readDisplayTransform(displayObject);
      if (!active || !transform) {
        return null;
      }

      active.userTransformed = true;
      active.savedTransform = writeSavedTransform(active.entry, transform);
      setResetButtonEnabled(controlsRoot, true);
      app.render();
      return active.savedTransform;
    }

    function applySavedTransform(result) {
      const displayObject = getActiveDisplayObject(result);
      const savedTransform = readSavedTransform(result?.entry);
      if (!displayObject || !savedTransform) {
        if (result) {
          result.userTransformed = false;
          result.savedTransform = null;
        }
        updateResetButton();
        return false;
      }

      result.savedTransform = applyDisplayTransform(displayObject, savedTransform);
      result.userTransformed = true;
      updateResetButton();
      return true;
    }

    function resetActiveTransform() {
      const active = getActiveResult();
      const displayObject = getActiveDisplayObject(active);
      if (!active || !displayObject || !active.fit) {
        return null;
      }

      clearSavedTransform(active.entry);
      active.savedTransform = null;
      active.userTransformed = false;
      applyDisplayTransform(displayObject, {
        x: active.fit.x,
        y: active.fit.y,
        scale: active.fit.scale,
        rotation: 0,
      });
      updateResetButton();
      app.render();
      return readDisplayTransform(displayObject);
    }

    function canvasPointToLogical(clientX, clientY) {
      const rect = app.canvas.getBoundingClientRect();
      const rootScale = contentRoot.scale?.x || 1;
      return {
        x: (clientX - rect.left - contentRoot.position.x) / rootScale,
        y: (clientY - rect.top - contentRoot.position.y) / rootScale,
      };
    }

    function pointerSnapshot(event) {
      return {
        pointerId: event.pointerId,
        clientX: event.clientX,
        clientY: event.clientY,
        logical: canvasPointToLogical(event.clientX, event.clientY),
      };
    }

    function distanceBetween(left, right) {
      return Math.hypot(right.clientX - left.clientX, right.clientY - left.clientY);
    }

    function midpointLogical(left, right) {
      return canvasPointToLogical((left.clientX + right.clientX) / 2, (left.clientY + right.clientY) / 2);
    }

    function beginPinchInteraction() {
      const active = getActiveResult();
      const displayObject = getActiveDisplayObject(active);
      const pointers = Array.from(activePointers.values());
      if (!active || !displayObject || pointers.length < 2) {
        return;
      }

      const [first, second] = pointers;
      interactionState = {
        type: 'pinch',
        initialDistance: Math.max(1, distanceBetween(first, second)),
        initialTransform: readDisplayTransform(displayObject),
        initialCenter: midpointLogical(first, second),
      };
    }

    function beginDragInteraction(event) {
      const active = getActiveResult();
      const displayObject = getActiveDisplayObject(active);
      if (!active || !displayObject || event.button !== 0) {
        return false;
      }

      const pointer = pointerSnapshot(event);
      activePointers.set(event.pointerId, pointer);
      interactionState = {
        type: 'drag',
        pointerId: event.pointerId,
        startPointer: pointer,
        initialTransform: readDisplayTransform(displayObject),
      };
      app.canvas.setPointerCapture?.(event.pointerId);
      return true;
    }

    function handlePointerDown(event) {
      if (!beginDragInteraction(event)) {
        return;
      }

      event.preventDefault();
      if (activePointers.size >= 2) {
        beginPinchInteraction();
      }
    }

    function handlePointerMove(event) {
      if (!activePointers.has(event.pointerId)) {
        return;
      }

      activePointers.set(event.pointerId, pointerSnapshot(event));
      const active = getActiveResult();
      const displayObject = getActiveDisplayObject(active);
      if (!active || !displayObject || !interactionState?.initialTransform) {
        return;
      }

      event.preventDefault();
      if (activePointers.size >= 2) {
        if (interactionState.type !== 'pinch') {
          beginPinchInteraction();
        }

        const pointers = Array.from(activePointers.values());
        const [first, second] = pointers;
        const currentCenter = midpointLogical(first, second);
        const ratio = distanceBetween(first, second) / interactionState.initialDistance;
        const initial = interactionState.initialTransform;
        const scale = clamp(initial.scale * ratio, MIN_MODEL_SCALE, MAX_MODEL_SCALE);
        const scaleRatio = scale / initial.scale;
        const x = currentCenter.x - (interactionState.initialCenter.x - initial.x) * scaleRatio;
        const y = currentCenter.y - (interactionState.initialCenter.y - initial.y) * scaleRatio;

        applyDisplayTransform(displayObject, { x, y, scale, rotation: initial.rotation });
        markActiveTransformChanged();
        return;
      }

      if (interactionState.type !== 'drag' || interactionState.pointerId !== event.pointerId) {
        return;
      }

      const pointer = activePointers.get(event.pointerId);
      const initial = interactionState.initialTransform;
      applyDisplayTransform(displayObject, {
        x: initial.x + pointer.logical.x - interactionState.startPointer.logical.x,
        y: initial.y + pointer.logical.y - interactionState.startPointer.logical.y,
        scale: initial.scale,
        rotation: initial.rotation,
      });
      markActiveTransformChanged();
    }

    function finishPointerInteraction(event) {
      if (activePointers.has(event.pointerId)) {
        activePointers.delete(event.pointerId);
      }
      app.canvas.releasePointerCapture?.(event.pointerId);

      if (activePointers.size >= 2) {
        beginPinchInteraction();
        return;
      }

      if (activePointers.size === 1) {
        const [pointer] = activePointers.values();
        const displayObject = getActiveDisplayObject();
        interactionState = {
          type: 'drag',
          pointerId: pointer.pointerId,
          startPointer: pointer,
          initialTransform: readDisplayTransform(displayObject),
        };
        return;
      }

      interactionState = null;
    }

    function handleWheel(event) {
      const active = getActiveResult();
      const displayObject = getActiveDisplayObject(active);
      const initial = readDisplayTransform(displayObject);
      if (!active || !displayObject || !initial) {
        return;
      }

      event.preventDefault();
      const logicalPoint = canvasPointToLogical(event.clientX, event.clientY);
      const factor = Math.exp(-event.deltaY * 0.001);
      const scale = clamp(initial.scale * factor, MIN_MODEL_SCALE, MAX_MODEL_SCALE);
      const scaleRatio = scale / initial.scale;
      const x = logicalPoint.x - (logicalPoint.x - initial.x) * scaleRatio;
      const y = logicalPoint.y - (logicalPoint.y - initial.y) * scaleRatio;
      applyDisplayTransform(displayObject, { x, y, scale, rotation: initial.rotation });
      markActiveTransformChanged();
    }

    app.canvas.addEventListener('pointerdown', handlePointerDown);
    app.canvas.addEventListener('pointermove', handlePointerMove);
    app.canvas.addEventListener('pointerup', finishPointerInteraction);
    app.canvas.addEventListener('pointercancel', finishPointerInteraction);
    app.canvas.addEventListener('wheel', handleWheel, { passive: false });

    controlsRoot?.querySelector?.('#reset-transform')?.addEventListener('click', resetActiveTransform);
    setResetButtonEnabled(controlsRoot, false);

    async function unloadSpineAssets(result) {
      const Assets = globalScope.PIXI?.Assets;
      if (!Assets || typeof Assets.unload !== 'function') {
        return;
      }

      const aliases = result?.assetInfo?.loadAliases;
      if (!Array.isArray(aliases) || aliases.length === 0) {
        return;
      }

      try {
        await Assets.unload(aliases);
      } catch {
        // Pixi may already have released shared aliases; display objects are still destroyed by layer cleanup.
      }
    }

    function resizeShell() {
      const width = Math.max(1, app.screen.width);
      const height = Math.max(1, app.screen.height);

      backgroundFill.clear();
      backgroundFill.rect(0, 0, width, height).fill({ color: BACKGROUND_COLOR });

      lastLayout = globalScope.AzurLaneStageLayout.applyFixedStageRoot(contentRoot, width, height);
      overlayLayer.position.set(0, 0);
      overlayLayer.scale.set(1, 1);
      app.render();
      return lastLayout;
    }

    function requestResize() {
      if (resizeFrame) {
        return;
      }

      resizeFrame = globalScope.requestAnimationFrame(() => {
        resizeFrame = 0;
        app.resize();
        resizeShell();
      });
    }

    const resizeObserver = new globalScope.ResizeObserver(requestResize);
    resizeObserver.observe(mount);
    globalScope.addEventListener('resize', requestResize, { passive: true });
    resizeShell();
    setStatusText(controlsRoot, 'Shell');

    async function loadLive2DEntry(entry, loadOptions = {}) {
      if (!globalScope.AzurLaneLive2D) {
        throw new Error('live2d-loader.js is required before loading Live2D entries');
      }

      cancelInteractions();
      const previousSpine = activeSpine;
      clearSpineLayer();
      await unloadSpineAssets(previousSpine);
      setStatusText(controlsRoot, 'Loading Live2D');
      activeLive2D = null;
      updateResetButton();
      try {
        const result = await globalScope.AzurLaneLive2D.loadLive2DEntry(entry, {
          app,
          live2dLayer,
          stageLayout: globalScope.AzurLaneStageLayout,
          ...loadOptions,
        });
        if (typeof loadOptions.shouldContinueLoad === 'function' && !loadOptions.shouldContinueLoad()) {
          globalScope.AzurLaneLive2D?.removeLayerChildren?.(live2dLayer);
          throw new DOMException('Live2D load was superseded by a newer selection', 'AbortError');
        }

        activeLive2D = result;
        applySavedTransform(result);
        live2dLoadCount += 1;
        setStatusText(controlsRoot, 'Live2D');
        app.render();
        return result;
      } catch (error) {
        if (error?.name !== 'AbortError' || typeof loadOptions.shouldContinueLoad !== 'function' || loadOptions.shouldContinueLoad()) {
          setStatusText(controlsRoot, 'Live2D Error');
        }
        throw error;
      }
    }

    function clearLive2DLayer() {
      cancelInteractions();
      if (!globalScope.AzurLaneLive2D) {
        live2dLayer.removeChildren();
      } else {
        globalScope.AzurLaneLive2D.removeLayerChildren(live2dLayer);
      }
      activeLive2D = null;
      updateResetButton();
      setStatusText(controlsRoot, 'Shell');
      app.render();
    }

    async function loadSpineEntry(entry, loadOptions = {}) {
      if (!globalScope.AzurLaneSpine) {
        throw new Error('spine-loader.js is required before loading Spine entries');
      }

      cancelInteractions();
      clearLive2DLayer();
      const previousSpine = activeSpine;
      clearSpineLayer();
      await unloadSpineAssets(previousSpine);
      setStatusText(controlsRoot, 'Loading Spine');
      activeSpine = null;
      updateResetButton();
      try {
        const result = await globalScope.AzurLaneSpine.loadSpineEntry(entry, {
          app,
          spineLayer,
          stageLayout: globalScope.AzurLaneStageLayout,
          ...loadOptions,
          clearLayer: false,
        });
        if (typeof loadOptions.shouldContinueLoad === 'function' && !loadOptions.shouldContinueLoad()) {
          await unloadSpineAssets(result);
          globalScope.AzurLaneSpine?.removeLayerChildren?.(spineLayer);
          throw new DOMException('Spine load was superseded by a newer selection', 'AbortError');
        }

        activeSpine = result;
        applySavedTransform(result);
        spineLoadCount += 1;
        setStatusText(controlsRoot, 'Spine');
        await nextAnimationFrame();
        app.render();
        return result;
      } catch (error) {
        if (error?.name !== 'AbortError' || typeof loadOptions.shouldContinueLoad !== 'function' || loadOptions.shouldContinueLoad()) {
          setStatusText(controlsRoot, 'Spine Error');
        }
        throw error;
      }
    }

    function clearSpineLayer() {
      cancelInteractions();
      if (!globalScope.AzurLaneSpine) {
        spineLayer.removeChildren();
      } else {
        globalScope.AzurLaneSpine.removeLayerChildren(spineLayer);
      }
      activeSpine = null;
      updateResetButton();
      setStatusText(controlsRoot, 'Shell');
      app.render();
    }

    async function loadCatalogEntry(entry, loadOptions = {}) {
      const sequence = catalogLoadSequence + 1;
      catalogLoadSequence = sequence;
      const shouldContinueLoad = () => sequence === catalogLoadSequence;

      if (entry?.type === 'live2d') {
        return loadLive2DEntry(entry, {
          ...(loadOptions.live2d ?? loadOptions),
          shouldContinueLoad,
        });
      }

      if (entry?.type === 'spine') {
        return loadSpineEntry(entry, {
          ...(loadOptions.spine ?? loadOptions),
          shouldContinueLoad,
          onStaleAssets(assetInfo) {
            return unloadSpineAssets({ assetInfo });
          },
        });
      }

      throw new TypeError(`Unsupported catalog entry type: ${entry?.type ?? 'unknown'}`);
    }

    function live2dState() {
      const model = activeLive2D?.model;
      return {
        loadCount: live2dLoadCount,
        layerChildren: live2dLayer.children.length,
        current: model
          ? {
              entryId: activeLive2D.entry?.id ?? '',
              modelUrl: activeLive2D.modelUrl,
              visible: model.visible,
              x: model.position?.x ?? 0,
              y: model.position?.y ?? 0,
              scaleX: model.scale?.x ?? 0,
              scaleY: model.scale?.y ?? 0,
              rotation: model.rotation ?? 0,
              anchorX: model.anchor?.x ?? 0,
              anchorY: model.anchor?.y ?? 0,
              fit: activeLive2D.fit,
              dimensions: activeLive2D.dimensions,
              userTransformed: Boolean(activeLive2D.userTransformed),
              savedTransform: activeLive2D.savedTransform ?? null,
            }
          : null,
      };
    }

    function spineState() {
      const spine = activeSpine?.spine;
      return {
        loadCount: spineLoadCount,
        layerChildren: spineLayer.children.length,
        current: spine
          ? {
              entryId: activeSpine.entry?.id ?? '',
              baseUrl: activeSpine.resourceUrls?.baseUrl,
              skelUrl: activeSpine.resourceUrls?.skelUrl,
              atlasUrl: activeSpine.resourceUrls?.atlasUrl,
              textureUrls: activeSpine.resourceUrls?.textureUrls ?? [],
              visible: spine.visible,
              x: spine.position?.x ?? 0,
              y: spine.position?.y ?? 0,
              pivotX: spine.pivot?.x ?? 0,
              pivotY: spine.pivot?.y ?? 0,
              scaleX: spine.scale?.x ?? 0,
              scaleY: spine.scale?.y ?? 0,
              rotation: spine.rotation ?? 0,
              fit: activeSpine.fit,
              defaultAnimation: activeSpine.defaultAnimation,
              userTransformed: Boolean(activeSpine.userTransformed),
              savedTransform: activeSpine.savedTransform ?? null,
            }
          : null,
      };
    }

    const shell = {
      ready: true,
      controlsRuntime: 'dom',
      modelLoadingRequested: false,
      app,
      canvasBackgroundLayer,
      contentRoot,
      spineLayer,
      live2dLayer,
      stageDebugLayer,
      overlayLayer,
      loadLive2DEntry,
      loadSpineEntry,
      loadCatalogEntry,
      clearLive2DLayer,
      clearSpineLayer,
      resetActiveTransform,
      readActiveTransform() {
        return readDisplayTransform(getActiveDisplayObject());
      },
      resize: resizeShell,
      setDebugStageVisible(visible) {
        stageDebugVisible = Boolean(visible);
        stageDebugLayer.visible = stageDebugVisible;
        app.render();
      },
      destroy() {
        if (resizeFrame) {
          globalScope.cancelAnimationFrame(resizeFrame);
        }
        cancelInteractions();
        resizeObserver.disconnect();
        globalScope.removeEventListener('resize', requestResize);
        app.canvas.removeEventListener('pointerdown', handlePointerDown);
        app.canvas.removeEventListener('pointermove', handlePointerMove);
        app.canvas.removeEventListener('pointerup', finishPointerInteraction);
        app.canvas.removeEventListener('pointercancel', finishPointerInteraction);
        app.canvas.removeEventListener('wheel', handleWheel);
        clearSpineLayer();
        clearLive2DLayer();
        app.destroy(true, { children: true });
      },
      getState() {
        const canvasRect = app.canvas.getBoundingClientRect();
        const backgroundBounds = backgroundFill.getBounds();
        const stageDebugBounds = stageDebugGraphics.getBounds();
        const { DESIGN_WIDTH, DESIGN_HEIGHT, DESIGN_CENTER_X, DESIGN_CENTER_Y } = globalScope.AzurLaneStageLayout;

        return {
          ready: true,
          controlsRuntime: 'dom',
          modelLoadingRequested: live2dLoadCount > 0 || spineLoadCount > 0,
          pixiApplicationCount: 1,
          backgroundColor: BACKGROUND_COLOR,
          design: {
            width: DESIGN_WIDTH,
            height: DESIGN_HEIGHT,
            centerX: DESIGN_CENTER_X,
            centerY: DESIGN_CENTER_Y,
          },
          screen: {
            width: app.screen.width,
            height: app.screen.height,
          },
          canvasCssSize: {
            width: canvasRect.width,
            height: canvasRect.height,
          },
          stageChildren: app.stage.children.map((child) => child.label || child.name),
          contentChildren: contentRoot.children.map((child) => child.label || child.name),
          contentRoot: {
            x: contentRoot.position.x,
            y: contentRoot.position.y,
            scaleX: contentRoot.scale.x,
            scaleY: contentRoot.scale.y,
          },
          overlayLayer: {
            x: overlayLayer.position.x,
            y: overlayLayer.position.y,
            scaleX: overlayLayer.scale.x,
            scaleY: overlayLayer.scale.y,
          },
          backgroundLayerChildren: canvasBackgroundLayer.children.length,
          spineLayerChildren: spineLayer.children.length,
          live2dLayerChildren: live2dLayer.children.length,
          spine: spineState(),
          live2d: live2dState(),
          backgroundBounds: {
            x: backgroundBounds.x,
            y: backgroundBounds.y,
            width: backgroundBounds.width,
            height: backgroundBounds.height,
          },
          stageDebug: {
            visible: stageDebugVisible,
            logicalBounds: {
              x: 0,
              y: 0,
              width: DESIGN_WIDTH,
              height: DESIGN_HEIGHT,
            },
            logicalCenter: {
              x: DESIGN_CENTER_X,
              y: DESIGN_CENTER_Y,
            },
            bounds: {
              x: stageDebugBounds.x,
              y: stageDebugBounds.y,
              width: stageDebugBounds.width,
              height: stageDebugBounds.height,
            },
          },
          lastLayout,
        };
      },
    };

    globalScope.azurLaneViewerShell = shell;
    return shell;
  }

  globalScope.createAzurLaneViewerShell = createAzurLaneViewerShell;

  document.addEventListener('DOMContentLoaded', () => {
    createAzurLaneViewerShell().catch((error) => {
      setStatusText(document.querySelector('#viewer-controls'), 'Error');
      globalScope.azurLaneViewerShellError = error;
      throw error;
    });
  });
})(typeof globalThis !== 'undefined' ? globalThis : window);
