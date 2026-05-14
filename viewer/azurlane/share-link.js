(function attachAzurLaneShareLink(globalScope) {
  'use strict';

  const SHARE_VERSION = 1;
  const SHARE_HASH_KEY = 'azls';
  const MODEL_TYPES = Object.freeze(new Set(['live2d', 'spine']));
  const MIN_MODEL_SCALE = 0.02;
  const MAX_MODEL_SCALE = 12;

  function clamp(value, minimum, maximum) {
    return Math.min(maximum, Math.max(minimum, value));
  }

  function base64UrlEncode(text) {
    if (typeof Buffer !== 'undefined') {
      return Buffer.from(text, 'utf8').toString('base64url');
    }

    const bytes = new TextEncoder().encode(text);
    let binary = '';
    for (const byte of bytes) {
      binary += String.fromCharCode(byte);
    }
    return globalScope
      .btoa(binary)
      .replaceAll('+', '-')
      .replaceAll('/', '_')
      .replace(/=+$/u, '');
  }

  function base64UrlDecode(token) {
    const value = String(token ?? '').trim();
    if (!value) {
      throw new Error('Share link payload is empty');
    }

    if (typeof Buffer !== 'undefined') {
      return Buffer.from(value, 'base64url').toString('utf8');
    }

    const padded = `${value.replaceAll('-', '+').replaceAll('_', '/')}${'='.repeat((4 - (value.length % 4)) % 4)}`;
    const binary = globalScope.atob(padded);
    const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0));
    return new TextDecoder().decode(bytes);
  }

  function normalizeTransform(transform) {
    const x = Number(transform?.x);
    const y = Number(transform?.y);
    const scale = Number(transform?.scale);
    const rotation = Number(transform?.rotation);

    if (!Number.isFinite(x) || !Number.isFinite(y) || !Number.isFinite(scale) || scale <= 0) {
      throw new Error('Share link transform is invalid');
    }

    return {
      x,
      y,
      scale: clamp(scale, MIN_MODEL_SCALE, MAX_MODEL_SCALE),
      ...(Number.isFinite(rotation) ? { rotation } : {}),
    };
  }

  function normalizeModel(model) {
    const id = String(model?.id ?? '').trim();
    const type = String(model?.type ?? '').trim();

    if (!id) {
      throw new Error('Share link model id is missing');
    }
    if (!MODEL_TYPES.has(type)) {
      throw new Error('Share link model type is invalid');
    }

    return { id, type };
  }

  function normalizeOptionalState(state) {
    if (!state || typeof state !== 'object') {
      return {};
    }

    const normalized = {};
    const backgroundColor = Number(state.backgroundColor);
    if (Number.isInteger(backgroundColor) && backgroundColor >= 0 && backgroundColor <= 0xffffff) {
      normalized.backgroundColor = backgroundColor;
    }
    for (const [sourceKey, targetKey] of [
      ['motion', 'motion'],
      ['expression', 'expression'],
    ]) {
      const value = String(state[sourceKey] ?? '').trim();
      if (value) {
        normalized[targetKey] = value;
      }
    }
    for (const [sourceKey, targetKey] of [
      ['textEnabled', 'textEnabled'],
      ['audioEnabled', 'audioEnabled'],
    ]) {
      if (typeof state[sourceKey] === 'boolean') {
        normalized[targetKey] = state[sourceKey];
      }
    }
    return normalized;
  }

  function normalizeSharePayload(payload) {
    if (Number(payload?.version ?? payload?.v) !== SHARE_VERSION) {
      throw new Error('Share link version is unsupported');
    }

    return {
      version: SHARE_VERSION,
      model: normalizeModel(payload.model),
      transform: normalizeTransform(payload.transform),
      state: normalizeOptionalState(payload.state),
    };
  }

  function compactSharePayload(payload) {
    const normalized = normalizeSharePayload(payload);
    const compact = {
      v: normalized.version,
      m: {
        i: normalized.model.id,
        t: normalized.model.type,
      },
      tr: {
        x: normalized.transform.x,
        y: normalized.transform.y,
        s: normalized.transform.scale,
        ...(Number.isFinite(normalized.transform.rotation) ? { r: normalized.transform.rotation } : {}),
      },
    };

    const state = normalized.state;
    const compactState = {
      ...(Number.isInteger(state.backgroundColor) ? { bg: state.backgroundColor } : {}),
      ...(state.motion ? { m: state.motion } : {}),
      ...(state.expression ? { e: state.expression } : {}),
      ...(typeof state.textEnabled === 'boolean' ? { txt: state.textEnabled } : {}),
      ...(typeof state.audioEnabled === 'boolean' ? { aud: state.audioEnabled } : {}),
    };
    if (Object.keys(compactState).length > 0) {
      compact.st = compactState;
    }

    return compact;
  }

  function expandSharePayload(payload) {
    if (payload?.v !== undefined || payload?.m || payload?.tr) {
      return {
        version: payload?.v,
        model: {
          id: payload?.m?.i,
          type: payload?.m?.t,
        },
        transform: {
          x: payload?.tr?.x,
          y: payload?.tr?.y,
          scale: payload?.tr?.s,
          ...(payload?.tr?.r !== undefined ? { rotation: payload.tr.r } : {}),
        },
        state: {
          ...(payload?.st?.bg !== undefined ? { backgroundColor: payload.st.bg } : {}),
          ...(payload?.st?.m !== undefined ? { motion: payload.st.m } : {}),
          ...(payload?.st?.e !== undefined ? { expression: payload.st.e } : {}),
          ...(payload?.st?.txt !== undefined ? { textEnabled: payload.st.txt } : {}),
          ...(payload?.st?.aud !== undefined ? { audioEnabled: payload.st.aud } : {}),
        },
      };
    }

    return payload;
  }

  function encodeSharePayload(payload) {
    return base64UrlEncode(JSON.stringify(compactSharePayload(payload)));
  }

  function decodeSharePayload(token) {
    return normalizeSharePayload(expandSharePayload(JSON.parse(base64UrlDecode(token))));
  }

  function decodeSharePayloadResult(token) {
    try {
      return { ok: true, payload: decodeSharePayload(token), error: null };
    } catch (error) {
      return {
        ok: false,
        payload: null,
        error: error instanceof Error ? error : new Error(String(error)),
      };
    }
  }

  function shareTokenFromUrl(urlLike) {
    const url = new URL(String(urlLike ?? globalScope.location?.href ?? ''), globalScope.location?.href ?? 'http://viewer.local/index.html');
    const hash = url.hash.startsWith('#') ? url.hash.slice(1) : url.hash;
    const hashParams = new URLSearchParams(hash);
    return hashParams.get(SHARE_HASH_KEY) || url.searchParams.get(SHARE_HASH_KEY) || '';
  }

  function decodeShareUrl(urlLike) {
    const token = shareTokenFromUrl(urlLike);
    if (!token) {
      return { ok: true, payload: null, error: null };
    }
    return decodeSharePayloadResult(token);
  }

  function createShareUrl(urlLike, payload) {
    const url = new URL(String(urlLike ?? globalScope.location?.href ?? ''), globalScope.location?.href ?? 'http://viewer.local/index.html');
    const hash = url.hash.startsWith('#') ? url.hash.slice(1) : url.hash;
    const hashParams = new URLSearchParams(hash);
    hashParams.set(SHARE_HASH_KEY, encodeSharePayload(payload));
    url.hash = hashParams.toString();
    return url.toString();
  }

  function createSharePayload({ entry, transform, state = {} }) {
    return normalizeSharePayload({
      version: SHARE_VERSION,
      model: {
        id: entry?.id,
        type: entry?.type,
      },
      transform,
      state,
    });
  }

  const api = Object.freeze({
    SHARE_VERSION,
    SHARE_HASH_KEY,
    createSharePayload,
    createShareUrl,
    decodeSharePayload,
    decodeSharePayloadResult,
    decodeShareUrl,
    encodeSharePayload,
    normalizeSharePayload,
    shareTokenFromUrl,
  });

  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
    return;
  }

  globalScope.AzurLaneShareLink = api;
})(typeof globalThis !== 'undefined' ? globalThis : window);
