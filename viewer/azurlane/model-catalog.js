(function attachAzurLaneModelCatalog(globalScope) {
  'use strict';

  const DEFAULT_CATALOG_URL = 'https://l2d.su/json/live2dMaster.json';
  const AZUR_LANE_GAME_ID = 1;
  const MODEL_TYPES = Object.freeze(['live2d', 'spine']);
  const READY_STATES = Object.freeze(new Set(['valid', 'fallback-only', 'unchecked', '']));
  const OVERRIDE_LAYOUT_FIELDS = Object.freeze(['scaleOverride', 'offsetX', 'offsetY']);

  function normalizeSearchText(value) {
    return String(value ?? '').normalize('NFKC').toLocaleLowerCase();
  }

  function compactText(...values) {
    return values
      .map((value) => String(value ?? '').trim())
      .filter(Boolean)
      .join(' ');
  }

  function catalogKey(value, fallback = 'unknown') {
    const key = String(value ?? '').normalize('NFKC').trim();
    return key || fallback;
  }

  function lastPathSegments(url) {
    try {
      return new URL(url).pathname.split('/').filter(Boolean);
    } catch {
      return String(url ?? '').split('/').filter(Boolean);
    }
  }

  function modelKeyFromPath(model) {
    const segments = lastPathSegments(model?.path);
    const filename = segments.at(-1) ?? '';

    if (model?.kind === 'live2d') {
      const parent = segments.at(-2) ?? '';
      if (parent) {
        return catalogKey(parent);
      }
      return catalogKey(filename.replace(/\.model3\.json$/i, ''), `costume-${model?.costumeId ?? 'unknown'}`);
    }

    return catalogKey(filename, `costume-${model?.costumeId ?? 'unknown'}`);
  }

  function catalogEntryId(type, characterKey, costumeKey) {
    return `azurlane:${type}:${catalogKey(characterKey)}:${catalogKey(costumeKey)}`;
  }

  function toCatalogEntry(character, model) {
    const type = model.kind;
    const characterKey = catalogKey(character.charKey);
    const costumeKey = modelKeyFromPath(model);

    return {
      id: catalogEntryId(type, characterKey, costumeKey),
      type,
      source: 'l2d.su',
      character: {
        id: Number.isInteger(character.charId) ? character.charId : undefined,
        key: characterKey,
        name_zh: String(character.charName ?? ''),
        name_en: String(character.charNameEn ?? ''),
      },
      costume: {
        id: Number.isInteger(model.costumeId) ? model.costumeId : undefined,
        key: costumeKey,
        name_zh: String(model.costumeName ?? ''),
        name_en: String(model.costumeNameEn ?? ''),
      },
      resources: {
        primary_url: String(model.path ?? ''),
        fallback_url: '',
        display_info_url: '',
      },
      capabilities: {
        motions: [],
        expressions: [],
        has_audio: false,
        has_text: false,
        has_display_info: false,
      },
      layout: {
        mode: 'auto-fit',
        anchor: [0.5, 0.5],
      },
      availability: {
        state: model.availability?.state ?? 'unchecked',
        validated_url: model.availability?.validated_url ?? model.availability?.validatedUrl ?? '',
        checked_at: model.availability?.checked_at ?? model.availability?.checkedAt ?? '',
        message: model.availability?.message ?? '',
      },
    };
  }

  function modelList(character, kind) {
    const models = Array.isArray(character?.[kind]) ? character[kind] : [];
    return models
      .filter((model) => model && typeof model === 'object' && typeof model.path === 'string' && model.path.trim())
      .map((model) => ({ ...model, kind }));
  }

  function normalizeL2DSuCatalog(payload, options = {}) {
    const gameId = options.gameId ?? AZUR_LANE_GAME_ID;
    const masters = Array.isArray(payload?.Master) ? payload.Master : [];
    const game = masters.find((item) => item?.gameId === gameId) ?? null;
    const characters = Array.isArray(game?.character) ? game.character : [];
    const entries = [];

    for (const character of characters) {
      for (const model of [...modelList(character, 'live2d'), ...modelList(character, 'spine')]) {
        entries.push(toCatalogEntry(character, model));
      }
    }

    entries.sort((left, right) => left.id.localeCompare(right.id));
    return {
      entries,
      source: 'l2d.su',
      summary: {
        entry_count: entries.length,
        by_type: {
          live2d: entries.filter((entry) => entry.type === 'live2d').length,
          spine: entries.filter((entry) => entry.type === 'spine').length,
        },
      },
    };
  }

  function normalizeCatalogPayload(payload, options = {}) {
    if (Array.isArray(payload?.entries)) {
      const entries = payload.entries.map(normalizeEntry).sort((left, right) => left.id.localeCompare(right.id));
      return {
        ...payload,
        entries: applyModelOverrides(entries, options.overrides),
      };
    }
    if (Array.isArray(payload?.catalog?.entries)) {
      return normalizeCatalogPayload(payload.catalog, options);
    }
    return applyOverridesToCatalog(normalizeL2DSuCatalog(payload, options), options.overrides);
  }

  function camelOrSnake(item, camelName, snakeName, fallback = '') {
    return item?.[camelName] ?? item?.[snakeName] ?? fallback;
  }

  function normalizeEntry(entry) {
    const character = entry?.character ?? {};
    const costume = entry?.costume ?? {};
    const resources = entry?.resources ?? {};
    const availability = entry?.availability ?? {};
    const type = MODEL_TYPES.includes(entry?.type) ? entry.type : 'live2d';
    const characterKey = catalogKey(character.key);
    const costumeKey = catalogKey(costume.key);

    return {
      ...entry,
      id: entry?.id || catalogEntryId(type, characterKey, costumeKey),
      type,
      source: entry?.source ?? 'l2d.su',
      character: {
        ...character,
        key: characterKey,
        name_zh: camelOrSnake(character, 'nameZh', 'name_zh'),
        name_en: camelOrSnake(character, 'nameEn', 'name_en'),
      },
      costume: {
        ...costume,
        key: costumeKey,
        name_zh: camelOrSnake(costume, 'nameZh', 'name_zh'),
        name_en: camelOrSnake(costume, 'nameEn', 'name_en'),
      },
      resources: {
        ...resources,
        primary_url: camelOrSnake(resources, 'primaryUrl', 'primary_url'),
        fallback_url: camelOrSnake(resources, 'fallbackUrl', 'fallback_url'),
        display_info_url: camelOrSnake(resources, 'displayInfoUrl', 'display_info_url'),
      },
      availability: {
        ...availability,
        state: availability.state ?? 'unchecked',
        validated_url: camelOrSnake(availability, 'validatedUrl', 'validated_url'),
        checked_at: camelOrSnake(availability, 'checkedAt', 'checked_at'),
        message: availability.message ?? '',
      },
    };
  }

  function finiteOverrideNumber(value) {
    const numberValue = Number(value);
    return Number.isFinite(numberValue) ? numberValue : null;
  }

  function normalizeModelOverrides(overrides = globalScope.AzurLaneModelOverrides ?? {}) {
    const source = overrides?.entries && typeof overrides.entries === 'object' ? overrides.entries : overrides;
    if (!source || typeof source !== 'object') {
      return {};
    }

    const normalized = {};
    for (const [rawId, rawOverride] of Object.entries(source)) {
      const id = String(rawId ?? '').trim();
      if (!id || !rawOverride || typeof rawOverride !== 'object') {
        continue;
      }

      const override = {};
      for (const field of OVERRIDE_LAYOUT_FIELDS) {
        const value = finiteOverrideNumber(rawOverride[field]);
        if (value !== null) {
          override[field] = value;
        }
      }

      const defaultMotion = String(rawOverride.defaultMotion ?? rawOverride.default_motion ?? '').trim();
      if (defaultMotion) {
        override.defaultMotion = defaultMotion;
      }

      const notes = String(rawOverride.notes ?? '').trim();
      if (notes) {
        override.notes = notes;
      }

      if (Object.keys(override).length > 0) {
        normalized[id] = override;
      }
    }

    return normalized;
  }

  function applyModelOverride(entry, override) {
    const normalizedEntry = normalizeEntry(entry);
    if (!override) {
      return normalizedEntry;
    }

    const nextEntry = {
      ...normalizedEntry,
      layout: {
        ...(normalizedEntry.layout ?? {}),
      },
      override: {
        ...(normalizedEntry.override ?? {}),
        source: 'model-overrides',
      },
    };

    let hasLayoutOverride = false;
    for (const field of OVERRIDE_LAYOUT_FIELDS) {
      if (Object.prototype.hasOwnProperty.call(override, field)) {
        nextEntry.layout[field] = override[field];
        hasLayoutOverride = true;
      }
    }

    if (hasLayoutOverride && !nextEntry.layout.mode) {
      nextEntry.layout.mode = 'auto-fit';
    }

    if (override.defaultMotion) {
      nextEntry.defaultAnimation = override.defaultMotion;
      nextEntry.default_animation = override.defaultMotion;
      nextEntry.override.defaultMotion = override.defaultMotion;
    }

    if (override.notes) {
      nextEntry.override.notes = override.notes;
    }

    return nextEntry;
  }

  function applyModelOverrides(entries, overrides = globalScope.AzurLaneModelOverrides ?? {}) {
    const normalizedOverrides = normalizeModelOverrides(overrides);
    return entries.map((entry) => {
      const normalizedEntry = normalizeEntry(entry);
      return applyModelOverride(normalizedEntry, normalizedOverrides[normalizedEntry.id]);
    });
  }

  function applyOverridesToCatalog(catalog, overrides = globalScope.AzurLaneModelOverrides ?? {}) {
    return {
      ...catalog,
      entries: applyModelOverrides(catalog?.entries ?? [], overrides),
    };
  }

  function entrySearchHaystack(entry) {
    return normalizeSearchText(
      compactText(
        entry.id,
        entry.type,
        entry.source,
        entry.character?.key,
        entry.character?.name_zh,
        entry.character?.name_en,
        entry.costume?.key,
        entry.costume?.name_zh,
        entry.costume?.name_en,
      ),
    );
  }

  function isEntryVisibleByDebug(entry, debugMode) {
    const state = entry?.availability?.state ?? 'unchecked';
    const hasResource = Boolean(entry?.resources?.validated_url || entry?.resources?.primary_url || entry?.resources?.primaryUrl);
    return debugMode || (hasResource && READY_STATES.has(state));
  }

  function filterCatalogEntries(entries, options = {}) {
    const type = options.type ?? 'all';
    const terms = normalizeSearchText(options.query ?? '')
      .split(/\s+/)
      .filter(Boolean);
    const debugMode = Boolean(options.debugMode);

    return entries.filter((entry) => {
      if (type !== 'all' && entry.type !== type) {
        return false;
      }
      if (!isEntryVisibleByDebug(entry, debugMode)) {
        return false;
      }
      if (terms.length === 0) {
        return true;
      }

      const haystack = entrySearchHaystack(entry);
      return terms.every((term) => haystack.includes(term));
    });
  }

  function displayName(entry) {
    const character = compactText(entry.character?.name_en, entry.character?.name_zh) || entry.character?.key || 'Unknown character';
    const costume = compactText(entry.costume?.name_en, entry.costume?.name_zh) || entry.costume?.key || 'Default';
    return { character, costume };
  }

  function debugModeFromLocation() {
    try {
      const params = new URLSearchParams(globalScope.location?.search ?? '');
      return params.has('debug') || params.has('debugCatalog');
    } catch {
      return false;
    }
  }

  function catalogUrlFromLocation() {
    try {
      const params = new URLSearchParams(globalScope.location?.search ?? '');
      const explicitUrl = params.get('catalogUrl');
      if (explicitUrl) {
        return explicitUrl;
      }
      if (params.get('catalog') === 'off') {
        return '';
      }
    } catch {
      return DEFAULT_CATALOG_URL;
    }
    return DEFAULT_CATALOG_URL;
  }

  async function fetchCatalog(url, options = {}) {
    if (!url) {
      return { entries: [], source: 'disabled', summary: { entry_count: 0, by_type: { live2d: 0, spine: 0 } } };
    }

    const fetcher = options.fetch ?? globalScope.fetch?.bind(globalScope);
    if (typeof fetcher !== 'function') {
      throw new Error('Catalog fetch is unavailable');
    }

    const response = await fetcher(url, { cache: 'default' });
    if (!response.ok) {
      throw new Error(`Catalog request failed with HTTP ${response.status}`);
    }
    return normalizeCatalogPayload(await response.json(), options);
  }

  function createCatalogController(options = {}) {
    const root = options.root ?? document.querySelector('#viewer-controls');
    const list = options.list ?? document.querySelector('#catalog-list');
    const search = options.search ?? document.querySelector('#catalog-search');
    const meta = options.meta ?? document.querySelector('#catalog-meta');
    const typeButtons = Array.from(options.typeButtons ?? document.querySelectorAll('[data-model-type]'));
    const shell = options.shell ?? globalScope.azurLaneViewerShell;
    const debugMode = options.debugMode ?? debugModeFromLocation();
    const state = {
      entries: [],
      filteredEntries: [],
      type: 'all',
      query: '',
      loadingEntryId: '',
      selectedEntryId: '',
      error: null,
      notice: '',
      loadSequence: 0,
    };

    function setMeta(text) {
      if (meta) {
        meta.textContent = text;
      }
    }

    function render() {
      state.filteredEntries = filterCatalogEntries(state.entries, {
        type: state.type,
        query: state.query,
        debugMode,
      });

      for (const button of typeButtons) {
        button.classList.toggle('is-active', button.dataset.modelType === state.type);
        button.setAttribute('aria-pressed', String(button.dataset.modelType === state.type));
      }

      const countText = `${state.filteredEntries.length} of ${state.entries.length} models`;
      setMeta(state.notice ? `${state.notice} · ${countText}` : countText);
      if (!list) {
        return;
      }

      list.replaceChildren(...state.filteredEntries.map(renderEntryButton));
    }

    function renderEntryButton(entry) {
      const { character, costume } = displayName(entry);
      const button = document.createElement('button');
      const title = document.createElement('span');
      const subtitle = document.createElement('span');
      const badges = document.createElement('span');
      const typeBadge = document.createElement('span');
      const sourceBadge = document.createElement('span');

      button.type = 'button';
      button.className = 'catalog-entry';
      button.dataset.entryId = entry.id;
      button.setAttribute('role', 'option');
      button.setAttribute('aria-selected', String(entry.id === state.selectedEntryId));
      button.disabled = Boolean(state.loadingEntryId);
      title.className = 'catalog-entry-title';
      subtitle.className = 'catalog-entry-subtitle';
      badges.className = 'catalog-entry-badges';
      typeBadge.className = `catalog-badge catalog-badge-${entry.type}`;
      sourceBadge.className = 'catalog-badge catalog-badge-source';
      title.textContent = character;
      subtitle.textContent = costume;
      typeBadge.textContent = entry.type === 'live2d' ? 'Live2D' : 'Spine';
      sourceBadge.textContent = entry.availability?.state === 'broken' ? 'Broken' : entry.source;
      badges.append(typeBadge, sourceBadge);
      button.append(title, subtitle, badges);
      button.addEventListener('click', () => selectEntry(entry));
      return button;
    }

    async function selectEntry(entry) {
      if (!shell) {
        throw new Error('Viewer shell is not ready');
      }

      const sequence = state.loadSequence + 1;
      state.loadSequence = sequence;
      state.loadingEntryId = entry.id;
      state.error = null;
      state.notice = '';
      setMeta(`Loading ${entry.type === 'live2d' ? 'Live2D' : 'Spine'}`);
      render();

      try {
        if (typeof shell.loadCatalogEntry === 'function') {
          await shell.loadCatalogEntry(entry);
        } else if (entry.type === 'live2d') {
          shell.clearSpineLayer?.();
          await shell.loadLive2DEntry(entry);
        } else {
          shell.clearLive2DLayer?.();
          await shell.loadSpineEntry(entry);
        }
        if (sequence !== state.loadSequence) {
          return null;
        }
        state.selectedEntryId = entry.id;
        return entry;
      } catch (error) {
        if (sequence !== state.loadSequence || error?.name === 'AbortError') {
          return null;
        }
        state.error = error;
        setMeta(error instanceof Error ? error.message : String(error));
        throw error;
      } finally {
        if (sequence === state.loadSequence) {
          state.loadingEntryId = '';
          render();
        }
      }
    }

    search?.addEventListener('input', () => {
      state.query = search.value;
      render();
    });
    for (const button of typeButtons) {
      button.addEventListener('click', () => {
        state.type = button.dataset.modelType ?? 'all';
        render();
      });
    }

    root?.classList.add('has-catalog');

    return {
      state,
      render,
      selectEntry,
      setNotice(message) {
        state.notice = String(message ?? '').trim();
        render();
      },
      setEntries(entries) {
        state.entries = applyModelOverrides(entries, options.overrides);
        render();
      },
      async load(url = catalogUrlFromLocation()) {
        setMeta('Loading catalog');
        const catalog = applyOverridesToCatalog(await fetchCatalog(url, options), options.overrides);
        state.entries = catalog.entries;
        render();
        return catalog;
      },
    };
  }

  async function restoreShareLinkSelection(controller) {
    const shareLink = globalScope.AzurLaneShareLink;
    const shell = controller?.state ? globalScope.azurLaneViewerShell : null;
    if (!shareLink || !shell) {
      return null;
    }

    const decoded = shareLink.decodeShareUrl(globalScope.location?.href);
    if (!decoded.ok) {
      controller.setNotice(`Share link ignored: ${decoded.error.message}`);
      return null;
    }
    if (!decoded.payload) {
      return null;
    }

    const { model, transform } = decoded.payload;
    const entry = controller.state.entries.find((candidate) => candidate.id === model.id && candidate.type === model.type);
    if (!entry) {
      controller.setNotice('Share link model is not available in this catalog');
      return null;
    }

    try {
      await controller.selectEntry(entry);
      const restoredTransform = shell.applyActiveTransform?.(transform, { persist: false });
      if (!restoredTransform) {
        controller.setNotice('Share link model loaded without its transform');
        return null;
      }
      controller.setNotice('Share link restored');
      return { entry, payload: decoded.payload };
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      controller.setNotice(`Share link load failed: ${message}`);
      return null;
    }
  }

  async function bootCatalogController() {
    const shell = globalScope.azurLaneViewerShell;
    if (!shell?.ready) {
      globalScope.setTimeout(bootCatalogController, 25);
      return;
    }

    const controller = createCatalogController({ shell });
    globalScope.azurLaneModelCatalogController = controller;
    try {
      await controller.load();
      await restoreShareLinkSelection(controller);
    } catch (error) {
      controller.state.error = error;
      const message = error instanceof Error ? error.message : String(error);
      const meta = document.querySelector('#catalog-meta');
      if (meta) {
        meta.textContent = `Catalog unavailable: ${message}`;
      }
    }
  }

  const api = Object.freeze({
    DEFAULT_CATALOG_URL,
    filterCatalogEntries,
    applyModelOverride,
    applyModelOverrides,
    debugModeFromLocation,
    normalizeCatalogPayload,
    normalizeL2DSuCatalog,
    normalizeModelOverrides,
    applyOverridesToCatalog,
    normalizeSearchText,
    createCatalogController,
    fetchCatalog,
    restoreShareLinkSelection,
  });

  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
    return;
  }

  globalScope.AzurLaneModelCatalog = api;
  document.addEventListener('DOMContentLoaded', bootCatalogController);
})(typeof globalThis !== 'undefined' ? globalThis : window);
