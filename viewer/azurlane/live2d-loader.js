(function attachAzurLaneLive2D(globalScope) {
  'use strict';

  const DEFAULT_STABLE_FRAMES = 3;
  const DEFAULT_MAX_DIMENSION_FRAMES = 90;
  const DEFAULT_DIMENSION_EPSILON = 0.5;

  function isFinitePositive(value) {
    return Number.isFinite(value) && value > 0;
  }

  function firstFinitePositive(...values) {
    return values.find(isFinitePositive);
  }

  function readCallableNumber(owner, methodName) {
    if (typeof owner?.[methodName] !== 'function') {
      return undefined;
    }

    try {
      return owner[methodName]();
    } catch {
      return undefined;
    }
  }

  function normalizeBounds(bounds, source) {
    if (!bounds || !isFinitePositive(bounds.width) || !isFinitePositive(bounds.height)) {
      return null;
    }

    const x = Number.isFinite(bounds.x) ? bounds.x : 0;
    const y = Number.isFinite(bounds.y) ? bounds.y : 0;
    const width = bounds.width;
    const height = bounds.height;

    return {
      source,
      x,
      y,
      width,
      height,
      centerX: x + width / 2,
      centerY: y + height / 2,
    };
  }

  function measureLive2DModel(model) {
    if (!model) {
      return null;
    }

    const localBounds = typeof model.getLocalBounds === 'function' ? normalizeBounds(model.getLocalBounds(), 'local-bounds') : null;
    if (localBounds) {
      return localBounds;
    }

    const explicitWidth = firstFinitePositive(model.width, model.canvasWidth, model.originalWidth);
    const explicitHeight = firstFinitePositive(model.height, model.canvasHeight, model.originalHeight);
    if (explicitWidth && explicitHeight) {
      return {
        source: 'model-size',
        x: -explicitWidth / 2,
        y: -explicitHeight / 2,
        width: explicitWidth,
        height: explicitHeight,
        centerX: 0,
        centerY: 0,
      };
    }

    const internalModel = model.internalModel;
    const internalWidth = firstFinitePositive(
      internalModel?.width,
      internalModel?.originalWidth,
      internalModel?.canvasWidth,
      readCallableNumber(internalModel, 'getCanvasWidth'),
    );
    const internalHeight = firstFinitePositive(
      internalModel?.height,
      internalModel?.originalHeight,
      internalModel?.canvasHeight,
      readCallableNumber(internalModel, 'getCanvasHeight'),
    );
    if (internalWidth && internalHeight) {
      return {
        source: 'internal-model-size',
        x: -internalWidth / 2,
        y: -internalHeight / 2,
        width: internalWidth,
        height: internalHeight,
        centerX: 0,
        centerY: 0,
      };
    }

    return null;
  }

  function dimensionsAreClose(left, right, epsilon) {
    return (
      Math.abs(left.width - right.width) <= epsilon
      && Math.abs(left.height - right.height) <= epsilon
      && Math.abs(left.centerX - right.centerX) <= epsilon
      && Math.abs(left.centerY - right.centerY) <= epsilon
    );
  }

  function waitForNextFrame(options) {
    const app = options.app;
    if (typeof options.requestFrame === 'function') {
      return new Promise((resolve) => options.requestFrame(resolve));
    }
    if (typeof app?.ticker?.addOnce === 'function') {
      return new Promise((resolve) => app.ticker.addOnce(resolve));
    }
    if (typeof globalScope.requestAnimationFrame === 'function') {
      return new Promise((resolve) => globalScope.requestAnimationFrame(() => resolve()));
    }
    return new Promise((resolve) => setTimeout(resolve, 0));
  }

  async function waitForStableLive2DDimensions(model, options = {}) {
    const stableFrames = options.stableFrames ?? DEFAULT_STABLE_FRAMES;
    const maxFrames = options.maxFrames ?? DEFAULT_MAX_DIMENSION_FRAMES;
    const epsilon = options.epsilon ?? DEFAULT_DIMENSION_EPSILON;
    let previous = null;
    let lastMeasurement = null;
    let stableCount = 0;

    for (let frame = 0; frame < maxFrames; frame += 1) {
      await waitForNextFrame(options);

      const measurement = measureLive2DModel(model);
      if (!measurement) {
        stableCount = 0;
        previous = null;
        continue;
      }

      if (previous && dimensionsAreClose(measurement, previous, epsilon)) {
        stableCount += 1;
      } else {
        stableCount = 1;
      }

      previous = measurement;
      lastMeasurement = measurement;

      if (stableCount >= stableFrames) {
        return {
          ...measurement,
          frames: frame + 1,
          stableFrames: stableCount,
          timedOut: false,
        };
      }
    }

    if (lastMeasurement) {
      return {
        ...lastMeasurement,
        frames: maxFrames,
        stableFrames: stableCount,
        timedOut: true,
      };
    }

    throw new Error('Live2D model dimensions did not become measurable');
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

  function setAnchor(model, anchor) {
    if (!model?.anchor) {
      return;
    }

    const [anchorX, anchorY] = anchor;
    if (typeof model.anchor.set === 'function') {
      model.anchor.set(anchorX, anchorY);
      return;
    }

    model.anchor.x = anchorX;
    model.anchor.y = anchorY;
  }

  function numberOrDefault(value, defaultValue) {
    return Number.isFinite(value) ? value : defaultValue;
  }

  function camelOrSnake(item, camelName, snakeName) {
    return item?.[camelName] ?? item?.[snakeName];
  }

  function normalizeLayout(layout) {
    const anchor = Array.isArray(layout?.anchor) && layout.anchor.length >= 2 ? layout.anchor : [0.5, 0.5];
    return {
      anchor: [numberOrDefault(anchor[0], 0.5), numberOrDefault(anchor[1], 0.5)],
      scaleOverride: camelOrSnake(layout, 'scaleOverride', 'scale_override'),
      offsetX: camelOrSnake(layout, 'offsetX', 'offset_x'),
      offsetY: camelOrSnake(layout, 'offsetY', 'offset_y'),
    };
  }

  function resolveStage(stageLayout) {
    const designWidth = stageLayout?.DESIGN_WIDTH;
    const designHeight = stageLayout?.DESIGN_HEIGHT;
    const centerX = stageLayout?.DESIGN_CENTER_X;
    const centerY = stageLayout?.DESIGN_CENTER_Y;

    if (!isFinitePositive(designWidth) || !isFinitePositive(designHeight) || !Number.isFinite(centerX) || !Number.isFinite(centerY)) {
      throw new TypeError('stageLayout must expose finite design dimensions and center coordinates');
    }

    return { designWidth, designHeight, centerX, centerY };
  }

  function prepareLive2DModelForMeasurement(model, layout = {}) {
    const normalizedLayout = normalizeLayout(layout);
    setAnchor(model, normalizedLayout.anchor);
    setUniformScale(model.scale, 1);
    return normalizedLayout;
  }

  function fitLive2DModelIntoStage(model, dimensions, options = {}) {
    if (!dimensions || !isFinitePositive(dimensions.width) || !isFinitePositive(dimensions.height)) {
      throw new TypeError('Live2D dimensions must contain positive width and height');
    }

    const stage = resolveStage(options.stageLayout ?? globalScope.AzurLaneStageLayout);
    const layout = normalizeLayout(options.layout);
    setAnchor(model, layout.anchor);

    const autoScale = Math.min(stage.designWidth / dimensions.width, stage.designHeight / dimensions.height);
    const scale = isFinitePositive(layout.scaleOverride) ? layout.scaleOverride : autoScale;
    const offsetX = numberOrDefault(layout.offsetX, 0);
    const offsetY = numberOrDefault(layout.offsetY, 0);
    const centerX = Number.isFinite(dimensions.centerX) ? dimensions.centerX : 0;
    const centerY = Number.isFinite(dimensions.centerY) ? dimensions.centerY : 0;
    const x = stage.centerX - centerX * scale + offsetX;
    const y = stage.centerY - centerY * scale + offsetY;

    setUniformScale(model.scale, scale);
    setPoint(model.position, x, y);

    return {
      x,
      y,
      scale,
      autoScale,
      offsetX,
      offsetY,
      stageWidth: stage.designWidth,
      stageHeight: stage.designHeight,
      dimensions,
    };
  }

  function resolveLive2DModelUrl(entry, options = {}) {
    if (typeof options.modelUrl === 'string' && options.modelUrl.trim()) {
      return options.modelUrl.trim();
    }
    if (typeof entry === 'string' && entry.trim()) {
      return entry.trim();
    }

    const resources = entry?.resources ?? {};
    const availability = entry?.availability ?? {};
    const validatedUrl = camelOrSnake(availability, 'validatedUrl', 'validated_url');
    const primaryUrl = camelOrSnake(resources, 'primaryUrl', 'primary_url');
    const fallbackUrl = camelOrSnake(resources, 'fallbackUrl', 'fallback_url');

    if (typeof validatedUrl === 'string' && validatedUrl.trim()) {
      return validatedUrl.trim();
    }
    if (options.preferFallback && typeof fallbackUrl === 'string' && fallbackUrl.trim()) {
      return fallbackUrl.trim();
    }
    if (typeof primaryUrl === 'string' && primaryUrl.trim()) {
      return primaryUrl.trim();
    }
    if (typeof fallbackUrl === 'string' && fallbackUrl.trim()) {
      return fallbackUrl.trim();
    }
    return '';
  }

  function resolveLive2DRuntime(options = {}) {
    const runtime = options.runtime ?? options.live2dRuntime ?? globalScope.PIXI?.live2d;
    const Live2DModel = runtime?.Live2DModel ?? options.Live2DModel;

    if (!Live2DModel || typeof Live2DModel.from !== 'function') {
      throw new Error('Live2D runtime is unavailable; expected PIXI.live2d.Live2DModel.from');
    }

    return {
      runtime,
      Live2DModel,
    };
  }

  function removeLayerChildren(layer) {
    const children = Array.from(layer?.children ?? []);
    for (const child of children) {
      if (typeof layer.removeChild === 'function') {
        layer.removeChild(child);
      }
      if (typeof child.destroy === 'function') {
        child.destroy({ children: true });
      }
    }
  }

  function shouldContinueLoad(options) {
    return typeof options.shouldContinueLoad !== 'function' || options.shouldContinueLoad();
  }

  function abortStaleLoad(model, layer) {
    if (typeof layer?.removeChild === 'function' && model?.parent === layer) {
      layer.removeChild(model);
    }
    if (typeof model?.destroy === 'function') {
      model.destroy({ children: true });
    }
    throw new DOMException('Live2D load was superseded by a newer selection', 'AbortError');
  }

  async function loadLive2DEntry(entry, options = {}) {
    if (entry?.type && entry.type !== 'live2d') {
      throw new TypeError(`Expected a live2d entry, received ${entry.type}`);
    }
    if (!options.live2dLayer || typeof options.live2dLayer.addChild !== 'function') {
      throw new TypeError('live2dLayer with addChild() is required');
    }

    const modelUrl = resolveLive2DModelUrl(entry, options);
    if (!modelUrl) {
      throw new Error('Live2D entry does not contain a model URL');
    }

    const { Live2DModel } = resolveLive2DRuntime(options);
    if (options.clearLayer ?? true) {
      removeLayerChildren(options.live2dLayer);
    }

    const modelOptions = {
      autoInteract: false,
      autoFocus: false,
      autoHitTest: false,
      ...(options.modelOptions ?? {}),
    };
    const model = await Live2DModel.from(modelUrl, modelOptions);
    if (!shouldContinueLoad(options)) {
      abortStaleLoad(model, options.live2dLayer);
    }

    const layout = prepareLive2DModelForMeasurement(model, entry?.layout);
    const previousVisibility = model.visible;
    model.visible = false;
    options.live2dLayer.addChild(model);

    const dimensions = await waitForStableLive2DDimensions(model, {
      app: options.app,
      requestFrame: options.requestFrame,
      ...(options.dimensionOptions ?? {}),
    });
    if (!shouldContinueLoad(options)) {
      abortStaleLoad(model, options.live2dLayer);
    }

    const fit = fitLive2DModelIntoStage(model, dimensions, {
      stageLayout: options.stageLayout,
      layout,
    });
    model.visible = previousVisibility !== false;

    return {
      entry,
      model,
      modelUrl,
      dimensions,
      fit,
    };
  }

  const api = Object.freeze({
    DEFAULT_STABLE_FRAMES,
    DEFAULT_MAX_DIMENSION_FRAMES,
    DEFAULT_DIMENSION_EPSILON,
    fitLive2DModelIntoStage,
    loadLive2DEntry,
    measureLive2DModel,
    prepareLive2DModelForMeasurement,
    removeLayerChildren,
    resolveLive2DModelUrl,
    waitForStableLive2DDimensions,
  });

  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
    return;
  }

  globalScope.AzurLaneLive2D = api;
})(typeof globalThis !== 'undefined' ? globalThis : window);
