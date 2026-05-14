const assert = require('node:assert/strict');
const test = require('node:test');

const {
  fitLive2DModelIntoStage,
  loadLive2DEntry,
  measureLive2DModel,
  resolveLive2DModelUrl,
  waitForStableLive2DDimensions,
} = require('../live2d-loader.js');
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

function createFakeModel(boundsSequence) {
  const bounds = Array.isArray(boundsSequence) ? boundsSequence : [boundsSequence];
  let boundIndex = 0;

  return {
    position: createPoint(),
    scale: createPoint(),
    anchor: createPoint(),
    visible: true,
    destroyed: false,
    getLocalBounds() {
      const value = bounds[Math.min(boundIndex, bounds.length - 1)];
      boundIndex += 1;
      return value;
    },
    destroy() {
      this.destroyed = true;
    },
  };
}

function createFakeLayer() {
  return {
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

test('fitLive2DModelIntoStage fits wide, tall, and square models into the fixed stage', () => {
  const cases = [
    {
      name: 'wide',
      dimensions: { x: -400, y: -200, width: 800, height: 400, centerX: 0, centerY: 0 },
      expectedScale: 2,
    },
    {
      name: 'tall',
      dimensions: { x: -225, y: -600, width: 450, height: 1200, centerX: 0, centerY: 0 },
      expectedScale: 0.75,
    },
    {
      name: 'square',
      dimensions: { x: -250, y: -250, width: 500, height: 500, centerX: 0, centerY: 0 },
      expectedScale: 1.8,
    },
  ];

  for (const testCase of cases) {
    const model = createFakeModel(testCase.dimensions);
    const fit = fitLive2DModelIntoStage(model, testCase.dimensions, { stageLayout });

    assertClose(fit.scale, testCase.expectedScale, `${testCase.name} scale`);
    assertClose(model.scale.x, testCase.expectedScale, `${testCase.name} model scale x`);
    assertClose(model.scale.y, testCase.expectedScale, `${testCase.name} model scale y`);
    assertClose(model.position.x, 800, `${testCase.name} position x`);
    assertClose(model.position.y, 450, `${testCase.name} position y`);
    assertClose(model.anchor.x, 0.5, `${testCase.name} anchor x`);
    assertClose(model.anchor.y, 0.5, `${testCase.name} anchor y`);
    assert.equal(fit.scale, fit.autoScale);
  }
});

test('fitLive2DModelIntoStage centers offset local bounds', () => {
  const model = createFakeModel({ x: 100, y: 50, width: 300, height: 600 });
  const dimensions = measureLive2DModel(model);

  const fit = fitLive2DModelIntoStage(model, dimensions, { stageLayout });

  assertClose(fit.scale, 1.5, 'scale');
  assertClose(model.position.x, 425, 'position x');
  assertClose(model.position.y, -75, 'position y');
});

test('waitForStableLive2DDimensions waits for repeated stable measurements', async () => {
  let requestedFrames = 0;
  const model = createFakeModel([
    { x: 0, y: 0, width: 0, height: 0 },
    { x: -200, y: -250, width: 400, height: 500 },
    { x: -205, y: -250, width: 410, height: 500 },
    { x: -205.1, y: -250.1, width: 410.2, height: 500.2 },
    { x: -205.1, y: -250.1, width: 410.2, height: 500.2 },
  ]);

  const dimensions = await waitForStableLive2DDimensions(model, {
    stableFrames: 2,
    maxFrames: 8,
    epsilon: 0.5,
    requestFrame(resolve) {
      requestedFrames += 1;
      resolve();
    },
  });

  assert.equal(requestedFrames, 4);
  assert.equal(dimensions.frames, 4);
  assert.equal(dimensions.stableFrames, 2);
  assert.equal(dimensions.timedOut, false);
  assertClose(dimensions.width, 410.2, 'stable width');
  assertClose(dimensions.height, 500.2, 'stable height');
});

test('resolveLive2DModelUrl prefers validated URLs and supports fallback selection', () => {
  const entry = {
    resources: {
      primary_url: 'https://static.example/live2d/azurlane/new/new.model3.json',
      fallback_url: 'https://cdn.example/live2d/old/old.model3.json',
    },
    availability: {
      validated_url: 'https://validated.example/live2d/model.model3.json',
    },
  };

  assert.equal(resolveLive2DModelUrl(entry), 'https://validated.example/live2d/model.model3.json');
  assert.equal(resolveLive2DModelUrl({ ...entry, availability: {} }, { preferFallback: true }), 'https://cdn.example/live2d/old/old.model3.json');
  assert.equal(resolveLive2DModelUrl('https://direct.example/model.model3.json'), 'https://direct.example/model.model3.json');
});

test('loadLive2DEntry creates a runtime model, adds it only to live2dLayer, and fits it', async () => {
  const live2dLayer = createFakeLayer();
  const unrelatedLayer = createFakeLayer();
  const model = createFakeModel({ x: -500, y: -1000, width: 1000, height: 2000 });
  const calls = [];
  const runtime = {
    Live2DModel: {
      async from(url, options) {
        calls.push({ url, options });
        return model;
      },
    },
  };
  const entry = {
    id: 'azurlane:live2d:xingdengbao:xingdengbao_2',
    type: 'live2d',
    resources: {
      primary_url: 'https://static.l2d.su/live2d/azurlane/xingdengbao_2/xingdengbao_2.model3.json',
    },
    layout: {
      mode: 'auto-fit',
      anchor: [0.5, 0.5],
    },
  };

  const result = await loadLive2DEntry(entry, {
    live2dLayer,
    runtime,
    stageLayout,
    dimensionOptions: { stableFrames: 1, maxFrames: 3 },
    requestFrame(resolve) {
      resolve();
    },
  });

  assert.deepEqual(calls, [
    {
      url: 'https://static.l2d.su/live2d/azurlane/xingdengbao_2/xingdengbao_2.model3.json',
      options: {
        autoInteract: false,
        autoFocus: false,
        autoHitTest: false,
      },
    },
  ]);
  assert.deepEqual(live2dLayer.children, [model]);
  assert.deepEqual(unrelatedLayer.children, []);
  assert.equal(result.model, model);
  assertClose(result.fit.scale, 0.45, 'fit scale');
  assertClose(model.position.x, 800, 'model position x');
  assertClose(model.position.y, 450, 'model position y');
  assert.equal(model.visible, true);
});

test('loadLive2DEntry rejects non-Live2D entries', async () => {
  await assert.rejects(
    loadLive2DEntry(
      {
        type: 'spine',
        resources: {
          primary_url: 'https://static.example/live2d/azurlane/example-spine',
        },
      },
      {
        live2dLayer: createFakeLayer(),
        runtime: {
          Live2DModel: {
            async from() {
              throw new Error('should not load');
            },
          },
        },
        stageLayout,
      },
    ),
    /Expected a live2d entry/,
  );
});
