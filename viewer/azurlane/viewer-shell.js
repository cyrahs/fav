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
    let activeLive2D = null;

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

      setStatusText(controlsRoot, 'Loading Live2D');
      activeLive2D = null;
      try {
        const result = await globalScope.AzurLaneLive2D.loadLive2DEntry(entry, {
          app,
          live2dLayer,
          stageLayout: globalScope.AzurLaneStageLayout,
          ...loadOptions,
        });
        activeLive2D = result;
        live2dLoadCount += 1;
        setStatusText(controlsRoot, 'Live2D');
        app.render();
        return result;
      } catch (error) {
        setStatusText(controlsRoot, 'Live2D Error');
        throw error;
      }
    }

    function clearLive2DLayer() {
      if (!globalScope.AzurLaneLive2D) {
        live2dLayer.removeChildren();
      } else {
        globalScope.AzurLaneLive2D.removeLayerChildren(live2dLayer);
      }
      activeLive2D = null;
      setStatusText(controlsRoot, 'Shell');
      app.render();
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
              anchorX: model.anchor?.x ?? 0,
              anchorY: model.anchor?.y ?? 0,
              fit: activeLive2D.fit,
              dimensions: activeLive2D.dimensions,
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
      clearLive2DLayer,
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
        resizeObserver.disconnect();
        globalScope.removeEventListener('resize', requestResize);
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
          modelLoadingRequested: live2dLoadCount > 0,
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
