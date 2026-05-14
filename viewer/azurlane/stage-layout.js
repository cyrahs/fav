(function attachStageLayout(globalScope) {
  'use strict';

  function assertFiniteDimension(value, name) {
    if (!Number.isFinite(value) || value <= 0) {
      throw new RangeError(`${name} must be a positive finite number`);
    }
  }

  function calculateCenteredRoot(viewportWidth, viewportHeight) {
    assertFiniteDimension(viewportWidth, 'viewportWidth');
    assertFiniteDimension(viewportHeight, 'viewportHeight');

    return {
      x: viewportWidth / 2,
      y: viewportHeight / 2,
      scaleX: 1,
      scaleY: 1,
    };
  }

  function applyCenteredRoot(displayObject, viewportWidth, viewportHeight) {
    if (!displayObject?.position?.set || !displayObject?.scale?.set) {
      throw new TypeError('displayObject must expose Pixi-style position and scale setters');
    }

    const layout = calculateCenteredRoot(viewportWidth, viewportHeight);
    displayObject.position.set(layout.x, layout.y);
    displayObject.scale.set(layout.scaleX, layout.scaleY);
    return layout;
  }

  const api = Object.freeze({
    calculateCenteredRoot,
    applyCenteredRoot,
  });

  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
    return;
  }

  globalScope.AzurLaneStageLayout = api;
})(typeof globalThis !== 'undefined' ? globalThis : window);
