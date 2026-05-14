import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { extname, join, normalize, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { chromium } = require('playwright');

const viewerRoot = resolve(fileURLToPath(new URL('..', import.meta.url)));
const mimeTypes = new Map([
  ['.css', 'text/css; charset=utf-8'],
  ['.html', 'text/html; charset=utf-8'],
  ['.js', 'application/javascript; charset=utf-8'],
]);

function isInsideRoot(path) {
  const relativePath = relative(viewerRoot, path);
  return relativePath === '' || (!relativePath.startsWith(`..${sep}`) && relativePath !== '..' && !normalize(relativePath).startsWith('..'));
}

function createStaticServer() {
  const server = createServer(async (request, response) => {
    const requestUrl = new URL(request.url ?? '/', 'http://127.0.0.1');
    const pathname = requestUrl.pathname === '/' ? '/index.html' : requestUrl.pathname;
    const filePath = resolve(join(viewerRoot, decodeURIComponent(pathname)));

    if (!isInsideRoot(filePath)) {
      response.writeHead(403).end('Forbidden');
      return;
    }

    try {
      const body = await readFile(filePath);
      response.writeHead(200, {
        'content-type': mimeTypes.get(extname(filePath)) ?? 'application/octet-stream',
      });
      response.end(body);
    } catch {
      response.writeHead(404).end('Not found');
    }
  });

  return new Promise((resolveServer) => {
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      resolveServer({
        server,
        url: `http://127.0.0.1:${address.port}/index.html`,
      });
    });
  });
}

function calculateExpectedLayout(width, height) {
  const designWidth = 1600;
  const designHeight = 900;
  const rootScale = Math.min(width / designWidth, height / designHeight);
  const scaledWidth = designWidth * rootScale;
  const scaledHeight = designHeight * rootScale;

  return {
    x: (width - scaledWidth) / 2,
    y: (height - scaledHeight) / 2,
    scale: rootScale,
    scaledWidth,
    scaledHeight,
    centerX: width / 2,
    centerY: height / 2,
  };
}

function assertClose(actual, expected, message, tolerance = 0.75) {
  assert.ok(Math.abs(actual - expected) <= tolerance, `${message}: expected ${expected}, received ${actual}`);
}

function assertFixedStageState(state, width, height, viewportName) {
  const expected = calculateExpectedLayout(width, height);

  assert.equal(state.design.width, 1600, viewportName);
  assert.equal(state.design.height, 900, viewportName);
  assert.equal(state.design.centerX, 800, viewportName);
  assert.equal(state.design.centerY, 450, viewportName);
  assert.equal(state.stageDebug.visible, true, viewportName);
  assert.deepEqual(state.stageDebug.logicalBounds, { x: 0, y: 0, width: 1600, height: 900 }, viewportName);
  assert.deepEqual(state.stageDebug.logicalCenter, { x: 800, y: 450 }, viewportName);
  assertClose(state.contentRoot.x, expected.x, `${viewportName} contentRoot.x`);
  assertClose(state.contentRoot.y, expected.y, `${viewportName} contentRoot.y`);
  assertClose(state.contentRoot.scaleX, expected.scale, `${viewportName} contentRoot.scaleX`, 0.001);
  assertClose(state.contentRoot.scaleY, expected.scale, `${viewportName} contentRoot.scaleY`, 0.001);
  assertClose(state.lastLayout.scaledWidth, expected.scaledWidth, `${viewportName} stage width`);
  assertClose(state.lastLayout.scaledHeight, expected.scaledHeight, `${viewportName} stage height`);
  assertClose(state.lastLayout.centerX, expected.centerX, `${viewportName} stage center x`);
  assertClose(state.lastLayout.centerY, expected.centerY, `${viewportName} stage center y`);
  assertClose(state.stageDebug.bounds.x, expected.x, `${viewportName} debug bounds x`);
  assertClose(state.stageDebug.bounds.y, expected.y, `${viewportName} debug bounds y`);
  assertClose(state.stageDebug.bounds.width, expected.scaledWidth, `${viewportName} debug bounds width`);
  assertClose(state.stageDebug.bounds.height, expected.scaledHeight, `${viewportName} debug bounds height`);
  assert.ok(state.lastLayout.x >= -0.75, viewportName);
  assert.ok(state.lastLayout.y >= -0.75, viewportName);
  assert.ok(state.lastLayout.x + state.lastLayout.scaledWidth <= width + 0.75, viewportName);
  assert.ok(state.lastLayout.y + state.lastLayout.scaledHeight <= height + 0.75, viewportName);
  assert.equal(state.overlayLayer.x, 0, viewportName);
  assert.equal(state.overlayLayer.y, 0, viewportName);
  assert.equal(state.overlayLayer.scaleX, 1, viewportName);
  assert.equal(state.overlayLayer.scaleY, 1, viewportName);
}

