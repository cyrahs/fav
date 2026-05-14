import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { inflateSync } from 'node:zlib';
import { createRequire } from 'node:module';
import { extname, join, normalize, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { chromium } = require('playwright');
const { MODELS, VIEWPORTS } = require('../visual-regression-set.js');

const viewerRoot = resolve(fileURLToPath(new URL('..', import.meta.url)));
const screenshotOutputDir = process.env.AZURLANE_VISUAL_OUTPUT_DIR ? resolve(process.env.AZURLANE_VISUAL_OUTPUT_DIR) : '';
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

function sampleCatalogPayload() {
  return {
    Master: [
      {
        gameId: 1,
        gameName: 'Azur Lane',
        character: [],
      },
    ],
  };
}

function assertClose(actual, expected, message, tolerance = 0.75) {
  assert.ok(Math.abs(actual - expected) <= tolerance, `${message}: expected ${expected}, received ${actual}`);
}

function readChunk(buffer, offset) {
  const length = buffer.readUInt32BE(offset);
  const type = buffer.toString('ascii', offset + 4, offset + 8);
  const dataStart = offset + 8;
  return {
    length,
    type,
    data: buffer.subarray(dataStart, dataStart + length),
    nextOffset: dataStart + length + 4,
  };
}

function bytesPerPixel(colorType) {
  if (colorType === 2) {
    return 3;
  }
  if (colorType === 6) {
    return 4;
  }
  throw new Error(`Unsupported PNG color type ${colorType}`);
}

function paethPredictor(left, up, upLeft) {
  const estimate = left + up - upLeft;
  const leftDistance = Math.abs(estimate - left);
  const upDistance = Math.abs(estimate - up);
  const upLeftDistance = Math.abs(estimate - upLeft);

  if (leftDistance <= upDistance && leftDistance <= upLeftDistance) {
    return left;
  }
  if (upDistance <= upLeftDistance) {
    return up;
  }
  return upLeft;
}

function unfilterScanline(filter, current, previous, bpp) {
  for (let index = 0; index < current.length; index += 1) {
    const left = index >= bpp ? current[index - bpp] : 0;
    const up = previous ? previous[index] : 0;
    const upLeft = previous && index >= bpp ? previous[index - bpp] : 0;

    if (filter === 1) {
      current[index] = (current[index] + left) & 0xff;
    } else if (filter === 2) {
      current[index] = (current[index] + up) & 0xff;
    } else if (filter === 3) {
      current[index] = (current[index] + Math.floor((left + up) / 2)) & 0xff;
    } else if (filter === 4) {
      current[index] = (current[index] + paethPredictor(left, up, upLeft)) & 0xff;
    } else if (filter !== 0) {
      throw new Error(`Unsupported PNG filter ${filter}`);
    }
  }
}

function decodePng(buffer) {
  assert.equal(buffer.toString('hex', 0, 8), '89504e470d0a1a0a');

  let width = 0;
  let height = 0;
  let colorType = 0;
  const imageChunks = [];
  let offset = 8;

  while (offset < buffer.length) {
    const chunk = readChunk(buffer, offset);
    offset = chunk.nextOffset;

    if (chunk.type === 'IHDR') {
      width = chunk.data.readUInt32BE(0);
      height = chunk.data.readUInt32BE(4);
      assert.equal(chunk.data[8], 8, 'Only 8-bit PNG screenshots are supported');
      colorType = chunk.data[9];
    } else if (chunk.type === 'IDAT') {
      imageChunks.push(chunk.data);
    } else if (chunk.type === 'IEND') {
      break;
    }
  }

  const bpp = bytesPerPixel(colorType);
  const rowLength = width * bpp;
  const inflated = inflateSync(Buffer.concat(imageChunks));
  const pixels = Buffer.alloc(width * height * 4);
  let sourceOffset = 0;
  let previous = null;

  for (let y = 0; y < height; y += 1) {
    const filter = inflated[sourceOffset];
    sourceOffset += 1;
    const row = Buffer.from(inflated.subarray(sourceOffset, sourceOffset + rowLength));
    sourceOffset += rowLength;
    unfilterScanline(filter, row, previous, bpp);

    for (let x = 0; x < width; x += 1) {
      const sourceIndex = x * bpp;
      const targetIndex = (y * width + x) * 4;
      pixels[targetIndex] = row[sourceIndex];
      pixels[targetIndex + 1] = row[sourceIndex + 1];
      pixels[targetIndex + 2] = row[sourceIndex + 2];
      pixels[targetIndex + 3] = bpp === 4 ? row[sourceIndex + 3] : 255;
    }

    previous = row;
  }

  return { width, height, pixels };
}

function colorDistanceSquared(left, right) {
  return (left.red - right.red) ** 2 + (left.green - right.green) ** 2 + (left.blue - right.blue) ** 2;
}

function countColorPixels(image, expectedColor, tolerance = 18) {
  const maxDistance = tolerance ** 2;
  let count = 0;

  for (let index = 0; index < image.pixels.length; index += 4) {
    const actual = {
      red: image.pixels[index],
      green: image.pixels[index + 1],
      blue: image.pixels[index + 2],
    };
    if (colorDistanceSquared(actual, expectedColor) <= maxDistance) {
      count += 1;
    }
  }

  return count;
}

function logicalBoundsForCurrentModel(modelCase, current) {
  const bounds = current.fit.dimensions ?? current.fit.bounds;
  const left = current.x + (bounds.x - (current.pivotX ?? 0)) * current.scaleX;
  const top = current.y + (bounds.y - (current.pivotY ?? 0)) * current.scaleY;
  const width = bounds.width * current.scaleX;
  const height = bounds.height * current.scaleY;

  return {
    id: modelCase.entry.id,
    left,
    top,
    right: left + width,
    bottom: top + height,
    width,
    height,
    centerX: left + width / 2,
    centerY: top + height / 2,
  };
}

function assertModelPlacement(modelCase, current, viewportName) {
  const projected = logicalBoundsForCurrentModel(modelCase, current);
  const tolerance = 1;

  assert.equal(current.visible, true, `${modelCase.entry.id} visible in ${viewportName}`);
  assertClose(current.x, 800, `${modelCase.entry.id} logical x in ${viewportName}`);
  assertClose(current.y, 450, `${modelCase.entry.id} logical y in ${viewportName}`);
  assertClose(current.scaleX, current.scaleY, `${modelCase.entry.id} uniform scale in ${viewportName}`, 0.001);
  assertClose(current.scaleX, modelCase.expectedScale, `${modelCase.entry.id} fixed model scale in ${viewportName}`, 0.001);
  assert.ok(projected.left >= -tolerance, `${modelCase.entry.id} clipped left in ${viewportName}: ${projected.left}`);
  assert.ok(projected.top >= -tolerance, `${modelCase.entry.id} clipped top in ${viewportName}: ${projected.top}`);
  assert.ok(projected.right <= 1600 + tolerance, `${modelCase.entry.id} clipped right in ${viewportName}: ${projected.right}`);
  assert.ok(projected.bottom <= 900 + tolerance, `${modelCase.entry.id} clipped bottom in ${viewportName}: ${projected.bottom}`);
  assertClose(projected.centerX, 800, `${modelCase.entry.id} projected center x in ${viewportName}`);
  assertClose(projected.centerY, 450, `${modelCase.entry.id} projected center y in ${viewportName}`);
}

async function installFakeVisualModel(page, modelCase) {
  return page.evaluate(async (serializableModelCase) => {
    const makeColor = (color) => (color.red << 16) + (color.green << 8) + color.blue;
    const drawMainMarker = (container, bounds, color) => {
      const marker = new window.PIXI.Graphics();
      marker.label = 'visual-main-marker';
      marker.rect(bounds.x, bounds.y, bounds.width, bounds.height).fill({ color: makeColor(color), alpha: 1 });
      container.addChild(marker);
    };

    if (serializableModelCase.entry.type === 'live2d') {
      const runtime = {
        Live2DModel: {
          async from() {
            const model = new window.PIXI.Container();
            model.label = serializableModelCase.entry.id;
            model.name = serializableModelCase.entry.id;
            model.anchor = {
              x: 0,
              y: 0,
              set(x, y) {
                this.x = x;
                this.y = y;
              },
            };
            drawMainMarker(model, serializableModelCase.fakeBounds, serializableModelCase.markerColor);
            model.getLocalBounds = () => ({ ...serializableModelCase.fakeBounds });
            return model;
          },
        },
      };

      await window.azurLaneViewerShell.loadLive2DEntry(serializableModelCase.entry, {
        runtime,
        dimensionOptions: {
          stableFrames: 2,
          maxFrames: 6,
        },
      });
    } else {
      const runtime = {
        Spine: {
          from() {
            const spine = new window.PIXI.Container();
            spine.label = serializableModelCase.entry.id;
            spine.name = serializableModelCase.entry.id;
            spine.skeleton = {
              data: {
                slots: serializableModelCase.backgroundSlots.map((name) => ({ name })),
                animations: [{ name: 'normal' }],
              },
            };
            spine.state = {
              setAnimation() {},
            };
            spine.update = () => {};
            drawMainMarker(spine, serializableModelCase.fakeBounds, serializableModelCase.markerColor);
            for (const [index, slotName] of serializableModelCase.backgroundSlots.entries()) {
              const background = new window.PIXI.Graphics();
              background.label = slotName;
              background.name = slotName;
              background
                .rect(serializableModelCase.fakeBounds.x + 60 + index * 180, serializableModelCase.fakeBounds.y + 60, 140, 90)
                .fill({ color: makeColor(serializableModelCase.backgroundColor), alpha: 1 });
              spine.addChild(background);
            }
            spine.getLocalBounds = () => ({ ...serializableModelCase.fakeBounds });
            return spine;
          },
        },
      };
      const assets = {
        add() {},
        async load() {
          return {};
        },
        async unload() {},
      };

      await window.azurLaneViewerShell.loadSpineEntry(serializableModelCase.entry, {
        runtime,
        assets,
      });
    }

    await new Promise((resolve) => window.requestAnimationFrame(resolve));
    window.azurLaneViewerShell.app.render();
  }, modelCase);
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
      );
    },
    { expectedWidth: width, expectedHeight: height },
  );
}

