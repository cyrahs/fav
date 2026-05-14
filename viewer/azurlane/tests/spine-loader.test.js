const assert = require('node:assert/strict');
const test = require('node:test');

const {
  buildAtlasImageMap,
  deriveSpineResourceUrls,
  fitSpineModelIntoStage,
  loadSpineEntry,
  removeLayerChildren,
  selectDefaultSpineAnimation,
} = require('../spine-loader.js');
const stageLayout = require('../stage-layout.js');

const EPSILON = 1e-9;

function assertClose(actual, expected, message) {
  assert.ok(Math.abs(actual - expected) < EPSILON, `${message}: expected ${expected}, received ${actual}`);
}

function createPoint() {
  return {
    x: 0,
    y: 0,
    set(x, y) {
      this.x = x;
      this.y = y;
    },
  };
}

function createFakeSpine(bounds, animations = ['idle', 'normal']) {
  const animationCalls = [];

  return {
    position: createPoint(),
    pivot: createPoint(),
    scale: createPoint(),
    visible: true,
    updateCalls: [],
    animationCalls,
    skeleton: {
      data: {
        animations: animations.map((name) => ({ name })),
      },
    },
    state: {
      setAnimation(trackIndex, name, loop) {
        animationCalls.push({ trackIndex, name, loop });
      },
    },
    update(delta) {
      this.updateCalls.push(delta);
    },
    getLocalBounds() {
      return bounds;
    },
  };
}

function createFakeLayer() {
  return {
    label: 'fakeLayer',
    children: [],
    addChild(child) {
      this.children.push(child);
      child.parent = this;
      return child;
    },
    removeChild(child) {
      this.children = this.children.filter((item) => item !== child);
      child.parent = null;
      return child;
    },
  };
}

function createFakeAssets(loadResultFactory) {
  const calls = {
    add: [],
    load: [],
  };

  return {
    calls,
    assets: {
      add(descriptor) {
        calls.add.push(descriptor);
      },
      async load(aliases) {
        calls.load.push(aliases);
        if (typeof loadResultFactory === 'function') {
          return loadResultFactory(aliases);
        }
        return Object.fromEntries(aliases.map((alias) => [alias, { alias }]));
      },
    },
  };
}

function createFakeTicker() {
  return {
    listeners: [],
    add(listener) {
      this.listeners.push(listener);
    },
    remove(listener) {
      this.listeners = this.listeners.filter((item) => item !== listener);
    },
  };
}

test('deriveSpineResourceUrls builds suffix-free asset URLs for l2d.su -spine directories', () => {
  const urls = deriveSpineResourceUrls({
    type: 'spine',
    resources: {
      primary_url: 'https://static.example/live2d/azurlane/aerbien_4-spine',
    },
  });

  assert.equal(urls.baseUrl, 'https://static.example/live2d/azurlane/aerbien_4-spine');
  assert.equal(urls.baseDirectoryUrl, 'https://static.example/live2d/azurlane/aerbien_4-spine');
  assert.equal(urls.fileStem, 'aerbien_4');
  assert.equal(urls.skelUrl, 'https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.skel');
  assert.equal(urls.atlasUrl, 'https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.atlas');
  assert.deepEqual(urls.textureUrls, ['https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.webp']);
  assert.deepEqual(buildAtlasImageMap(urls.textureUrls), {
    'aerbien_4.webp': 'https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.webp',
  });
});

test('deriveSpineResourceUrls keeps validated texture URLs as runtime atlas image inputs', () => {
  const urls = deriveSpineResourceUrls({
    type: 'spine',
    resources: {
      primary_url: 'https://static.example/live2d/azurlane/yilisi_2_doa',
    },
    capabilities: {
      textures: ['https://cdn.example/spine-pages/yilisi_2_doa.webp'],
    },
  });

  assert.equal(urls.skelUrl, 'https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.skel');
  assert.equal(urls.atlasUrl, 'https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.atlas');
  assert.deepEqual(urls.textureUrls, ['https://cdn.example/spine-pages/yilisi_2_doa.webp']);
  assert.deepEqual(buildAtlasImageMap(urls.textureUrls), {
    'yilisi_2_doa.webp': 'https://cdn.example/spine-pages/yilisi_2_doa.webp',
  });
});

