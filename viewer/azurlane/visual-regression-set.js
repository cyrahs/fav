(function attachAzurLaneVisualRegressionSet(globalScope) {
  'use strict';

  const VIEWPORTS = Object.freeze([
    Object.freeze({ name: 'baseline-desktop', width: 1600, height: 900 }),
    Object.freeze({ name: 'full-hd', width: 1920, height: 1080 }),
    Object.freeze({ name: 'laptop', width: 1366, height: 768 }),
    Object.freeze({ name: 'mobile-tall', width: 390, height: 844 }),
  ]);

  const MODELS = Object.freeze([
    Object.freeze({
      category: 'shared-old-nagami-live2d',
      reason: 'Shared older Nagami/l2d.su Live2D resource that exercises fallback-era framing.',
      entry: Object.freeze({
        id: 'azurlane:live2d:xingdengbao:xingdengbao_2',
        type: 'live2d',
        source: 'l2d.su',
        character: Object.freeze({ key: 'xingdengbao', name_en: 'Golden Hind', name_zh: '' }),
        costume: Object.freeze({ key: 'xingdengbao_2', name_en: 'Shared old Live2D', name_zh: 'Shared old Live2D' }),
        resources: Object.freeze({
          primary_url: 'https://static.l2d.su/live2d/azurlane/xingdengbao_2/xingdengbao_2.model3.json',
        }),
        layout: Object.freeze({ mode: 'auto-fit', anchor: Object.freeze([0.5, 0.5]) }),
      }),
      fakeBounds: Object.freeze({ x: -400, y: -200, width: 800, height: 400 }),
      markerColor: Object.freeze({ red: 216, green: 88, blue: 92 }),
      minimumMarkerPixels: 1400,
      expectedScale: 2,
    }),
    Object.freeze({
      category: 'newer-l2dsu-live2d-only',
      reason: 'Newer l2d.su Live2D-only shape with a tall body and no Spine counterpart in this fixed set.',
      entry: Object.freeze({
        id: 'azurlane:live2d:yuanchou:yuanchou_3',
        type: 'live2d',
        source: 'l2d.su',
        character: Object.freeze({ key: 'yuanchou', name_en: 'Unzen', name_zh: '' }),
        costume: Object.freeze({ key: 'yuanchou_3', name_en: 'Newer Live2D-only', name_zh: 'Newer Live2D-only' }),
        resources: Object.freeze({
          primary_url: 'https://static.l2d.su/live2d/azurlane/yuanchou_3/yuanchou_3.model3.json',
        }),
        layout: Object.freeze({ mode: 'auto-fit', anchor: Object.freeze([0.5, 0.5]) }),
      }),
      fakeBounds: Object.freeze({ x: -225, y: -600, width: 450, height: 1200 }),
      markerColor: Object.freeze({ red: 86, green: 176, blue: 216 }),
      minimumMarkerPixels: 1400,
      expectedScale: 0.75,
    }),
    Object.freeze({
      category: 'spine-dynamic-background',
      reason: 'Spine/Dynamic entry with visible background attachment slots.',
      entry: Object.freeze({
        id: 'azurlane:spine:aerbien:aerbien_4',
        type: 'spine',
        source: 'l2d.su',
        character: Object.freeze({ key: 'aerbien', name_en: 'Albion', name_zh: '' }),
        costume: Object.freeze({ key: 'aerbien_4', name_en: 'Dynamic with background', name_zh: 'Dynamic with background' }),
        resources: Object.freeze({
          primary_url: 'https://static.l2d.su/live2d/azurlane/aerbien_4-spine',
        }),
        layout: Object.freeze({ mode: 'auto-fit' }),
      }),
      fakeBounds: Object.freeze({ x: -500, y: -1000, width: 1000, height: 2000 }),
      backgroundSlots: Object.freeze(['bj_background', 'bj_window']),
      markerColor: Object.freeze({ red: 118, green: 207, blue: 123 }),
      backgroundColor: Object.freeze({ red: 64, green: 111, blue: 178 }),
      minimumMarkerPixels: 1400,
      minimumBackgroundPixels: 100,
      expectedScale: 0.45,
    }),
    Object.freeze({
      category: 'large-unusual-aspect-ratio',
      reason: 'Wide model bounds catch unusual aspect ratio and horizontal fitting regressions.',
      entry: Object.freeze({
        id: 'azurlane:spine:yilisi:yilisi_2_doa',
        type: 'spine',
        source: 'l2d.su',
        character: Object.freeze({ key: 'yilisi', name_en: 'Elise', name_zh: '' }),
        costume: Object.freeze({ key: 'yilisi_2_doa', name_en: 'Large wide dynamic', name_zh: 'Large wide dynamic' }),
        resources: Object.freeze({
          primary_url: 'https://static.l2d.su/live2d/azurlane/yilisi_2_doa',
        }),
        layout: Object.freeze({ mode: 'auto-fit' }),
      }),
      fakeBounds: Object.freeze({ x: -800, y: -300, width: 1600, height: 600 }),
      backgroundSlots: Object.freeze(['bj_sea']),
      markerColor: Object.freeze({ red: 232, green: 185, blue: 80 }),
      backgroundColor: Object.freeze({ red: 82, green: 112, blue: 190 }),
      minimumMarkerPixels: 1400,
      minimumBackgroundPixels: 100,
      expectedScale: 1,
    }),
  ]);

  const api = Object.freeze({
    MODELS,
    VIEWPORTS,
  });

  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
    return;
  }

  globalScope.AzurLaneVisualRegressionSet = api;
})(typeof globalThis !== 'undefined' ? globalThis : window);
