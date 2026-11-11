
(() => {
  const version = 1;
  const existing = window.__gvRuntimeCapture;
  if (existing && existing.version === version) return;

  const state = {
    version,
    patched: false,
    players: [],
    events: [],
    sequence: 0,
  };

  function nowMs() {
    return Math.round(performance.now());
  }

  function animationName(animation) {
    if (typeof animation === "string") return animation;
    if (animation && animation.name) return String(animation.name);
    return "";
  }

  function toInt(value) {
    const parsed = Number.parseInt(value, 10);
    return Number.isFinite(parsed) ? parsed : null;
  }

  function containerForPlayer(player) {
    const parent = player && player.parent;
    return parent && parent.closest ? parent.closest(".spine-player-container") : null;
  }

  function containerIndex(container) {
    if (!container) return -1;
    return [...document.querySelectorAll(".spine-player-container")].indexOf(container);
  }

  function animationDurationsMs(player) {
    const animations = player?.skeleton?.data?.animations;
    if (!Array.isArray(animations)) return {};
    const out = {};
    for (const animation of animations) {
      const name = animationName(animation);
      if (!name) continue;
      const duration = Number(animation.duration);
      if (Number.isFinite(duration) && duration >= 0) {
        out[name] = Math.round(duration * 1000);
      }
    }
    return out;
  }

  function playerSummary(player, playerIndex) {
    const container = containerForPlayer(player);
    const style = container ? getComputedStyle(container) : null;
    return {
      playerIndex,
      containerIndex: containerIndex(container),
      zIndex: style ? toInt(style.zIndex) : null,
      rawZIndex: style ? style.zIndex : "",
      atlasUrl: player?.config?.atlasUrl || "",
      skelUrl: player?.config?.skelUrl || player?.config?.binaryUrl || "",
      configuredAnimation: player?.config?.animation || "",
      currentAnimation: player?.animationState?.tracks?.[0]?.animation?.name || "",
      animationDurationsMs: animationDurationsMs(player),
    };
  }

  function record(type, payload) {
    state.events.push({
      sequence: ++state.sequence,
      timeMs: nowMs(),
      type,
      ...payload,
    });
  }

  function ensurePlayer(player) {
    let playerIndex = state.players.indexOf(player);
    if (playerIndex === -1) {
      playerIndex = state.players.length;
      state.players.push(player);
      record("player", playerSummary(player, playerIndex));
    }
    return playerIndex;
  }

  function patchSpine(spine) {
    if (!spine || state.patched) return;
    state.patched = true;

    const playerPrototype = spine.SpinePlayer && spine.SpinePlayer.prototype;
    if (playerPrototype) {
      for (const method of ["initialize", "loadSkeleton", "play", "setAnimation", "addAnimation"]) {
        const original = playerPrototype[method];
        if (typeof original !== "function" || original.__gvRuntimePatched) continue;
        playerPrototype[method] = function patchedPlayerMethod(...args) {
          const playerIndex = ensurePlayer(this);
          record("playerMethod", {
            playerIndex,
            method,
            args: args.map((arg) => animationName(arg) || String(arg ?? "")).slice(0, 8),
            ...playerSummary(this, playerIndex),
          });
          return original.apply(this, args);
        };
        playerPrototype[method].__gvRuntimePatched = true;
      }
    }

    const statePrototype = spine.AnimationState && spine.AnimationState.prototype;
    if (statePrototype) {
      for (const method of ["setAnimation", "addAnimation", "setAnimationWith", "addAnimationWith"]) {
        const original = statePrototype[method];
        if (typeof original !== "function" || original.__gvRuntimePatched) continue;
        statePrototype[method] = function patchedAnimationStateMethod(...args) {
          const playerIndex = state.players.findIndex((player) => player && player.animationState === this);
          record("animationState", {
            playerIndex,
            method,
            trackIndex: Number.isFinite(args[0]) ? args[0] : null,
            animation: animationName(args[1]),
            loop: typeof args[2] === "boolean" ? args[2] : null,
            delayMs: Number.isFinite(args[3]) ? Math.round(args[3] * 1000) : null,
          });
          return original.apply(this, args);
        };
        statePrototype[method].__gvRuntimePatched = true;
      }
    }
  }

  state.snapshot = () => {
    const events = state.events.slice();
    return state.players
      .map((player, playerIndex) => ({
        ...playerSummary(player, playerIndex),
        events: events.filter((event) => event.playerIndex === playerIndex),
      }))
      .filter((player) => player.containerIndex >= 0);
  };

  state.eventCount = () => state.sequence;

  Object.defineProperty(window, "__gvRuntimeCapture", {
    value: state,
    configurable: true,
  });

  let spineValue = window.spine_4_1_54;
  Object.defineProperty(window, "spine_4_1_54", {
    configurable: true,
    get() {
      return spineValue;
    },
    set(value) {
      spineValue = value;
      patchSpine(value);
    },
  });
  if (spineValue) patchSpine(spineValue);
})();
