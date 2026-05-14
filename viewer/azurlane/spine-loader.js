(function attachAzurLaneSpine(globalScope) {
  'use strict';

  const DEFAULT_TEXTURE_EXTENSION = 'webp';
  const DEFAULT_ANIMATION_NAMES = Object.freeze(['normal', 'idle', 'stand', 'main']);

  function isFinitePositive(value) {
    return Number.isFinite(value) && value > 0;
  }

  function numberOrDefault(value, defaultValue) {
    return Number.isFinite(value) ? value : defaultValue;
  }

  function camelOrSnake(item, camelName, snakeName) {
    return item?.[camelName] ?? item?.[snakeName];
  }

  function trimString(value) {
    return typeof value === 'string' ? value.trim() : '';
  }

  function splitUrlSuffix(value) {
    const queryIndex = value.indexOf('?');
    const hashIndex = value.indexOf('#');
    const candidates = [queryIndex, hashIndex].filter((index) => index >= 0);
    const splitIndex = candidates.length > 0 ? Math.min(...candidates) : -1;

    if (splitIndex < 0) {
      return { main: value, suffix: '' };
    }

    return {
      main: value.slice(0, splitIndex),
      suffix: value.slice(splitIndex),
    };
  }

  function lastPathSegment(value) {
    const normalized = value.replace(/\/+$/, '');
    const slashIndex = normalized.lastIndexOf('/');
    return slashIndex >= 0 ? normalized.slice(slashIndex + 1) : normalized;
  }

  function dirnameUrl(value) {
    const normalized = value.replace(/\/+$/, '');
    const slashIndex = normalized.lastIndexOf('/');
    return slashIndex >= 0 ? normalized.slice(0, slashIndex) : '';
  }

  function basenameFromUrl(value) {
    return lastPathSegment(splitUrlSuffix(value).main);
  }

  function uniqueNonEmpty(values) {
    return [...new Set(values.map(trimString).filter(Boolean))];
  }

  function resolveSpineBaseUrl(entry, options = {}) {
    const optionBaseUrl = trimString(options.baseUrl);
    if (optionBaseUrl) {
      return optionBaseUrl;
    }
    if (typeof entry === 'string') {
      return trimString(entry);
    }

    const resources = entry?.resources ?? {};
    const availability = entry?.availability ?? {};
    const validatedUrl = camelOrSnake(availability, 'validatedUrl', 'validated_url');
    const primaryUrl = camelOrSnake(resources, 'primaryUrl', 'primary_url');

    return trimString(validatedUrl) || trimString(primaryUrl);
  }

  function resolveEntryTextureUrls(entry, stemUrl, suffix, options = {}) {
    const explicitTextureUrls = options.textureUrls ?? entry?.capabilities?.textures ?? entry?.capabilities?.texture_urls;
    const textureUrls = Array.isArray(explicitTextureUrls) ? uniqueNonEmpty(explicitTextureUrls) : [];

    if (textureUrls.length > 0) {
      return textureUrls;
    }

    const textureExtension = trimString(options.textureExtension) || DEFAULT_TEXTURE_EXTENSION;
    return [`${stemUrl}.${textureExtension}${suffix}`];
  }

  function deriveSpineResourceUrls(entry, options = {}) {
    const baseUrl = resolveSpineBaseUrl(entry, options);
    if (!baseUrl) {
      throw new Error('Spine entry does not contain a base URL');
    }

    const { main, suffix } = splitUrlSuffix(baseUrl);
    const normalizedBase = main.replace(/\/+$/, '');
    let stemUrl = '';
    let fileStem = '';
    let baseDirectoryUrl = '';

    if (normalizedBase.endsWith('.skel')) {
      stemUrl = normalizedBase.slice(0, -'.skel'.length);
      fileStem = lastPathSegment(stemUrl);
      baseDirectoryUrl = dirnameUrl(stemUrl);
    } else if (normalizedBase.endsWith('.atlas')) {
      stemUrl = normalizedBase.slice(0, -'.atlas'.length);
      fileStem = lastPathSegment(stemUrl);
      baseDirectoryUrl = dirnameUrl(stemUrl);
    } else {
      baseDirectoryUrl = normalizedBase;
      const pathName = lastPathSegment(normalizedBase);
      fileStem = pathName.endsWith('-spine') ? pathName.slice(0, -'-spine'.length) : pathName;
      stemUrl = `${normalizedBase}/${fileStem}`;
    }

    const textureUrls = resolveEntryTextureUrls(entry, stemUrl, suffix, options);

    return {
      baseUrl,
      baseDirectoryUrl,
      fileStem,
      skelUrl: `${stemUrl}.skel${suffix}`,
      atlasUrl: `${stemUrl}.atlas${suffix}`,
      textureUrls,
    };
  }

  function buildAtlasImageMap(textureUrls) {
    const imageMap = {};

    for (const textureUrl of textureUrls ?? []) {
      const pageName = basenameFromUrl(textureUrl);
      if (pageName) {
        imageMap[pageName] = textureUrl;
      }
    }

    return imageMap;
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

  function normalizeBounds(bounds) {
    if (!bounds || !isFinitePositive(bounds.width) || !isFinitePositive(bounds.height)) {
      return null;
    }

    const x = Number.isFinite(bounds.x) ? bounds.x : 0;
    const y = Number.isFinite(bounds.y) ? bounds.y : 0;
    const width = bounds.width;
    const height = bounds.height;

    return {
      x,
      y,
      width,
      height,
      centerX: x + width / 2,
      centerY: y + height / 2,
    };
  }

  function measureSpineBounds(spineModel) {
    if (typeof spineModel?.update === 'function') {
      spineModel.update(0);
    }
    if (typeof spineModel?.getLocalBounds !== 'function') {
      return null;
    }

    return normalizeBounds(spineModel.getLocalBounds());
  }

  function normalizeLayout(layout) {
    return {
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

  function fitSpineModelIntoStage(spineModel, options = {}) {
    const bounds = options.bounds ?? measureSpineBounds(spineModel);
    if (!bounds) {
      throw new Error('Spine model bounds did not become measurable');
    }

    const stage = resolveStage(options.stageLayout ?? globalScope.AzurLaneStageLayout);
    const layout = normalizeLayout(options.layout);
    const autoScale = Math.min(stage.designWidth / Math.max(1, bounds.width), stage.designHeight / Math.max(1, bounds.height));
    const scale = isFinitePositive(layout.scaleOverride) ? layout.scaleOverride : autoScale;
    const offsetX = numberOrDefault(layout.offsetX, 0);
    const offsetY = numberOrDefault(layout.offsetY, 0);
    const x = stage.centerX + offsetX;
    const y = stage.centerY + offsetY;

    setPoint(spineModel.pivot, bounds.centerX, bounds.centerY);
    setUniformScale(spineModel.scale, scale);
    setPoint(spineModel.position, x, y);

    return {
      x,
      y,
      scale,
      autoScale,
      offsetX,
      offsetY,
      stageWidth: stage.designWidth,
      stageHeight: stage.designHeight,
      bounds,
    };
  }

  function listSpineAnimationNames(spineModel) {
    const animationSources = [
      spineModel?.skeleton?.data?.animations,
      spineModel?.spineData?.animations,
      spineModel?.skeletonData?.animations,
      spineModel?.state?.data?.skeletonData?.animations,
    ];

    for (const animations of animationSources) {
      if (!Array.isArray(animations) || animations.length === 0) {
        continue;
      }

      return uniqueNonEmpty(
        animations.map((animation) => {
          if (typeof animation === 'string') {
            return animation;
          }
          return animation?.name;
        }),
      );
    }

    return [];
  }

  function selectDefaultSpineAnimation(spineModel, options = {}) {
    const requestedAnimation = trimString(options.animationName);
    const names = listSpineAnimationNames(spineModel);

    if (requestedAnimation && (names.length === 0 || names.includes(requestedAnimation) || options.allowUnknownAnimation)) {
      return requestedAnimation;
    }

    const lowerNameMap = new Map(names.map((name) => [name.toLowerCase(), name]));
    const preferredNames = options.animationNames ?? DEFAULT_ANIMATION_NAMES;
    for (const preferredName of preferredNames) {
      const match = lowerNameMap.get(trimString(preferredName).toLowerCase());
      if (match) {
        return match;
      }
    }

    return names[0] ?? '';
  }

  function startDefaultSpineAnimation(spineModel, options = {}) {
    const name = selectDefaultSpineAnimation(spineModel, options);
    if (!name || typeof spineModel?.state?.setAnimation !== 'function') {
      return {
        name,
        started: false,
        loop: true,
        trackIndex: 0,
      };
    }

    spineModel.state.setAnimation(0, name, true);
    return {
      name,
      started: true,
      loop: true,
      trackIndex: 0,
    };
  }

  function resolveSpineRuntime(options = {}) {
    const runtime = options.runtime ?? options.spineRuntime ?? globalScope.spine ?? globalScope.PIXI?.spine;
    const Spine = options.Spine ?? runtime?.Spine;
    const Assets = options.assets ?? options.Assets ?? globalScope.PIXI?.Assets;

    if (!Spine) {
      throw new Error('Spine runtime is unavailable; expected a Spine class');
    }
    if (!Assets || typeof Assets.load !== 'function') {
      throw new Error('Pixi Assets loader is unavailable; expected PIXI.Assets.load');
    }

    return {
      runtime,
      Spine,
      Assets,
    };
  }

  function supportsSpineFactory(Spine) {
    return typeof Spine?.from === 'function';
  }

  function assetAliasBase(entry, resourceUrls, options = {}) {
    return trimString(options.assetAliasBase) || trimString(entry?.id) || resourceUrls.baseUrl;
  }

  function addAsset(Assets, descriptor) {
    if (typeof Assets.add !== 'function') {
      return;
    }

    try {
      Assets.add(descriptor);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (!/already|duplicate|exists/i.test(message)) {
        throw error;
      }
    }
  }

  function objectWithKeys(value) {
    return value && typeof value === 'object' && Object.keys(value).length > 0 ? value : undefined;
  }

  function assetDataWithAtlasFile(resourceUrls, options = {}) {
    return objectWithKeys({
      ...(options.skeletonData ?? {}),
      spineAtlasFile: trimString(options.spineAtlasFile) || resourceUrls.atlasUrl,
    });
  }

  function resolveLoadedAsset(loadedAssets, alias) {
    if (!loadedAssets) {
      return null;
    }
    if (loadedAssets[alias]) {
      return loadedAssets[alias];
    }
    if (loadedAssets.spineData || loadedAssets.skeletonData) {
      return loadedAssets;
    }
    return null;
  }

  function resolveLoadedSpineData(assetInfo, options = {}) {
    const loadedSkeleton = options.spineData ? { spineData: options.spineData } : resolveLoadedAsset(assetInfo.loadedAssets, assetInfo.skeletonAlias);
    const spineData = loadedSkeleton?.spineData ?? loadedSkeleton?.skeletonData ?? loadedSkeleton;

    if (!spineData || typeof spineData !== 'object') {
      throw new Error('Loaded Spine skeleton data was not available');
    }

    return spineData;
  }

  function nowMilliseconds() {
    return typeof globalScope.performance?.now === 'function' ? globalScope.performance.now() : Date.now();
  }

  function attachSpineTicker(spineModel, app, options = {}) {
    if (!app?.ticker || typeof app.ticker.add !== 'function' || typeof spineModel?.update !== 'function') {
      return null;
    }

    const maxDeltaSeconds = numberOrDefault(options.maxDeltaSeconds, 0.064);
    let lastTime = nowMilliseconds();
    const update = () => {
      const currentTime = nowMilliseconds();
      const deltaSeconds = Math.max(0, Math.min(maxDeltaSeconds, (currentTime - lastTime) / 1000));
      lastTime = currentTime;
      spineModel.update(deltaSeconds);
    };
    const cleanup = () => {
      if (typeof app.ticker.remove === 'function') {
        app.ticker.remove(update);
      }
    };

    app.ticker.add(update);
    spineModel.azurLaneTickerCleanup = cleanup;
    return cleanup;
  }

  async function loadSpineAssets(entry, resourceUrls, options = {}) {
    const { Assets, Spine } = resolveSpineRuntime(options);
    const usesFactoryRuntime = supportsSpineFactory(Spine);
    const aliasBase = assetAliasBase(entry, resourceUrls, options);
    const skeletonAlias = trimString(options.skeletonAlias) || `${aliasBase}:skeleton`;
    const atlasAlias = trimString(options.atlasAlias) || `${aliasBase}:atlas`;
    const atlasImages = buildAtlasImageMap(resourceUrls.textureUrls);
    const skeletonAsset = {
      alias: skeletonAlias,
      src: resourceUrls.skelUrl,
    };
    const atlasAsset = {
      alias: atlasAlias,
      src: resourceUrls.atlasUrl,
    };
    const loadAliases = usesFactoryRuntime ? [skeletonAlias, atlasAlias] : [skeletonAlias];

    if (!usesFactoryRuntime) {
      skeletonAsset.data = assetDataWithAtlasFile(resourceUrls, options);
    }

    const atlasData = {
      ...(options.atlasData ?? {}),
      ...(usesFactoryRuntime && Object.keys(atlasImages).length > 0 ? { images: atlasImages } : {}),
    };
    atlasAsset.data = objectWithKeys(atlasData);

    if (usesFactoryRuntime && objectWithKeys(options.skeletonData)) {
      skeletonAsset.data = objectWithKeys(options.skeletonData);
    }

    if (options.addAssets !== false) {
      addAsset(Assets, skeletonAsset);
      addAsset(Assets, atlasAsset);
    }

    const loadedAssets = await Assets.load(loadAliases);
    return {
      skeletonAlias,
      atlasAlias,
      skeletonAsset,
      atlasAsset,
      atlasImages,
      loadAliases,
      usesFactoryRuntime,
      loadedAssets,
    };
  }

  function createSpineContainer(Spine, assetInfo, options = {}) {
    if (typeof options.createSpine === 'function') {
      return options.createSpine(assetInfo);
    }

    const spineOptions = {
      skeleton: assetInfo.skeletonAlias,
      atlas: assetInfo.atlasAlias,
      autoUpdate: true,
      ...(options.spineOptions ?? {}),
    };

    if (!spineOptions.ticker && options.app?.ticker) {
      spineOptions.ticker = options.app.ticker;
    }

    if (typeof Spine.from === 'function') {
      return Spine.from(spineOptions);
    }

    const spineModel = new Spine(resolveLoadedSpineData(assetInfo, options));
    const autoUpdate = options.spineOptions?.autoUpdate ?? spineOptions.autoUpdate;
    const tickerCleanup = autoUpdate === false ? null : attachSpineTicker(spineModel, options.app, options.tickerOptions);
    if (tickerCleanup) {
      spineModel.autoUpdate = false;
    } else if (autoUpdate !== undefined) {
      spineModel.autoUpdate = Boolean(autoUpdate);
    }
    return spineModel;
  }

  function removeLayerChildren(layer) {
    const children = Array.from(layer?.children ?? []);
    for (const child of children) {
      if (typeof child.azurLaneTickerCleanup === 'function') {
        child.azurLaneTickerCleanup();
        child.azurLaneTickerCleanup = null;
      }
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

  async function releaseStaleAssets(assetInfo, options) {
    if (typeof options.onStaleAssets === 'function') {
      await options.onStaleAssets(assetInfo);
    }
  }

  function abortStaleLoad(spineModel, layer) {
    if (typeof spineModel?.azurLaneTickerCleanup === 'function') {
      spineModel.azurLaneTickerCleanup();
      spineModel.azurLaneTickerCleanup = null;
    }
    if (typeof layer?.removeChild === 'function' && spineModel?.parent === layer) {
      layer.removeChild(spineModel);
    }
    if (typeof spineModel?.destroy === 'function') {
      spineModel.destroy({ children: true });
    }
    throw new DOMException('Spine load was superseded by a newer selection', 'AbortError');
  }

  async function loadSpineEntry(entry, options = {}) {
    if (entry?.type && entry.type !== 'spine') {
      throw new TypeError(`Expected a spine entry, received ${entry.type}`);
    }
    if (!options.spineLayer || typeof options.spineLayer.addChild !== 'function') {
      throw new TypeError('spineLayer with addChild() is required');
    }

    const resourceUrls = deriveSpineResourceUrls(entry, options);
    const { Spine } = resolveSpineRuntime(options);
    if (options.clearLayer ?? true) {
      removeLayerChildren(options.spineLayer);
    }

    const assetInfo = await loadSpineAssets(entry, resourceUrls, options);
    if (!shouldContinueLoad(options)) {
      await releaseStaleAssets(assetInfo, options);
      throw new DOMException('Spine load was superseded by a newer selection', 'AbortError');
    }

    const spineModel = createSpineContainer(Spine, assetInfo, options);
    if (!shouldContinueLoad(options)) {
      abortStaleLoad(spineModel, options.spineLayer);
    }

    const previousVisibility = spineModel.visible;
    spineModel.visible = false;
    if (entry?.id) {
      spineModel.label = entry.id;
      spineModel.name = entry.id;
    }
    options.spineLayer.addChild(spineModel);
    if (!shouldContinueLoad(options)) {
      abortStaleLoad(spineModel, options.spineLayer);
    }

    const defaultAnimation = startDefaultSpineAnimation(spineModel, {
      animationName: options.animationName ?? entry?.defaultAnimation ?? entry?.default_animation,
      animationNames: options.animationNames,
      allowUnknownAnimation: options.allowUnknownAnimation,
    });
    const fit = fitSpineModelIntoStage(spineModel, {
      stageLayout: options.stageLayout,
      layout: entry?.layout,
    });
    if (!shouldContinueLoad(options)) {
      abortStaleLoad(spineModel, options.spineLayer);
    }

    spineModel.visible = previousVisibility !== false;

    return {
      entry,
      spine: spineModel,
      resourceUrls,
      assetInfo,
      defaultAnimation,
      fit,
    };
  }

  const api = Object.freeze({
    DEFAULT_ANIMATION_NAMES,
    DEFAULT_TEXTURE_EXTENSION,
    buildAtlasImageMap,
    deriveSpineResourceUrls,
    fitSpineModelIntoStage,
    listSpineAnimationNames,
    loadSpineAssets,
    loadSpineEntry,
    measureSpineBounds,
    removeLayerChildren,
    resolveSpineBaseUrl,
    selectDefaultSpineAnimation,
    startDefaultSpineAnimation,
  });

  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
    return;
  }

  globalScope.AzurLaneSpine = api;
})(typeof globalThis !== 'undefined' ? globalThis : window);
