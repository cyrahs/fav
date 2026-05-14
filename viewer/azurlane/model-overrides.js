(function attachAzurLaneModelOverrides(globalScope) {
  'use strict';

  const overrides = Object.freeze({
    'azurlane:live2d:xingdengbao:xingdengbao_2': Object.freeze({
      scaleOverride: 0.42,
      offsetX: 0,
      offsetY: 38,
      notes: 'Manual framing correction for a model that sits low in the automatic bounds.',
    }),
  });

  if (typeof module !== 'undefined' && module.exports) {
    module.exports = overrides;
    return;
  }

  globalScope.AzurLaneModelOverrides = overrides;
})(typeof globalThis !== 'undefined' ? globalThis : window);
