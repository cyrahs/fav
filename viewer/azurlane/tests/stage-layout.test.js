const assert = require('node:assert/strict');
const test = require('node:test');

const {
  DESIGN_CENTER_X,
  DESIGN_CENTER_Y,
  DESIGN_HEIGHT,
  DESIGN_WIDTH,
  applyFixedStageRoot,
  calculateFixedStageRoot,
} = require('../stage-layout.js');

const EPSILON = 1e-9;

function assertClose(actual, expected) {
  assert.ok(Math.abs(actual - expected) < EPSILON, `${actual} should equal ${expected}`);
}

function assertStageFitsViewport(layout, viewportWidth, viewportHeight) {
  assert.ok(layout.x >= -EPSILON);
  assert.ok(layout.y >= -EPSILON);
  assert.ok(layout.x + layout.scaledWidth <= viewportWidth + EPSILON);
  assert.ok(layout.y + layout.scaledHeight <= viewportHeight + EPSILON);
  assertClose(layout.x + layout.scaledWidth / 2, viewportWidth / 2);
  assertClose(layout.y + layout.scaledHeight / 2, viewportHeight / 2);
}

test('exports fixed 1600x900 design constants', () => {
  assert.equal(DESIGN_WIDTH, 1600);
  assert.equal(DESIGN_HEIGHT, 900);
  assert.equal(DESIGN_CENTER_X, 800);
  assert.equal(DESIGN_CENTER_Y, 450);
});

test('calculateFixedStageRoot scales and centers the fixed logical stage', () => {
  const cases = [
    {
      name: 'native',
      viewportWidth: 1600,
      viewportHeight: 900,
      scale: 1,
      x: 0,
      y: 0,
      scaledWidth: 1600,
      scaledHeight: 900,
    },
    {
      name: 'desktop',
      viewportWidth: 1280,
      viewportHeight: 720,
      scale: 0.8,
      x: 0,
      y: 0,
      scaledWidth: 1280,
      scaledHeight: 720,
    },
    {
      name: 'wide',
      viewportWidth: 1920,
      viewportHeight: 720,
      scale: 0.8,
      x: 320,
      y: 0,
      scaledWidth: 1280,
      scaledHeight: 720,
    },
    {
      name: 'narrow',
      viewportWidth: 900,
      viewportHeight: 900,
      scale: 0.5625,
      x: 0,
      y: 196.875,
      scaledWidth: 900,
      scaledHeight: 506.25,
    },
    {
      name: 'mobile-like',
      viewportWidth: 390,
      viewportHeight: 844,
      scale: 0.24375,
      x: 0,
      y: 312.3125,
      scaledWidth: 390,
      scaledHeight: 219.375,
    },
  ];

  for (const testCase of cases) {
    const layout = calculateFixedStageRoot(testCase.viewportWidth, testCase.viewportHeight);

    assert.equal(layout.designWidth, DESIGN_WIDTH, testCase.name);
    assert.equal(layout.designHeight, DESIGN_HEIGHT, testCase.name);
    assertClose(layout.x, testCase.x);
    assertClose(layout.y, testCase.y);
    assertClose(layout.scaleX, testCase.scale);
    assertClose(layout.scaleY, testCase.scale);
    assertClose(layout.scaledWidth, testCase.scaledWidth);
    assertClose(layout.scaledHeight, testCase.scaledHeight);
    assertClose(layout.centerX, testCase.viewportWidth / 2);
    assertClose(layout.centerY, testCase.viewportHeight / 2);
    assertStageFitsViewport(layout, testCase.viewportWidth, testCase.viewportHeight);
  }
});

test('calculateFixedStageRoot rejects invalid viewport dimensions', () => {
  assert.throws(() => calculateFixedStageRoot(0, 720), /viewportWidth/);
  assert.throws(() => calculateFixedStageRoot(1280, Number.NaN), /viewportHeight/);
});

test('applyFixedStageRoot writes Pixi-style position and scale setters', () => {
  const writes = [];
  const displayObject = {
    position: {
      set(x, y) {
        writes.push(['position', x, y]);
      },
    },
    scale: {
      set(x, y) {
        writes.push(['scale', x, y]);
      },
    },
  };

  const layout = applyFixedStageRoot(displayObject, 800, 600);

  assert.equal(layout.x, 0);
  assert.equal(layout.y, 75);
  assert.equal(layout.scaleX, 0.5);
  assert.equal(layout.scaleY, 0.5);
  assert.equal(layout.scaledWidth, 800);
  assert.equal(layout.scaledHeight, 450);
  assert.deepEqual(writes, [
    ['position', 0, 75],
    ['scale', 0.5, 0.5],
  ]);
});
