const assert = require('node:assert/strict');
const test = require('node:test');

const { applyCenteredRoot, calculateCenteredRoot } = require('../stage-layout.js');

test('calculateCenteredRoot centers the content root without scaling it', () => {
  assert.deepEqual(calculateCenteredRoot(1280, 720), {
    x: 640,
    y: 360,
    scaleX: 1,
    scaleY: 1,
  });

  assert.deepEqual(calculateCenteredRoot(390, 844), {
    x: 195,
    y: 422,
    scaleX: 1,
    scaleY: 1,
  });
});

test('calculateCenteredRoot rejects invalid viewport dimensions', () => {
  assert.throws(() => calculateCenteredRoot(0, 720), /viewportWidth/);
  assert.throws(() => calculateCenteredRoot(1280, Number.NaN), /viewportHeight/);
});

test('applyCenteredRoot writes Pixi-style position and scale setters', () => {
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

  const layout = applyCenteredRoot(displayObject, 800, 600);

  assert.deepEqual(layout, {
    x: 400,
    y: 300,
    scaleX: 1,
    scaleY: 1,
  });
  assert.deepEqual(writes, [
    ['position', 400, 300],
    ['scale', 1, 1],
  ]);
});
