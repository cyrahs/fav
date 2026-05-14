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

function sampleCatalogPayload() {
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
                costumeId: 7,
                costumeName: '永不落幕的茶会',
                costumeNameEn: 'Never-Ending Tea Party',
                path: 'https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json',
              },
              {
                costumeId: 8,
                costumeName: '破损条目',
                costumeNameEn: 'Broken Entry',
                path: 'https://static.l2d.su/live2d/azurlane/broken/broken.model3.json',
                availability: {
                  state: 'broken',
                },
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
await page.route('https://l2d.su/json/live2dMaster.json', async (route) => {
  await route.fulfill({
    status: 200,
    contentType: 'application/json; charset=utf-8',
    body: JSON.stringify(sampleCatalogPayload()),
  });
});

try {
  await page.goto(`${url}?debugStage=1`, { waitUntil: 'networkidle' });
  await waitForFixedStageShell(page, 1280, 720);
  await page.waitForFunction(() => window.azurLaneModelCatalogController?.state?.entries?.length === 3);

  const initialState = await page.evaluate(() => window.azurLaneViewerShell.getState());
  assert.equal(initialState.pixiApplicationCount, 1);
  assert.deepEqual(initialState.stageChildren, ['canvasBackgroundLayer', 'contentRoot', 'overlayLayer']);
  assert.deepEqual(initialState.contentChildren, ['spineLayer', 'live2dLayer', 'stageDebugLayer']);
  assert.equal(initialState.controlsRuntime, 'dom');
  assert.equal(initialState.modelLoadingRequested, false);
  assert.equal(initialState.backgroundColor, 0x151815);
  assert.equal(initialState.backgroundLayerChildren, 1);
  assert.equal(initialState.spineLayerChildren, 0);
  assert.equal(initialState.live2dLayerChildren, 0);
  assert.equal(initialState.spine.loadCount, 0);
  assert.equal(initialState.spine.current, null);
  assert.equal(initialState.live2d.loadCount, 0);
  assert.equal(initialState.live2d.current, null);
  assert.ok(initialState.backgroundBounds.width >= 1280);
  assert.ok(initialState.backgroundBounds.height >= 720);
  assertFixedStageState(initialState, 1280, 720, 'desktop');
  assert.equal(await page.locator('#pixi-root canvas').count(), 1);
  assert.equal(await page.locator('.catalog-entry').count(), 2);
  assert.equal(await page.locator('.catalog-entry', { hasText: 'Broken Entry' }).count(), 0);

  await page.locator('#catalog-search').fill('光辉');
  assert.deepEqual(await page.locator('.catalog-entry-title').allTextContents(), ['Illustrious 光辉']);
  await page.locator('#catalog-search').fill('Golden Afternoon');
  assert.deepEqual(await page.locator('.catalog-entry-subtitle').allTextContents(), ['Golden Afternoon 金色午后']);
  await page.locator('[data-model-type="live2d"]').click();
  assert.equal(await page.locator('.catalog-entry').count(), 0);
  await page.locator('[data-model-type="spine"]').click();
  assert.deepEqual(await page.locator('.catalog-entry-title').allTextContents(), ['Elise 伊丽丝']);
  await page.locator('[data-model-type="all"]').click();
  await page.locator('#catalog-search').fill('');

  const selectionSmoke = await page.evaluate(async () => {
    const liveModel = new window.PIXI.Container();
    liveModel.anchor = {
      x: 0,
      y: 0,
      set(x, y) {
        this.x = x;
        this.y = y;
      },
    };
    liveModel.getLocalBounds = () => ({ x: -400, y: -200, width: 800, height: 400 });
    window.PIXI.live2d = {
      Live2DModel: {
        async from(url) {
          liveModel.azurLaneLoadedUrl = url;
          return liveModel;
        },
      },
    };

    const spineModel = new window.PIXI.Container();
    const animationCalls = [];
    spineModel.pivot = {
      x: 0,
      y: 0,
      set(x, y) {
        this.x = x;
        this.y = y;
      },
    };
    spineModel.skeleton = {
      data: {
        animations: [{ name: 'normal' }],
      },
    };
    spineModel.state = {
      setAnimation(trackIndex, name, loop) {
        animationCalls.push({ trackIndex, name, loop });
      },
    };
    spineModel.update = () => {};
    spineModel.getLocalBounds = () => ({ x: -500, y: -1000, width: 1000, height: 2000 });
    window.spine = {
      Spine: {
        from() {
          return spineModel;
        },
      },
    };

    const assetCalls = [];
    window.PIXI.Assets.add = (descriptor) => assetCalls.push(['add', descriptor.alias, descriptor.src]);
    window.PIXI.Assets.load = async (aliases) => {
      assetCalls.push(['load', ...aliases]);
      return {};
    };
    window.PIXI.Assets.unload = async (aliases) => {
      assetCalls.push(['unload', ...aliases]);
    };

    const live2dEntry = window.azurLaneModelCatalogController.state.entries.find((entry) => entry.id === 'azurlane:live2d:guanghui:guanghui_7');
    const spineEntry = window.azurLaneModelCatalogController.state.entries.find((entry) => entry.id === 'azurlane:spine:yilisi:yilisi_2_doa');
    await window.azurLaneModelCatalogController.selectEntry(live2dEntry);
    const afterLive2D = window.azurLaneViewerShell.getState();
    await window.azurLaneModelCatalogController.selectEntry(spineEntry);
    const afterSpine = window.azurLaneViewerShell.getState();

    return {
      live2dUrl: liveModel.azurLaneLoadedUrl,
      afterLive2D: {
        live2dLayerChildren: afterLive2D.live2dLayerChildren,
        spineLayerChildren: afterLive2D.spineLayerChildren,
        entryId: afterLive2D.live2d.current.entryId,
      },
      afterSpine: {
        live2dLayerChildren: afterSpine.live2dLayerChildren,
        spineLayerChildren: afterSpine.spineLayerChildren,
        entryId: afterSpine.spine.current.entryId,
      },
      animationCalls,
      assetCalls,
    };
  });

  assert.equal(selectionSmoke.live2dUrl, 'https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json');
  assert.deepEqual(selectionSmoke.afterLive2D, {
    live2dLayerChildren: 1,
    spineLayerChildren: 0,
    entryId: 'azurlane:live2d:guanghui:guanghui_7',
  });
  assert.deepEqual(selectionSmoke.afterSpine, {
    live2dLayerChildren: 0,
    spineLayerChildren: 1,
    entryId: 'azurlane:spine:yilisi:yilisi_2_doa',
  });
  assert.deepEqual(selectionSmoke.animationCalls, [{ trackIndex: 0, name: 'normal', loop: true }]);
  assert.ok(selectionSmoke.assetCalls.some((call) => call[0] === 'load' && call.includes('azurlane:spine:yilisi:yilisi_2_doa:skeleton')));

  const concurrentSelectionSmoke = await page.evaluate(async () => {
    const live2dEntry = window.azurLaneModelCatalogController.state.entries.find((entry) => entry.id === 'azurlane:live2d:guanghui:guanghui_7');
    const spineEntry = window.azurLaneModelCatalogController.state.entries.find((entry) => entry.id === 'azurlane:spine:yilisi:yilisi_2_doa');
    let resolveLive2DLoad;
    const slowLive2DLoad = new Promise((resolve) => {
      resolveLive2DLoad = resolve;
    });
    const staleLiveModel = new window.PIXI.Container();
    staleLiveModel.anchor = {
      x: 0,
      y: 0,
      set(x, y) {
        this.x = x;
        this.y = y;
      },
    };
    staleLiveModel.getLocalBounds = () => ({ x: -400, y: -200, width: 800, height: 400 });
    staleLiveModel.destroyedByTest = false;
    const originalDestroy = staleLiveModel.destroy.bind(staleLiveModel);
    staleLiveModel.destroy = (options) => {
      staleLiveModel.destroyedByTest = true;
      originalDestroy(options);
    };
    window.PIXI.live2d = {
      Live2DModel: {
        async from() {
          await slowLive2DLoad;
          return staleLiveModel;
        },
      },
    };

    const spineModel = new window.PIXI.Container();
    spineModel.pivot = {
      x: 0,
      y: 0,
      set(x, y) {
        this.x = x;
        this.y = y;
      },
    };
    spineModel.skeleton = {
      data: {
        animations: [{ name: 'normal' }],
      },
    };
    spineModel.state = {
      setAnimation() {},
    };
    spineModel.update = () => {};
    spineModel.getLocalBounds = () => ({ x: -500, y: -1000, width: 1000, height: 2000 });
    window.spine = {
      Spine: {
        from() {
          return spineModel;
        },
      },
    };
    window.PIXI.Assets.add = () => {};
    window.PIXI.Assets.load = async () => ({});
    window.PIXI.Assets.unload = async () => {};

    const staleSelection = window.azurLaneModelCatalogController.selectEntry(live2dEntry);
    const currentSelection = window.azurLaneModelCatalogController.selectEntry(spineEntry);
    await currentSelection;
    resolveLive2DLoad();
    await staleSelection;

    const state = window.azurLaneViewerShell.getState();
    return {
      selectedEntryId: window.azurLaneModelCatalogController.state.selectedEntryId,
      loadingEntryId: window.azurLaneModelCatalogController.state.loadingEntryId,
      live2dLayerChildren: state.live2dLayerChildren,
      spineLayerChildren: state.spineLayerChildren,
      live2dCurrent: state.live2d.current,
      spineCurrentEntryId: state.spine.current.entryId,
      staleLiveDestroyed: staleLiveModel.destroyedByTest,
      staleLiveParentLabel: staleLiveModel.parent?.label ?? '',
    };
  });

  assert.deepEqual(concurrentSelectionSmoke, {
    selectedEntryId: 'azurlane:spine:yilisi:yilisi_2_doa',
    loadingEntryId: '',
    live2dLayerChildren: 0,
    spineLayerChildren: 1,
    live2dCurrent: null,
    spineCurrentEntryId: 'azurlane:spine:yilisi:yilisi_2_doa',
    staleLiveDestroyed: true,
    staleLiveParentLabel: '',
  });

  const shareSmoke = await page.evaluate(async () => {
    const liveModel = new window.PIXI.Container();
    liveModel.anchor = {
      x: 0,
      y: 0,
      set(x, y) {
        this.x = x;
        this.y = y;
      },
    };
    liveModel.getLocalBounds = () => ({ x: -400, y: -200, width: 800, height: 400 });
    window.PIXI.live2d = {
      Live2DModel: {
        async from() {
          return liveModel;
        },
      },
    };

    const entry = window.azurLaneModelCatalogController.state.entries.find((candidate) => candidate.id === 'azurlane:live2d:guanghui:guanghui_7');
    await window.azurLaneModelCatalogController.selectEntry(entry);
    const restored = window.azurLaneViewerShell.applyActiveTransform({
      x: 930,
      y: 510,
      scale: 1.42,
      rotation: 0.125,
    });
    const url = window.azurLaneViewerShell.createActiveShareUrl(window.location.href);
    const decoded = window.AzurLaneShareLink.decodeShareUrl(url);

    return {
      restored,
      url,
      decoded,
      shareDisabled: document.querySelector('#share-link').disabled,
    };
  });

  assert.equal(shareSmoke.shareDisabled, false);
  assert.deepEqual(shareSmoke.restored, {
    x: 930,
    y: 510,
    scale: 1.42,
    rotation: 0.125,
  });
  assert.equal(shareSmoke.decoded.ok, true);
  assert.deepEqual(shareSmoke.decoded.payload, {
    version: 1,
    model: {
      id: 'azurlane:live2d:guanghui:guanghui_7',
      type: 'live2d',
    },
    transform: {
      x: 930,
      y: 510,
      scale: 1.42,
      rotation: 0.125,
    },
    state: {
      backgroundColor: 0x151815,
    },
  });

  const restorePageErrors = [];
  const restorePage = await browser.newPage({ viewport: { width: 900, height: 900 } });
  restorePage.on('pageerror', (error) => restorePageErrors.push(error.message));
  await restorePage.addInitScript(() => {
    const installRuntime = () => {
      if (!window.PIXI?.Container) {
        window.requestAnimationFrame(installRuntime);
        return;
      }

      window.PIXI.live2d = {
        Live2DModel: {
          async from(url) {
            const model = new window.PIXI.Container();
            model.anchor = {
              x: 0,
              y: 0,
              set(x, y) {
                this.x = x;
                this.y = y;
              },
            };
            model.azurLaneLoadedUrl = url;
            model.getLocalBounds = () => ({ x: -400, y: -200, width: 800, height: 400 });
            return model;
          },
        },
      };
    };

    installRuntime();
  });
  await restorePage.route('https://l2d.su/json/live2dMaster.json', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json; charset=utf-8',
      body: JSON.stringify(sampleCatalogPayload()),
    });
  });
  await restorePage.goto(shareSmoke.url, { waitUntil: 'networkidle' });
  await waitForFixedStageShell(restorePage, 900, 900);
  await restorePage.waitForFunction(() => window.azurLaneModelCatalogController?.state?.selectedEntryId === 'azurlane:live2d:guanghui:guanghui_7');

  const restoredShareState = await restorePage.evaluate(() => ({
    current: window.azurLaneViewerShell.getState().live2d.current,
    selectedEntryId: window.azurLaneModelCatalogController.state.selectedEntryId,
    meta: document.querySelector('#catalog-meta').textContent,
  }));
  await restorePage.close();

  assert.equal(restoredShareState.selectedEntryId, 'azurlane:live2d:guanghui:guanghui_7');
  assert.equal(restoredShareState.current.entryId, 'azurlane:live2d:guanghui:guanghui_7');
  assert.equal(restoredShareState.current.userTransformed, true);
  assertClose(restoredShareState.current.x, 930, 'share link logical x on different viewport');
  assertClose(restoredShareState.current.y, 510, 'share link logical y on different viewport');
  assertClose(restoredShareState.current.scaleX, 1.42, 'share link logical scale on different viewport');
  assertClose(restoredShareState.current.rotation, 0.125, 'share link logical rotation on different viewport', 0.001);
  assert.match(restoredShareState.meta, /Share link restored/u);
  assert.deepEqual(restorePageErrors, []);

  const malformedSharePageErrors = [];
  const malformedSharePage = await browser.newPage({ viewport: { width: 1280, height: 720 } });
  malformedSharePage.on('pageerror', (error) => malformedSharePageErrors.push(error.message));
  await malformedSharePage.route('https://l2d.su/json/live2dMaster.json', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json; charset=utf-8',
      body: JSON.stringify(sampleCatalogPayload()),
    });
  });
  await malformedSharePage.goto(`${url}#azls=not-json`, { waitUntil: 'networkidle' });
  await waitForFixedStageShell(malformedSharePage, 1280, 720);
  await malformedSharePage.waitForFunction(() => window.azurLaneModelCatalogController?.state?.entries?.length === 3);
  const malformedShareState = await malformedSharePage.evaluate(() => ({
    selectedEntryId: window.azurLaneModelCatalogController.state.selectedEntryId,
    meta: document.querySelector('#catalog-meta').textContent,
    live2dCurrent: window.azurLaneViewerShell.getState().live2d.current,
    spineCurrent: window.azurLaneViewerShell.getState().spine.current,
  }));
  await malformedSharePage.close();

  assert.equal(malformedShareState.selectedEntryId, '');
  assert.equal(malformedShareState.live2dCurrent, null);
  assert.equal(malformedShareState.spineCurrent, null);
  assert.match(malformedShareState.meta, /Share link ignored:/u);
  assert.deepEqual(malformedSharePageErrors, []);

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

  const live2dLoads = await page.evaluate(async () => {
    const cases = [
      {
        id: 'azurlane:live2d:xingdengbao:xingdengbao_2',
        url: 'https://static.l2d.su/live2d/azurlane/xingdengbao_2/xingdengbao_2.model3.json',
        bounds: { x: -400, y: -200, width: 800, height: 400 },
      },
      {
        id: 'azurlane:live2d:yuanchou:yuanchou_3',
        url: 'https://static.l2d.su/live2d/azurlane/yuanchou_3/yuanchou_3.model3.json',
        bounds: { x: -225, y: -600, width: 450, height: 1200 },
      },
      {
        id: 'azurlane:live2d:mingji:mingji_2',
        url: 'https://static.l2d.su/live2d/azurlane/mingji_2/mingji_2.model3.json',
        bounds: { x: -250, y: -250, width: 500, height: 500 },
      },
    ];
    const loads = [];

    for (const testCase of cases) {
      const runtime = {
        Live2DModel: {
          async from(url, options) {
            const model = new window.PIXI.Container();
            model.label = testCase.id;
            model.name = testCase.id;
            model.anchor = {
              x: 0,
              y: 0,
              set(x, y) {
                this.x = x;
                this.y = y;
              },
            };
            model.getLocalBounds = () => ({ ...testCase.bounds });
            model.azurLaneLoadOptions = options;
            model.azurLaneLoadedUrl = url;
            return model;
          },
        },
      };

      const result = await window.azurLaneViewerShell.loadLive2DEntry(
        {
          id: testCase.id,
          type: 'live2d',
          resources: {
            primary_url: testCase.url,
          },
          layout: {
            mode: 'auto-fit',
            anchor: [0.5, 0.5],
          },
        },
        {
          runtime,
          dimensionOptions: {
            stableFrames: 2,
            maxFrames: 6,
          },
        },
      );
      const state = window.azurLaneViewerShell.getState();
      loads.push({
        id: testCase.id,
        parentLabel: result.model.parent.label,
        loadedUrl: result.model.azurLaneLoadedUrl,
        loadOptions: result.model.azurLaneLoadOptions,
        live2dLayerChildren: state.live2dLayerChildren,
        spineLayerChildren: state.spineLayerChildren,
        current: state.live2d.current,
      });
    }

    return loads;
  });

  assert.equal(live2dLoads.length, 3);
  assert.deepEqual(
    live2dLoads.map((load) => load.parentLabel),
    ['live2dLayer', 'live2dLayer', 'live2dLayer'],
  );
  assert.deepEqual(
    live2dLoads.map((load) => load.loadedUrl),
    [
      'https://static.l2d.su/live2d/azurlane/xingdengbao_2/xingdengbao_2.model3.json',
      'https://static.l2d.su/live2d/azurlane/yuanchou_3/yuanchou_3.model3.json',
      'https://static.l2d.su/live2d/azurlane/mingji_2/mingji_2.model3.json',
    ],
  );
  assert.deepEqual(
    live2dLoads.map((load) => load.loadOptions),
    [
      { autoInteract: false, autoFocus: false, autoHitTest: false },
      { autoInteract: false, autoFocus: false, autoHitTest: false },
      { autoInteract: false, autoFocus: false, autoHitTest: false },
    ],
  );
  assert.deepEqual(
    live2dLoads.map((load) => load.spineLayerChildren),
    [0, 0, 0],
  );
  assert.deepEqual(
    live2dLoads.map((load) => load.live2dLayerChildren),
    [1, 1, 1],
  );
  assert.deepEqual(
    live2dLoads.map((load) => load.current.entryId),
    [
      'azurlane:live2d:xingdengbao:xingdengbao_2',
      'azurlane:live2d:yuanchou:yuanchou_3',
      'azurlane:live2d:mingji:mingji_2',
    ],
  );
  assertClose(live2dLoads[0].current.scaleX, 2, 'wide Live2D fit');
  assertClose(live2dLoads[1].current.scaleX, 0.75, 'tall Live2D fit');
  assertClose(live2dLoads[2].current.scaleX, 1.8, 'square Live2D fit');
  for (const load of live2dLoads) {
    assertClose(load.current.x, 800, `${load.id} logical x`);
    assertClose(load.current.y, 450, `${load.id} logical y`);
    assertClose(load.current.scaleX, load.current.scaleY, `${load.id} uniform scale`);
    assert.equal(load.current.visible, true);
    assert.equal(load.current.fit.dimensions.timedOut, false);
  }

  const live2dBeforeResize = await page.evaluate(() => window.azurLaneViewerShell.getState().live2d.current);
  await page.setViewportSize({ width: 1920, height: 720 });
  await waitForFixedStageShell(page, 1920, 720);
  const live2dAfterResize = await page.evaluate(() => window.azurLaneViewerShell.getState().live2d.current);

  assert.equal(live2dAfterResize.entryId, live2dBeforeResize.entryId);
  assertClose(live2dAfterResize.x, live2dBeforeResize.x, 'Live2D logical x after resize');
  assertClose(live2dAfterResize.y, live2dBeforeResize.y, 'Live2D logical y after resize');
  assertClose(live2dAfterResize.scaleX, live2dBeforeResize.scaleX, 'Live2D scale x after resize');
  assertClose(live2dAfterResize.scaleY, live2dBeforeResize.scaleY, 'Live2D scale y after resize');

  const interactionStart = await page.evaluate(() => {
    const state = window.azurLaneViewerShell.getState();
    return {
      root: state.contentRoot,
      current: state.live2d.current,
      resetDisabled: document.querySelector('#reset-transform').disabled,
    };
  });
  assert.equal(interactionStart.resetDisabled, true);
  const dragStartX = interactionStart.root.x + interactionStart.current.x * interactionStart.root.scaleX;
  const dragStartY = interactionStart.root.y + interactionStart.current.y * interactionStart.root.scaleY;
  await page.mouse.move(dragStartX, dragStartY);
  await page.mouse.down();
  await page.mouse.move(dragStartX + 160, dragStartY + 80, { steps: 4 });
  await page.mouse.up();

  const afterDrag = await page.evaluate(() => {
    const state = window.azurLaneViewerShell.getState();
    return {
      current: state.live2d.current,
      resetDisabled: document.querySelector('#reset-transform').disabled,
    };
  });
  assert.equal(afterDrag.resetDisabled, false);
  assert.equal(afterDrag.current.userTransformed, true);
  assertClose(afterDrag.current.x, interactionStart.current.x + 200, 'Live2D logical x after drag');
  assertClose(afterDrag.current.y, interactionStart.current.y + 100, 'Live2D logical y after drag');
  assertClose(afterDrag.current.scaleX, interactionStart.current.scaleX, 'Live2D drag keeps scale');

  await page.mouse.move(dragStartX + 160, dragStartY + 80);
  await page.mouse.wheel(0, -240);
  const afterWheel = await page.evaluate(() => {
    const state = window.azurLaneViewerShell.getState();
    return {
      current: state.live2d.current,
      stored: JSON.parse(localStorage.getItem(`azurlane-viewer-transform:${encodeURIComponent(state.live2d.current.entryId)}`)),
    };
  });
  assert.equal(afterWheel.current.userTransformed, true);
  assert.ok(afterWheel.current.scaleX > afterDrag.current.scaleX, 'wheel zoom increases scale');
  assert.deepEqual(afterWheel.current.savedTransform, afterWheel.stored);

  await page.setViewportSize({ width: 390, height: 844 });
  await waitForFixedStageShell(page, 390, 844);
  const afterInteractionResize = await page.evaluate(() => window.azurLaneViewerShell.getState().live2d.current);
  assert.equal(afterInteractionResize.entryId, afterWheel.current.entryId);
  assertClose(afterInteractionResize.x, afterWheel.current.x, 'dragged Live2D x survives resize');
  assertClose(afterInteractionResize.y, afterWheel.current.y, 'dragged Live2D y survives resize');
  assertClose(afterInteractionResize.scaleX, afterWheel.current.scaleX, 'zoomed Live2D scale survives resize');

  const restoredTransform = await page.evaluate(async () => {
    const entry = {
      id: 'azurlane:live2d:mingji:mingji_2',
      type: 'live2d',
      resources: {
        primary_url: 'https://static.l2d.su/live2d/azurlane/mingji_2/mingji_2.model3.json',
      },
      layout: {
        mode: 'auto-fit',
        anchor: [0.5, 0.5],
      },
    };
    const runtime = {
      Live2DModel: {
        async from() {
          const model = new window.PIXI.Container();
          model.anchor = {
            x: 0,
            y: 0,
            set(x, y) {
              this.x = x;
              this.y = y;
            },
          };
          model.getLocalBounds = () => ({ x: -250, y: -250, width: 500, height: 500 });
          return model;
        },
      },
    };

    await window.azurLaneViewerShell.loadLive2DEntry(entry, {
      runtime,
      dimensionOptions: {
        stableFrames: 2,
        maxFrames: 6,
      },
    });
    return window.azurLaneViewerShell.getState().live2d.current;
  });
  assert.equal(restoredTransform.userTransformed, true);
  assertClose(restoredTransform.x, afterWheel.current.x, 'saved Live2D x restores on different viewport');
  assertClose(restoredTransform.y, afterWheel.current.y, 'saved Live2D y restores on different viewport');
  assertClose(restoredTransform.scaleX, afterWheel.current.scaleX, 'saved Live2D scale restores on different viewport');

  await page.locator('#reset-transform').click();
  const afterReset = await page.evaluate(() => {
    const state = window.azurLaneViewerShell.getState();
    return {
      current: state.live2d.current,
      stored: localStorage.getItem(`azurlane-viewer-transform:${encodeURIComponent(state.live2d.current.entryId)}`),
      resetDisabled: document.querySelector('#reset-transform').disabled,
    };
  });
  assert.equal(afterReset.resetDisabled, true);
  assert.equal(afterReset.current.userTransformed, false);
  assert.equal(afterReset.current.savedTransform, null);
  assert.equal(afterReset.stored, null);
  assertClose(afterReset.current.x, afterReset.current.fit.x, 'Live2D reset restores fit x');
  assertClose(afterReset.current.y, afterReset.current.fit.y, 'Live2D reset restores fit y');
  assertClose(afterReset.current.scaleX, afterReset.current.fit.scale, 'Live2D reset restores fit scale');

  const spineLoads = await page.evaluate(async () => {
    const cases = [
      {
        id: 'azurlane:spine:aerbien:aerbien_4',
        baseUrl: 'https://static.l2d.su/live2d/azurlane/aerbien_4-spine',
        bounds: { x: -500, y: -1000, width: 1000, height: 2000 },
        backgroundSlots: ['bj_background', 'bj_window'],
      },
      {
        id: 'azurlane:spine:yilisi:yilisi_2_doa',
        baseUrl: 'https://static.l2d.su/live2d/azurlane/yilisi_2_doa',
        bounds: { x: -800, y: -300, width: 1600, height: 600 },
        backgroundSlots: ['bj_sea'],
      },
      {
        id: 'azurlane:spine:zhuzi:zhuzi_2_doa',
        baseUrl: 'https://static.l2d.su/live2d/azurlane/zhuzi_2_doa',
        bounds: { x: -350, y: -350, width: 700, height: 700 },
        backgroundSlots: ['bj_room', 'bj_light'],
      },
    ];
    const loads = [];

    for (const testCase of cases) {
      const assetCalls = {
        add: [],
        load: [],
      };
      const animationCalls = [];
      const assets = {
        add(descriptor) {
          assetCalls.add.push({
            alias: descriptor.alias,
            src: descriptor.src,
            data: descriptor.data,
          });
        },
        async load(aliases) {
          assetCalls.load.push(aliases);
          return {};
        },
      };
      const runtime = {
        Spine: {
          from(options) {
            const model = new window.PIXI.Container();
            model.label = testCase.id;
            model.name = testCase.id;
            model.azurLaneSpineOptions = options;
            model.azurLaneAnimationCalls = animationCalls;
            model.skeleton = {
              data: {
                slots: testCase.backgroundSlots.map((name) => ({ name })),
                animations: [{ name: 'touch' }, { name: 'normal' }],
              },
            };
            model.state = {
              setAnimation(trackIndex, name, loop) {
                animationCalls.push({ trackIndex, name, loop });
              },
            };
            model.getLocalBounds = () => ({ ...testCase.bounds });
            for (const slotName of testCase.backgroundSlots) {
              const attachment = new window.PIXI.Graphics();
              attachment.label = slotName;
              attachment.name = slotName;
              attachment.rect(-80, -40, 160, 80).fill({ color: 0x406fb2, alpha: 0.6 });
              model.addChild(attachment);
            }
            return model;
          },
        },
      };

      const result = await window.azurLaneViewerShell.loadSpineEntry(
        {
          id: testCase.id,
          type: 'spine',
          resources: {
            primary_url: testCase.baseUrl,
          },
          layout: {
            mode: 'auto-fit',
          },
        },
        {
          runtime,
          assets,
        },
      );
      const state = window.azurLaneViewerShell.getState();
      loads.push({
        id: testCase.id,
        parentLabel: result.spine.parent.label,
        runtimeOptions: result.spine.azurLaneSpineOptions,
        assetCalls,
        animationCalls,
        childLabels: result.spine.children.map((child) => child.label || child.name),
        spineLayerChildren: state.spineLayerChildren,
        live2dLayerChildren: state.live2dLayerChildren,
        current: state.spine.current,
      });
    }

    return loads;
  });

  assert.equal(spineLoads.length, 3);
  assert.deepEqual(
    spineLoads.map((load) => load.parentLabel),
    ['spineLayer', 'spineLayer', 'spineLayer'],
  );
  assert.deepEqual(
    spineLoads.map((load) => load.spineLayerChildren),
    [1, 1, 1],
  );
  assert.deepEqual(
    spineLoads.map((load) => load.live2dLayerChildren),
    [0, 0, 0],
  );
  assert.deepEqual(
    spineLoads.map((load) => load.current.entryId),
    [
      'azurlane:spine:aerbien:aerbien_4',
      'azurlane:spine:yilisi:yilisi_2_doa',
      'azurlane:spine:zhuzi:zhuzi_2_doa',
    ],
  );
  assert.deepEqual(spineLoads[0].assetCalls.add.map((call) => call.src), [
    'https://static.l2d.su/live2d/azurlane/aerbien_4-spine/aerbien_4.skel',
    'https://static.l2d.su/live2d/azurlane/aerbien_4-spine/aerbien_4.atlas',
  ]);
  assert.deepEqual(spineLoads[0].assetCalls.add[1].data.images, {
    'aerbien_4.webp': 'https://static.l2d.su/live2d/azurlane/aerbien_4-spine/aerbien_4.webp',
  });
  assert.deepEqual(spineLoads[1].assetCalls.add.map((call) => call.src), [
    'https://static.l2d.su/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.skel',
    'https://static.l2d.su/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.atlas',
  ]);
  assert.deepEqual(spineLoads[2].assetCalls.add.map((call) => call.src), [
    'https://static.l2d.su/live2d/azurlane/zhuzi_2_doa/zhuzi_2_doa.skel',
    'https://static.l2d.su/live2d/azurlane/zhuzi_2_doa/zhuzi_2_doa.atlas',
  ]);
  for (const load of spineLoads) {
    assert.equal(load.assetCalls.load.length, 1, load.id);
    assert.equal(load.assetCalls.load[0].length, 2, load.id);
    assert.equal(load.runtimeOptions.autoUpdate, true, load.id);
    assert.equal(load.runtimeOptions.skeleton, load.assetCalls.load[0][0], load.id);
    assert.equal(load.runtimeOptions.atlas, load.assetCalls.load[0][1], load.id);
    assert.deepEqual(load.animationCalls, [{ trackIndex: 0, name: 'normal', loop: true }], load.id);
    assert.ok(load.childLabels.some((label) => label.startsWith('bj_')), load.id);
    assert.equal(load.current.defaultAnimation.name, 'normal', load.id);
    assert.equal(load.current.defaultAnimation.started, true, load.id);
    assertClose(load.current.x, 800, `${load.id} logical x`);
    assertClose(load.current.y, 450, `${load.id} logical y`);
    assertClose(load.current.scaleX, load.current.scaleY, `${load.id} uniform scale`);
    assert.equal(load.current.visible, true);
  }
  assertClose(spineLoads[0].current.scaleX, 0.45, 'tall Spine fit');
  assertClose(spineLoads[1].current.scaleX, 1, 'wide Spine fit');
  assertClose(spineLoads[2].current.scaleX, 900 / 700, 'square Spine fit');

  const spineBeforeResize = await page.evaluate(() => window.azurLaneViewerShell.getState().spine.current);
  await page.setViewportSize({ width: 390, height: 844 });
  await waitForFixedStageShell(page, 390, 844);
  const spineAfterResize = await page.evaluate(() => window.azurLaneViewerShell.getState().spine.current);

  assert.equal(spineAfterResize.entryId, spineBeforeResize.entryId);
  assertClose(spineAfterResize.x, spineBeforeResize.x, 'Spine logical x after resize');
  assertClose(spineAfterResize.y, spineBeforeResize.y, 'Spine logical y after resize');
  assertClose(spineAfterResize.scaleX, spineBeforeResize.scaleX, 'Spine scale x after resize');
  assertClose(spineAfterResize.scaleY, spineBeforeResize.scaleY, 'Spine scale y after resize');

  assert.deepEqual(modelAssetRequests, []);
  assert.deepEqual(pageErrors, []);
} finally {
  await browser.close();
  await new Promise((resolveClose) => server.close(resolveClose));
}