async function writeOptionalArtifact(path, content) {
  if (!screenshotOutputDir) {
    return;
  }

  await mkdir(screenshotOutputDir, { recursive: true });
  await writeFile(join(screenshotOutputDir, path), content);
}

const { server, url } = await createStaticServer();
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1600, height: 900 } });
const pageErrors = [];
const externalAssetRequests = [];
const report = [];

page.on('pageerror', (error) => pageErrors.push(error.message));
page.on('request', (request) => {
  const requestUrl = request.url();
  if (requestUrl.includes('/live2d/azurlane/') || /\.(?:model3\.json|moc3|skel|atlas)(?:[?#]|$)/.test(requestUrl)) {
    externalAssetRequests.push(requestUrl);
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
  await page.goto(url, { waitUntil: 'networkidle' });
  await waitForFixedStageShell(page, 1600, 900);

  for (const modelCase of MODELS) {
    await installFakeVisualModel(page, modelCase);

    for (const viewport of VIEWPORTS) {
      await page.setViewportSize({ width: viewport.width, height: viewport.height });
      await waitForFixedStageShell(page, viewport.width, viewport.height);
      const state = await page.evaluate(() => window.azurLaneViewerShell.getState());
      const current = modelCase.entry.type === 'live2d' ? state.live2d.current : state.spine.current;
      assert.equal(current.entryId, modelCase.entry.id, `${modelCase.entry.id} selected in ${viewport.name}`);
      assertModelPlacement(modelCase, current, viewport.name);

      const screenshot = await page.locator('#pixi-root canvas').screenshot();
      const image = decodePng(screenshot);
      const markerPixels = countColorPixels(image, modelCase.markerColor);
      assert.ok(
        markerPixels >= modelCase.minimumMarkerPixels,
        `${modelCase.entry.id} has too few marker pixels in ${viewport.name}: ${markerPixels}`,
      );

      const check = {
        modelId: modelCase.entry.id,
        category: modelCase.category,
        viewport: viewport.name,
        width: viewport.width,
        height: viewport.height,
        markerPixels,
      };

      if (modelCase.entry.type === 'spine') {
        const backgroundPixels = countColorPixels(image, modelCase.backgroundColor);
        assert.ok(
          backgroundPixels >= modelCase.minimumBackgroundPixels,
          `${modelCase.entry.id} has too few background attachment pixels in ${viewport.name}: ${backgroundPixels}`,
        );
        check.backgroundPixels = backgroundPixels;
      }

      report.push(check);
      await writeOptionalArtifact(`${modelCase.category}-${viewport.name}.png`, screenshot);
    }
  }

  await writeOptionalArtifact('visual-regression-report.json', `${JSON.stringify(report, null, 2)}\n`);
  assert.equal(report.length, MODELS.length * VIEWPORTS.length);
  assert.deepEqual(externalAssetRequests, []);
  assert.deepEqual(pageErrors, []);
} finally {
  await browser.close();
  await new Promise((resolveClose) => server.close(resolveClose));
}