test('fitSpineModelIntoStage uses local bounds for pivot, scale, and stage centering', () => {
  const spine = createFakeSpine({ x: -200, y: -1000, width: 400, height: 2000 });

  const fit = fitSpineModelIntoStage(spine, { stageLayout });

  assert.deepEqual(spine.updateCalls, [0]);
  assertClose(spine.pivot.x, 0, 'pivot x');
  assertClose(spine.pivot.y, 0, 'pivot y');
  assertClose(fit.scale, 0.45, 'fit scale');
  assertClose(spine.scale.x, 0.45, 'scale x');
  assertClose(spine.scale.y, 0.45, 'scale y');
  assertClose(spine.position.x, 800, 'position x');
  assertClose(spine.position.y, 450, 'position y');
});

test('selectDefaultSpineAnimation prefers normal and falls back to the first runtime animation', () => {
  assert.equal(selectDefaultSpineAnimation(createFakeSpine({ x: 0, y: 0, width: 10, height: 10 }, ['idle', 'normal'])), 'normal');
  assert.equal(selectDefaultSpineAnimation(createFakeSpine({ x: 0, y: 0, width: 10, height: 10 }, ['idle', 'touch'])), 'idle');
});

test('loadSpineEntry loads through the runtime, adds only to spineLayer, starts normal, and fits bounds', async () => {
  const spineLayer = createFakeLayer();
  spineLayer.label = 'spineLayer';
  const unrelatedLayer = createFakeLayer();
  const model = createFakeSpine({ x: -500, y: -1000, width: 1000, height: 2000 }, ['touch', 'normal']);
  const { calls, assets } = createFakeAssets();
  const runtimeCalls = [];
  const runtime = {
    Spine: {
      from(options) {
        runtimeCalls.push(options);
        return model;
      },
    },
  };
  const entry = {
    id: 'azurlane:spine:aerbien:aerbien_4',
    type: 'spine',
    resources: {
      primary_url: 'https://static.l2d.su/live2d/azurlane/aerbien_4-spine',
    },
    layout: {
      mode: 'auto-fit',
    },
  };

  const result = await loadSpineEntry(entry, {
    spineLayer,
    runtime,
    assets,
    stageLayout,
  });

  assert.deepEqual(
    calls.add.map((descriptor) => ({ alias: descriptor.alias, src: descriptor.src, data: descriptor.data })),
    [
      {
        alias: 'azurlane:spine:aerbien:aerbien_4:skeleton',
        src: 'https://static.l2d.su/live2d/azurlane/aerbien_4-spine/aerbien_4.skel',
        data: undefined,
      },
      {
        alias: 'azurlane:spine:aerbien:aerbien_4:atlas',
        src: 'https://static.l2d.su/live2d/azurlane/aerbien_4-spine/aerbien_4.atlas',
        data: {
          images: {
            'aerbien_4.webp': 'https://static.l2d.su/live2d/azurlane/aerbien_4-spine/aerbien_4.webp',
          },
        },
      },
    ],
  );
  assert.deepEqual(calls.load, [['azurlane:spine:aerbien:aerbien_4:skeleton', 'azurlane:spine:aerbien:aerbien_4:atlas']]);
  assert.deepEqual(runtimeCalls, [
    {
      skeleton: 'azurlane:spine:aerbien:aerbien_4:skeleton',
      atlas: 'azurlane:spine:aerbien:aerbien_4:atlas',
      autoUpdate: true,
    },
  ]);
  assert.deepEqual(spineLayer.children, [model]);
  assert.deepEqual(unrelatedLayer.children, []);
  assert.equal(result.spine, model);
  assert.deepEqual(model.animationCalls, [{ trackIndex: 0, name: 'normal', loop: true }]);
  assertClose(result.fit.scale, 0.45, 'fit scale');
  assertClose(model.position.x, 800, 'model position x');
  assertClose(model.position.y, 450, 'model position y');
  assert.equal(model.visible, true);
});

