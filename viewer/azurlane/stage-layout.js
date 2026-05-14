(function attachStageLayout(globalScope) {
  'use strict';

  function assertFiniteDimension(value, name) {
    if (!Number.isFinite(value) || value <= 0) {
      throw new RangeError(`${name} must be a positive finite number`);
    }
  }

  const DESIGN_WIDTH = 1600;
  const DESIGN_HEIGHT = 900;
  const DESIGN_CENTER_X = DESIGN_WIDTH / 2;
  const DESIGN_CENTER_Y = DESIGN_HEIGHT / 2;

  function calculateFixedStageRoot(viewportWidth, viewportHeight) {
    assertFiniteDimension(viewportWidth, 'viewportWidth');
    assertFiniteDimension(viewportHeight, 'viewportHeight');

    const rootScale = Math.min(viewportWidth / DESIGN_WIDTH, viewportHeight / DESIGN_HEIGHT);
    const scaledWidth = DESIGN_WIDTH * rootScale;
    const scaledHeight = DESIGN_HEIGHT * rootScale;
    const x = (viewportWidth - scaledWidth) / 2;
    const y = (viewportHeight - scaledHeight) / 2;

    return {
      x,
      y,
      scaleX: rootScale,
      scaleY: rootScale,
      scaledWidth,
      scaledHeight,
      viewportWidth,
      viewportHeight,
      designWidth: DESIGN_WIDTH,
      designHeight: DESIGN_HEIGHT,
      centerX: x + DESIGN_CENTER_X * rootScale,
      centerY: y + DESIGN_CENTER_Y * rootScale,
    };
  }

  function applyFixedStageRoot(displayObject, viewportWidth, viewportHeight) {
    if (!displayObject?.position?.set || !displayObject?.scale?.set) {
      throw new TypeError('displayObject must expose Pixi-style position and scale setters');
    }

    const layout = calculateFixedStageRoot(viewportWidth, viewportHeight);
    displayObject.position.set(layout.x, layout.y);
    displayObject.scale.set(layout.scaleX, layout.scaleY);
    return layout;
  }

  const api = Object.freeze({
    DESIGN_WIDTH,
    DESIGN_HEIGHT,
    DESIGN_CENTER_X,
    DESIGN_CENTER_Y,
    calculateFixedStageRoot,
    applyFixedStageRoot,
  });

  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
    return;
  }

  globalScope.AzurLaneStageLayout = api;
})(typeof globalThis !== 'undefined' ? globalThis : window);
