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

async function waitForCenteredShell(page, width, height) {
  await page.waitForFunction(
    ({ expectedWidth, expectedHeight }) => {
      const state = window.azurLaneViewerShell?.getState?.();
      if (!state?.ready) {
        return false;
      }

      return (
        Math.abs(state.screen.width - expectedWidth) < 1
        && Math.abs(state.screen.height - expectedHeight) < 1
        && Math.abs(state.contentRoot.x - expectedWidth / 2) < 0.5
        && Math.abs(state.contentRoot.y - expectedHeight / 2) < 0.5
        && state.contentRoot.scaleX === state.contentRoot.scaleY
      );
    },
    { expectedWidth: width, expectedHeight: height },
  );
}

const { server, url } = await createStaticServer();
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1280, height: 720 } });
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
  await page.goto(url, { waitUntil: 'networkidle' });
  await waitForCenteredShell(page, 1280, 720);

  const initialState = await page.evaluate(() => window.azurLaneViewerShell.getState());
  assert.equal(initialState.pixiApplicationCount, 1);
  assert.deepEqual(initialState.stageChildren, ['canvasBackgroundLayer', 'contentRoot', 'overlayLayer']);
  assert.deepEqual(initialState.contentChildren, ['spineLayer', 'live2dLayer']);
  assert.equal(initialState.controlsRuntime, 'dom');
  assert.equal(initialState.modelLoadingRequested, false);
  assert.equal(initialState.backgroundColor, 0x151815);
  assert.equal(initialState.backgroundLayerChildren, 1);
  assert.ok(initialState.backgroundBounds.width >= 1280);
  assert.ok(initialState.backgroundBounds.height >= 720);
  assert.equal(await page.locator('#pixi-root canvas').count(), 1);

  await page.setViewportSize({ width: 390, height: 844 });
  await waitForCenteredShell(page, 390, 844);

  const resizedState = await page.evaluate(() => window.azurLaneViewerShell.getState());
  assert.equal(resizedState.contentRoot.scaleX, 1);
  assert.equal(resizedState.contentRoot.scaleY, 1);
  assert.equal(resizedState.overlayLayer.scaleX, 1);
  assert.equal(resizedState.overlayLayer.scaleY, 1);
  assert.deepEqual(modelAssetRequests, []);
  assert.deepEqual(pageErrors, []);
} finally {
  await browser.close();
  await new Promise((resolveClose) => server.close(resolveClose));
}
