const assert = require('node:assert/strict');
const test = require('node:test');

const {
  SHARE_HASH_KEY,
  createSharePayload,
  createShareUrl,
  decodeSharePayload,
  decodeSharePayloadResult,
  decodeShareUrl,
  encodeSharePayload,
  shareTokenFromUrl,
} = require('../share-link.js');

test('encodeSharePayload serializes selected model, logical transform, and optional state compactly', () => {
  const payload = createSharePayload({
    entry: {
      id: 'azurlane:live2d:guanghui:guanghui_7',
      type: 'live2d',
    },
    transform: {
      x: 924.5,
      y: 512.25,
      scale: 1.375,
      rotation: 0,
    },
    state: {
      backgroundColor: 0x151815,
      textEnabled: true,
      audioEnabled: false,
    },
  });

  const token = encodeSharePayload(payload);

  assert.doesNotMatch(token, /[+/=]/u);
  assert.deepEqual(decodeSharePayload(token), payload);
});

test('createShareUrl stores compact state in the hash and keeps normal query parameters intact', () => {
  const url = createShareUrl('https://viewer.example/index.html?debugStage=1#panel=open', {
    version: 1,
    model: {
      id: 'azurlane:spine:yilisi:yilisi_2_doa',
      type: 'spine',
    },
    transform: {
      x: 720,
      y: 455,
      scale: 0.85,
    },
    state: {
      backgroundColor: 0x151815,
      motion: 'normal',
    },
  });

  assert.equal(new URL(url).search, '?debugStage=1');
  assert.equal(new URLSearchParams(new URL(url).hash.slice(1)).get('panel'), 'open');
  assert.equal(shareTokenFromUrl(url), new URLSearchParams(new URL(url).hash.slice(1)).get(SHARE_HASH_KEY));
  assert.deepEqual(decodeShareUrl(url).payload, {
    version: 1,
    model: {
      id: 'azurlane:spine:yilisi:yilisi_2_doa',
      type: 'spine',
    },
    transform: {
      x: 720,
      y: 455,
      scale: 0.85,
    },
    state: {
      backgroundColor: 0x151815,
      motion: 'normal',
    },
  });
});

test('decodeSharePayloadResult reports malformed links as recoverable errors', () => {
  const malformed = decodeSharePayloadResult('not-json');
  const unsupported = decodeSharePayloadResult(Buffer.from(JSON.stringify({ v: 99 }), 'utf8').toString('base64url'));

  assert.equal(malformed.ok, false);
  assert.ok(malformed.error instanceof Error);
  assert.equal(unsupported.ok, false);
  assert.ok(unsupported.error instanceof Error);
});