test('loadSpineEntry supports Spine 3.8 constructor runtimes loaded from skeleton asset data', async () => {
  const spineLayer = createFakeLayer();
  const spineData = {
    animations: [{ name: 'normal' }],
  };
  const model = createFakeSpine({ x: -250, y: -250, width: 500, height: 500 }, ['normal']);
  const { calls, assets } = createFakeAssets((aliases) => ({
    [aliases[0]]: {
      spineData,
    },
  }));
  const runtimeCalls = [];
  function Spine(loadedSpineData) {
    runtimeCalls.push(loadedSpineData);
    return model;
  }

  const result = await loadSpineEntry(
    {
      id: 'azurlane:spine:na:na_2_doa',
      type: 'spine',
      resources: {
        primary_url: 'https://static.l2d.su/live2d/azurlane/na_2_doa',
      },
      layout: {
        mode: 'auto-fit',
      },
    },
    {
      spineLayer,
      runtime: { Spine },
      assets,
      stageLayout,
    },
  );

  assert.deepEqual(
    calls.add.map((descriptor) => ({ alias: descriptor.alias, src: descriptor.src, data: descriptor.data })),
    [
      {
        alias: 'azurlane:spine:na:na_2_doa:skeleton',
        src: 'https://static.l2d.su/live2d/azurlane/na_2_doa/na_2_doa.skel',
        data: {
          spineAtlasFile: 'https://static.l2d.su/live2d/azurlane/na_2_doa/na_2_doa.atlas',
        },
      },
      {
        alias: 'azurlane:spine:na:na_2_doa:atlas',
        src: 'https://static.l2d.su/live2d/azurlane/na_2_doa/na_2_doa.atlas',
        data: undefined,
      },
    ],
  );
  assert.deepEqual(calls.load, [['azurlane:spine:na:na_2_doa:skeleton']]);
  assert.deepEqual(runtimeCalls, [spineData]);
  assert.equal(result.assetInfo.usesFactoryRuntime, false);
  assert.deepEqual(result.assetInfo.loadAliases, ['azurlane:spine:na:na_2_doa:skeleton']);
  assert.equal(model.autoUpdate, true);
  assert.deepEqual(spineLayer.children, [model]);
  assert.deepEqual(model.animationCalls, [{ trackIndex: 0, name: 'normal', loop: true }]);
  assertClose(model.position.x, 800, 'model position x');
  assertClose(model.position.y, 450, 'model position y');
});

test('loadSpineEntry wires constructor runtimes to the app ticker and cleans up on removal', async () => {
  const spineLayer = createFakeLayer();
  const spineData = {
    animations: [{ name: 'normal' }],
  };
  const model = createFakeSpine({ x: -100, y: -100, width: 200, height: 200 }, ['normal']);
  const ticker = createFakeTicker();
  const { assets } = createFakeAssets((aliases) => ({
    [aliases[0]]: {
      spineData,
    },
  }));
  function Spine(loadedSpineData) {
    assert.equal(loadedSpineData, spineData);
    return model;
  }

  await loadSpineEntry(
    {
      id: 'azurlane:spine:xiangdi:xiangdi_2_doa',
      type: 'spine',
      resources: {
        primary_url: 'https://static.l2d.su/live2d/azurlane/xiangdi_2_doa',
      },
    },
    {
      app: { ticker },
      spineLayer,
      runtime: { Spine },
      assets,
      stageLayout,
    },
  );

  assert.equal(ticker.listeners.length, 1);
  assert.equal(model.autoUpdate, false);
  ticker.listeners[0]();
  assert.ok(model.updateCalls.length >= 2);

  removeLayerChildren(spineLayer);

  assert.equal(ticker.listeners.length, 0);
  assert.equal(spineLayer.children.length, 0);
});

test('loadSpineEntry rejects non-Spine entries', async () => {
  await assert.rejects(
    loadSpineEntry(
      {
        type: 'live2d',
        resources: {
          primary_url: 'https://static.example/live2d/azurlane/example/example.model3.json',
        },
      },
      {
        spineLayer: createFakeLayer(),
        runtime: {
          Spine: {
            from() {
              throw new Error('should not load');
            },
          },
        },
        assets: createFakeAssets().assets,
        stageLayout,
      },
    ),
    /Expected a spine entry/,
  );
});
