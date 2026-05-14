const assert = require('node:assert/strict');
const test = require('node:test');

const {
  debugModeFromLocation,
  filterCatalogEntries,
  normalizeCatalogPayload,
  normalizeL2DSuCatalog,
} = require('../model-catalog.js');

function sampleL2DSuPayload() {
  return {
    Master: [
      {
        gameId: 1,
        gameName: 'Azur Lane',
        character: [
          {
            charId: 101,
            charKey: 'guanghui',
            charName: '光辉',
            charNameEn: 'Illustrious',
            live2d: [
              {
                costumeId: 701,
                costumeName: '永不落幕的茶会',
                costumeNameEn: 'Never-Ending Tea Party',
                path: 'https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json',
              },
            ],
            spine: [],
          },
          {
            charId: 202,
            charKey: 'yilisi',
            charName: '伊丽丝',
            charNameEn: 'Elise',
            live2d: [],
            spine: [
              {
                costumeId: 2,
                costumeName: '金色午后',
                costumeNameEn: 'Golden Afternoon',
                path: 'https://static.l2d.su/live2d/azurlane/yilisi_2_doa',
              },
            ],
          },
        ],
      },
    ],
  };
}

test('normalizeL2DSuCatalog converts Live2D and Spine records to viewer ModelEntry shape', () => {
  const catalog = normalizeL2DSuCatalog(sampleL2DSuPayload());

  assert.equal(catalog.entries.length, 2);
  assert.deepEqual(catalog.summary.by_type, { live2d: 1, spine: 1 });
  assert.deepEqual(
    catalog.entries.map((entry) => [entry.id, entry.type, entry.character.name_zh, entry.character.name_en, entry.costume.name_zh, entry.costume.name_en]),
    [
      ['azurlane:live2d:guanghui:guanghui_7', 'live2d', '光辉', 'Illustrious', '永不落幕的茶会', 'Never-Ending Tea Party'],
      ['azurlane:spine:yilisi:yilisi_2_doa', 'spine', '伊丽丝', 'Elise', '金色午后', 'Golden Afternoon'],
    ],
  );
  assert.equal(catalog.entries[0].resources.primary_url, 'https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json');
  assert.equal(catalog.entries[1].resources.primary_url, 'https://static.l2d.su/live2d/azurlane/yilisi_2_doa');
});

test('filterCatalogEntries searches Chinese and English character and costume names', () => {
  const { entries } = normalizeL2DSuCatalog(sampleL2DSuPayload());

  assert.deepEqual(
    filterCatalogEntries(entries, { query: '光辉' }).map((entry) => entry.id),
    ['azurlane:live2d:guanghui:guanghui_7'],
  );
  assert.deepEqual(
    filterCatalogEntries(entries, { query: 'Illustrious Tea' }).map((entry) => entry.id),
    ['azurlane:live2d:guanghui:guanghui_7'],
  );
  assert.deepEqual(
    filterCatalogEntries(entries, { query: '金色' }).map((entry) => entry.id),
    ['azurlane:spine:yilisi:yilisi_2_doa'],
  );
  assert.deepEqual(
    filterCatalogEntries(entries, { query: 'Golden Afternoon' }).map((entry) => entry.id),
    ['azurlane:spine:yilisi:yilisi_2_doa'],
  );
});

test('filterCatalogEntries filters by model type and hides broken entries unless debug mode is enabled', () => {
  const catalog = normalizeCatalogPayload({
    entries: [
      {
        id: 'azurlane:live2d:ok:ok',
        type: 'live2d',
        character: { key: 'ok', name_zh: '可用', name_en: 'Available' },
        costume: { key: 'ok', name_zh: '默认', name_en: 'Default' },
        resources: { primary_url: 'https://example.test/ok.model3.json' },
        availability: { state: 'valid' },
      },
      {
        id: 'azurlane:spine:ok:ok',
        type: 'spine',
        character: { key: 'ok', name_zh: '可用', name_en: 'Available' },
        costume: { key: 'ok', name_zh: '骨骼', name_en: 'Skeleton' },
        resources: { primary_url: 'https://example.test/ok-spine' },
        availability: { state: 'unchecked' },
      },
      {
        id: 'azurlane:live2d:broken:broken',
        type: 'live2d',
        character: { key: 'broken', name_zh: '损坏', name_en: 'Broken' },
        costume: { key: 'broken', name_zh: '缺失', name_en: 'Missing' },
        resources: { primary_url: 'https://example.test/broken.model3.json' },
        availability: { state: 'broken' },
      },
    ],
  });

  assert.deepEqual(
    filterCatalogEntries(catalog.entries, { type: 'live2d' }).map((entry) => entry.id),
    ['azurlane:live2d:ok:ok'],
  );
  assert.deepEqual(
    filterCatalogEntries(catalog.entries, { type: 'spine' }).map((entry) => entry.id),
    ['azurlane:spine:ok:ok'],
  );
  assert.deepEqual(
    filterCatalogEntries(catalog.entries, { query: 'Broken' }).map((entry) => entry.id),
    [],
  );
  assert.deepEqual(
    filterCatalogEntries(catalog.entries, { query: 'Broken', debugMode: true }).map((entry) => entry.id),
    ['azurlane:live2d:broken:broken'],
  );
});

test('debugModeFromLocation keeps catalog debug separate from stage debug', () => {
  const originalDescriptor = Object.getOwnPropertyDescriptor(globalThis, 'location');

  try {
    Object.defineProperty(globalThis, 'location', {
      configurable: true,
      value: { search: '?debugStage=1' },
    });
    assert.equal(debugModeFromLocation(), false);

    Object.defineProperty(globalThis, 'location', {
      configurable: true,
      value: { search: '?debugCatalog=1' },
    });
    assert.equal(debugModeFromLocation(), true);

    Object.defineProperty(globalThis, 'location', {
      configurable: true,
      value: { search: '?debug=1' },
    });
    assert.equal(debugModeFromLocation(), true);
  } finally {
    if (originalDescriptor) {
      Object.defineProperty(globalThis, 'location', originalDescriptor);
    } else {
      delete globalThis.location;
    }
  }
});
