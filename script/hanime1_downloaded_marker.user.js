// ==UserScript==
// @name         Hanime1 Downloaded Marker
// @namespace    fav
// @version      0.1.7
// @description  Mark downloaded Hanime1 videos with data from the fav FastAPI backend.
// @match        https://hanime1.me/*
// @grant        GM_addStyle
// @grant        GM_getValue
// @grant        GM_registerMenuCommand
// @grant        GM_setValue
// @grant        GM_xmlhttpRequest
// @connect      *
// ==/UserScript==

(function () {
  'use strict';

  const API_PATH = '/api/v2/hanime1/videos';
  const ANCHOR_SELECTOR = 'a[href*="/watch?v="]';
  const SYNC_INTERVAL_MS = 120_000;
  const OBSERVER_DEBOUNCE_MS = 300;
  const REQUEST_TIMEOUT_MS = 15_000;

  const KEY_API_BASE_URL = 'hanime1_marker_api_base_url';
  const KEY_API_TOKEN = 'hanime1_marker_api_token';
  const KEY_VIDEOS_CACHE = 'hanime1_marker_videos_cache';
  const KEY_LEGACY_IDS_CACHE = 'hanime1_marker_ids_cache';
  const KEY_ETAG = 'hanime1_marker_etag';
  const KEY_LAST_SUCCESS_AT = 'hanime1_marker_last_success_at';

  const TITLE_CLASS = 'hanime1-marker-title-downloaded';
  const LEGACY_BADGE_CLASS = 'hanime1-marker-badge';
  const LEGACY_HOST_CLASS = 'hanime1-marker-host';
  const BANNER_ID = 'hanime1-marker-stale-banner';

  let downloadedIdSet = new Set(
    loadCachedVideos()
      .filter((video) => video.downloaded)
      .map((video) => normalizeId(video.video_id))
  );
  let observer = null;
  let syncTimer = null;
  let syncInFlight = false;

  GM_addStyle(`
    .${TITLE_CLASS} {
      color: #22c55e !important;
    }
    .${TITLE_CLASS} * {
      color: #22c55e !important;
    }

    #${BANNER_ID} {
      position: fixed;
      top: 0;
      left: 0;
      right: 0;
      z-index: 2147483647;
      padding: 8px 12px;
      text-align: center;
      color: #fff;
      background: rgba(201, 62, 62, 0.95);
      font-size: 13px;
      line-height: 1.4;
      font-weight: 600;
    }
  `);

  GM_registerMenuCommand('Hanime1 Marker: Configure API', () => {
    promptAndSaveConfig();
  });

  GM_registerMenuCommand('Hanime1 Marker: Reset API Config', () => {
    GM_setValue(KEY_API_BASE_URL, '');
    GM_setValue(KEY_API_TOKEN, '');
    GM_setValue(KEY_ETAG, '');
    alert('Hanime1 Marker API config has been reset. Reload the page to reconfigure.');
  });

  function getStoredString(key) {
    const value = GM_getValue(key, '');
    return typeof value === 'string' ? value : '';
  }

  function normalizeVideo(rawVideo) {
    if (typeof rawVideo === 'string') {
      const videoId = normalizeId(rawVideo);
      if (!videoId) {
        return null;
      }
      return {
        video_id: videoId,
        title: '',
        downloaded: true,
        uploader: null,
        release_date: null,
        plot: null,
        watch_url: '',
      };
    }

    if (!rawVideo || typeof rawVideo !== 'object') {
      return null;
    }

    const videoId = normalizeId(rawVideo.video_id || rawVideo.id);
    if (!videoId) {
      return null;
    }

    return {
      video_id: videoId,
      title: typeof rawVideo.title === 'string' ? rawVideo.title : '',
      downloaded: rawVideo.downloaded !== false,
      uploader: typeof rawVideo.uploader === 'string' && rawVideo.uploader.trim() ? rawVideo.uploader : null,
      release_date: typeof rawVideo.release_date === 'string' && rawVideo.release_date.trim() ? rawVideo.release_date : null,
      plot: typeof rawVideo.plot === 'string' && rawVideo.plot.trim() ? rawVideo.plot : null,
      watch_url: typeof rawVideo.watch_url === 'string' ? rawVideo.watch_url : '',
    };
  }

  function normalizeVideos(rawVideos) {
    if (!Array.isArray(rawVideos)) {
      return [];
    }

    const output = [];
    const seen = new Set();
    for (const rawVideo of rawVideos) {
      const video = normalizeVideo(rawVideo);
      if (!video) {
        continue;
      }
      if (seen.has(video.video_id)) {
        continue;
      }
      seen.add(video.video_id);
      output.push(video);
    }
    return output;
  }

  function loadCachedVideos() {
    const raw = getStoredString(KEY_VIDEOS_CACHE) || getStoredString(KEY_LEGACY_IDS_CACHE);
    if (!raw) {
      return [];
    }
    try {
      const parsed = JSON.parse(raw);
      return normalizeVideos(parsed);
    } catch (_error) {
      return [];
    }
  }

  function normalizeId(itemId) {
    return String(itemId || '').trim().toLowerCase();
  }

  function normalizeApiBaseUrl(url) {
    return String(url || '').trim().replace(/\/+$/, '');
  }

  function parseWatchVideoId(href) {
    if (!href) {
      return null;
    }
    try {
      const url = new URL(href, window.location.origin);
      if (!url.pathname.includes('/watch')) {
        return null;
      }
      const itemId = normalizeId(url.searchParams.get('v'));
      return itemId || null;
    } catch (_error) {
      return null;
    }
  }

  function parseResponseHeader(headersText, headerName) {
    const lines = String(headersText || '')
      .split('\n')
      .map((line) => line.trim())
      .filter(Boolean);
    const target = headerName.toLowerCase();
    for (const line of lines) {
      const separatorIndex = line.indexOf(':');
      if (separatorIndex < 0) {
        continue;
      }
      const key = line.slice(0, separatorIndex).trim().toLowerCase();
      if (key !== target) {
        continue;
      }
      return line.slice(separatorIndex + 1).trim();
    }
    return '';
  }

  function requestDownloadedVideos({ apiBaseUrl, apiToken, etag }) {
    return new Promise((resolve, reject) => {
      const headers = {
        Authorization: `Bearer ${apiToken}`,
      };
      if (etag) {
        headers['If-None-Match'] = etag;
      }

      GM_xmlhttpRequest({
        method: 'GET',
        url: `${apiBaseUrl}${API_PATH}`,
        headers,
        timeout: REQUEST_TIMEOUT_MS,
        onload: (response) => {
          const nextEtag = parseResponseHeader(response.responseHeaders, 'etag');
          if (response.status === 304) {
            resolve({
              status: 304,
              ids: null,
              etag: nextEtag || etag || '',
            });
            return;
          }
          if (response.status !== 200) {
            reject(new Error(`API request failed with status ${response.status}`));
            return;
          }
          let payload = null;
          try {
            payload = JSON.parse(String(response.responseText || ''));
          } catch (_error) {
            reject(new Error('API payload is not valid JSON'));
            return;
          }
          resolve({
            status: 200,
            videos: normalizeVideos(payload.items || payload.ids),
            etag: nextEtag || '',
          });
        },
        onerror: () => {
          reject(new Error('API request failed'));
        },
        ontimeout: () => {
          reject(new Error('API request timed out'));
        },
      });
    });
  }

  function clearLegacyBadgeMarks(root = document) {
    const scope = root instanceof Element || root instanceof Document ? root : document;
    for (const badge of scope.querySelectorAll(`.${LEGACY_BADGE_CLASS}`)) {
      badge.remove();
    }
    for (const host of scope.querySelectorAll(`.${LEGACY_HOST_CLASS}`)) {
      host.classList.remove(LEGACY_HOST_CLASS);
    }
  }

  function markDownloadedCards(root = document) {
    clearLegacyBadgeMarks(root);
    const anchors = root.querySelectorAll(ANCHOR_SELECTOR);
    for (const anchor of anchors) {
      if (!(anchor instanceof HTMLAnchorElement)) {
        continue;
      }
      const itemId = parseWatchVideoId(anchor.getAttribute('href') || anchor.href);
      const shouldMark = Boolean(itemId && downloadedIdSet.has(itemId));
      anchor.classList.toggle(TITLE_CLASS, shouldMark);
    }
  }

  function showStaleBanner(lastSuccessAt) {
    const previous = document.getElementById(BANNER_ID);
    const readableTime = lastSuccessAt ? new Date(lastSuccessAt).toLocaleString() : 'unknown';
    const text = `已使用缓存，数据可能过期（上次成功同步：${readableTime}）`;
    if (previous) {
      previous.textContent = text;
      return;
    }
    const banner = document.createElement('div');
    banner.id = BANNER_ID;
    banner.textContent = text;
    document.body.appendChild(banner);
  }

  function clearStaleBanner() {
    const banner = document.getElementById(BANNER_ID);
    if (!banner) {
      return;
    }
    banner.remove();
  }

  function debounce(fn, waitMs) {
    let timeoutId = null;
    return (...args) => {
      if (timeoutId !== null) {
        clearTimeout(timeoutId);
      }
      timeoutId = window.setTimeout(() => {
        timeoutId = null;
        fn(...args);
      }, waitMs);
    };
  }

  function ensureObserver() {
    if (observer || !document.body) {
      return;
    }
    const scheduleMarking = debounce(() => {
      markDownloadedCards(document);
    }, OBSERVER_DEBOUNCE_MS);
    observer = new MutationObserver((mutations) => {
      for (const mutation of mutations) {
        if (mutation.type !== 'childList' || mutation.addedNodes.length === 0) {
          continue;
        }
        scheduleMarking();
        return;
      }
    });
    observer.observe(document.body, { childList: true, subtree: true });
  }

  function promptAndSaveConfig() {
    const currentBaseUrl = getStoredString(KEY_API_BASE_URL);
    const currentToken = getStoredString(KEY_API_TOKEN);
    const inputBaseUrl = prompt('Hanime1 Marker API base URL (e.g. https://api.example.com)', currentBaseUrl);
    if (inputBaseUrl === null) {
      return;
    }
    const normalizedBaseUrl = normalizeApiBaseUrl(inputBaseUrl);
    if (!normalizedBaseUrl) {
      alert('API base URL cannot be empty.');
      return;
    }

    const inputToken = prompt('Hanime1 Marker API token', currentToken);
    if (inputToken === null) {
      return;
    }
    const normalizedToken = String(inputToken).trim();
    if (!normalizedToken) {
      alert('API token cannot be empty.');
      return;
    }

    GM_setValue(KEY_API_BASE_URL, normalizedBaseUrl);
    GM_setValue(KEY_API_TOKEN, normalizedToken);
    alert('Hanime1 Marker API config saved. Reload the page to apply.');
  }

  function ensureConfigFromStorage() {
    const apiBaseUrl = normalizeApiBaseUrl(getStoredString(KEY_API_BASE_URL));
    const apiToken = getStoredString(KEY_API_TOKEN).trim();
    if (apiBaseUrl && apiToken) {
      return { apiBaseUrl, apiToken };
    }

    const inputBaseUrl = prompt('Hanime1 Marker API base URL (e.g. https://api.example.com)', apiBaseUrl || '');
    if (inputBaseUrl === null) {
      return null;
    }
    const normalizedBaseUrl = normalizeApiBaseUrl(inputBaseUrl);
    if (!normalizedBaseUrl) {
      alert('API base URL cannot be empty.');
      return null;
    }

    const inputToken = prompt('Hanime1 Marker API token', apiToken || '');
    if (inputToken === null) {
      return null;
    }
    const normalizedToken = String(inputToken).trim();
    if (!normalizedToken) {
      alert('API token cannot be empty.');
      return null;
    }

    GM_setValue(KEY_API_BASE_URL, normalizedBaseUrl);
    GM_setValue(KEY_API_TOKEN, normalizedToken);
    return { apiBaseUrl: normalizedBaseUrl, apiToken: normalizedToken };
  }

  async function syncDownloadedIds(config) {
    if (syncInFlight) {
      return;
    }
    syncInFlight = true;
    try {
      const currentEtag = getStoredString(KEY_ETAG);
      const response = await requestDownloadedVideos({
        apiBaseUrl: config.apiBaseUrl,
        apiToken: config.apiToken,
        etag: currentEtag,
      });
      if (response.status === 304) {
        clearStaleBanner();
        markDownloadedCards(document);
        return;
      }

      const videos = response.videos || [];
      downloadedIdSet = new Set(
        videos
          .filter((video) => video.downloaded)
          .map((video) => normalizeId(video.video_id))
      );
      GM_setValue(KEY_VIDEOS_CACHE, JSON.stringify(videos));
      GM_setValue(KEY_ETAG, response.etag || '');
      GM_setValue(KEY_LAST_SUCCESS_AT, new Date().toISOString());
      clearStaleBanner();
      markDownloadedCards(document);
    } catch (error) {
      const lastSuccessAt = getStoredString(KEY_LAST_SUCCESS_AT);
      showStaleBanner(lastSuccessAt);
      markDownloadedCards(document);
      console.error('[Hanime1 Marker] sync failed:', error);
    } finally {
      syncInFlight = false;
    }
  }

  async function init() {
    markDownloadedCards(document);
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', () => {
        ensureObserver();
        markDownloadedCards(document);
      });
    } else {
      ensureObserver();
    }

    const config = ensureConfigFromStorage();
    if (!config) {
      showStaleBanner(getStoredString(KEY_LAST_SUCCESS_AT));
      return;
    }

    await syncDownloadedIds(config);
    syncTimer = window.setInterval(() => {
      void syncDownloadedIds(config);
    }, SYNC_INTERVAL_MS);
  }

  void init();
})();
