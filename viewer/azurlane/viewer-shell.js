(function attachAzurLaneViewerShell(globalScope) {
  'use strict';

  const BACKGROUND_COLOR = 0x151815;
  const BACKGROUND_LABEL = 'canvasBackgroundLayer';
  const CONTENT_ROOT_LABEL = 'contentRoot';
  const SPINE_LAYER_LABEL = 'spineLayer';
  const LIVE2D_LAYER_LABEL = 'live2dLayer';
  const OVERLAY_LAYER_LABEL = 'overlayLayer';

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
    const overlayLayer = setLayerLabel(new globalScope.PIXI.Container(), OVERLAY_LAYER_LABEL);
    const backgroundFill = new globalScope.PIXI.Graphics();

    canvasBackgroundLayer.addChild(backgroundFill);
    contentRoot.addChild(spineLayer, live2dLayer);
    app.stage.addChild(canvasBackgroundLayer, contentRoot, overlayLayer);

    let lastLayout = null;
    let resizeFrame = 0;

    function resizeShell() {
      const width = Math.max(1, app.screen.width);
      const height = Math.max(1, app.screen.height);

      backgroundFill.clear();
      backgroundFill.rect(0, 0, width, height).fill({ color: BACKGROUND_COLOR });

      lastLayout = globalScope.AzurLaneStageLayout.applyCenteredRoot(contentRoot, width, height);
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

    const shell = {
      ready: true,
      controlsRuntime: 'dom',
      modelLoadingRequested: false,
      app,
      canvasBackgroundLayer,
      contentRoot,
      spineLayer,
      live2dLayer,
      overlayLayer,
      resize: resizeShell,
      destroy() {
        if (resizeFrame) {
          globalScope.cancelAnimationFrame(resizeFrame);
        }
        resizeObserver.disconnect();
        globalScope.removeEventListener('resize', requestResize);
        app.destroy(true, { children: true });
      },
      getState() {
        const canvasRect = app.canvas.getBoundingClientRect();
        const backgroundBounds = backgroundFill.getBounds();

        return {
          ready: true,
          controlsRuntime: 'dom',
          modelLoadingRequested: false,
          pixiApplicationCount: 1,
          backgroundColor: BACKGROUND_COLOR,
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
          backgroundBounds: {
            x: backgroundBounds.x,
            y: backgroundBounds.y,
            width: backgroundBounds.width,
            height: backgroundBounds.height,
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