async function waitForFixedStageShell(page, width, height) {
  await page.waitForFunction(
    ({ expectedWidth, expectedHeight }) => {
      const state = window.azurLaneViewerShell?.getState?.();
      if (!state?.ready) {
        return false;
      }

      const expected = window.AzurLaneStageLayout.calculateFixedStageRoot(expectedWidth, expectedHeight);

      return (
        Math.abs(state.screen.width - expectedWidth) < 1
        && Math.abs(state.screen.height - expectedHeight) < 1
        && Math.abs(state.contentRoot.x - expected.x) < 0.75
        && Math.abs(state.contentRoot.y - expected.y) < 0.75
        && Math.abs(state.contentRoot.scaleX - expected.scaleX) < 0.001
        && Math.abs(state.contentRoot.scaleY - expected.scaleY) < 0.001
        && Math.abs(state.stageDebug.bounds.width - expected.scaledWidth) < 0.75
        && Math.abs(state.stageDebug.bounds.height - expected.scaledHeight) < 0.75
        && state.contentRoot.scaleX === state.contentRoot.scaleY
      );
    },
    { expectedWidth: width, expectedHeight: height },
  );
}

const { server, url } = await createStaticServer();
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1280, height: 720 } });
const viewports = [
  { name: 'desktop', width: 1280, height: 720 },
  { name: 'wide', width: 1920, height: 720 },
  { name: 'narrow', width: 900, height: 900 },
  { name: 'mobile-like', width: 390, height: 844 },
];
const pageErrors = [];
const modelAssetRequests = [];

page.on('pageerror', (error) => pageErrors.push(error.message));
page.on('request', (request) => {
  const requestUrl = request.url();
  if (requestUrl.includes('/live2d/azurlane/') || /\.(?:model3\.json|moc3|skel|atlas)(?:[?#]|$)/.test(requestUrl)) {
    modelAssetRequests.push(requestUrl);
  }
});

try {
  await page.goto(`${url}?debugStage=1`, { waitUntil: 'networkidle' });
  await waitForFixedStageShell(page, 1280, 720);

  const initialState = await page.evaluate(() => window.azurLaneViewerShell.getState());
  assert.equal(initialState.pixiApplicationCount, 1);
  assert.deepEqual(initialState.stageChildren, ['canvasBackgroundLayer', 'contentRoot', 'overlayLayer']);
  assert.deepEqual(initialState.contentChildren, ['spineLayer', 'live2dLayer', 'stageDebugLayer']);
  assert.equal(initialState.controlsRuntime, 'dom');
  assert.equal(initialState.modelLoadingRequested, false);
  assert.equal(initialState.backgroundColor, 0x151815);
  assert.equal(initialState.backgroundLayerChildren, 1);
  assert.ok(initialState.backgroundBounds.width >= 1280);
  assert.ok(initialState.backgroundBounds.height >= 720);
  assertFixedStageState(initialState, 1280, 720, 'desktop');
  assert.equal(await page.locator('#pixi-root canvas').count(), 1);

  const initialProbe = await page.evaluate(() => {
    const probe = new window.PIXI.Container();
    probe.label = 'logicalProbe';
    probe.position.set(window.AzurLaneStageLayout.DESIGN_CENTER_X, window.AzurLaneStageLayout.DESIGN_CENTER_Y);
    window.azurLaneViewerShell.live2dLayer.addChild(probe);
    window.azurLaneViewerShell.logicalProbe = probe;

    return {
      x: probe.position.x,
      y: probe.position.y,
    };
  });

  assert.deepEqual(initialProbe, { x: 800, y: 450 });

  for (const viewport of viewports) {
    await page.setViewportSize({ width: viewport.width, height: viewport.height });
    await waitForFixedStageShell(page, viewport.width, viewport.height);

    const viewportState = await page.evaluate(() => {
      const state = window.azurLaneViewerShell.getState();
      const { logicalProbe } = window.azurLaneViewerShell;

      return {
        state,
        probe: {
          x: logicalProbe.position.x,
          y: logicalProbe.position.y,
        },
      };
    });

    assertFixedStageState(viewportState.state, viewport.width, viewport.height, viewport.name);
    assert.deepEqual(viewportState.probe, { x: 800, y: 450 }, viewport.name);
  }

  assert.deepEqual(modelAssetRequests, []);
  assert.deepEqual(pageErrors, []);
} finally {
  await browser.close();
  await new Promise((resolveClose) => server.close(resolveClose));
}
