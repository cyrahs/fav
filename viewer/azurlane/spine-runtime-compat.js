(function attachAzurLaneSpineRuntimeCompat(globalScope) {
  'use strict';

  function patchSpineGraphics(PIXI) {
    const SpineBase = PIXI?.spine?.SpineBase;
    if (!SpineBase || SpineBase.prototype.azurLanePixi8GraphicsPatched) {
      return false;
    }

    SpineBase.prototype.updateGraphics = function updateGraphics(slot, clip) {
      const points = SpineBase.clippingPolygon || (SpineBase.clippingPolygon = []);
      const length = clip.worldVerticesLength;

      points.length = length;
      clip.computeWorldVertices(slot, 0, length, points, 0, 2);
      slot.currentGraphics.clear();
      slot.currentGraphics.poly(points);
      slot.currentGraphics.fill({ color: 0xffffff, alpha: 1 });
      slot.currentGraphics.renderable = false;
    };
    SpineBase.prototype.azurLanePixi8GraphicsPatched = true;
    return true;
  }

  function patchSpineMeshRender(PIXI) {
    const prototype = PIXI?.spine?.SpineMesh?.prototype;
    if (!prototype || prototype.azurLanePixi8MeshRenderPatched) {
      return false;
    }

    prototype._render = function renderSpineMesh() {
      const positionBuffer = this.geometry?.getBuffer?.('aPosition');
      if (this.autoUpdate && positionBuffer) {
        positionBuffer.update();
      }
    };
    prototype.azurLanePixi8MeshRenderPatched = true;
    return true;
  }

  function patchBlendModePipe(app) {
    const pipe = app?.renderer?.renderPipes?.blendMode;
    const prototype = pipe?.constructor?.prototype;
    if (!prototype || prototype.azurLaneSpineBlendModePatched || typeof prototype.popBlendMode !== 'function') {
      return false;
    }

    const originalPopBlendMode = prototype.popBlendMode;
    prototype.popBlendMode = function popBlendMode(instructionSet) {
      if (typeof this._activeBlendMode !== 'string') {
        this._activeBlendMode = 'normal';
      }
      if (!Array.isArray(this._blendModeStack)) {
        this._blendModeStack = [];
      }

      return originalPopBlendMode.call(this, instructionSet);
    };
    prototype.azurLaneSpineBlendModePatched = true;
    return true;
  }

  function patchRuntime(PIXI = globalScope.PIXI) {
    return {
      graphics: patchSpineGraphics(PIXI),
      meshRender: patchSpineMeshRender(PIXI),
    };
  }

  function patchApplication(app) {
    return {
      blendMode: patchBlendModePipe(app),
    };
  }

  const api = Object.freeze({
    patchApplication,
    patchRuntime,
  });

  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
    return;
  }

  globalScope.AzurLaneSpineRuntimeCompat = api;
  patchRuntime();
})(typeof globalThis !== 'undefined' ? globalThis : window);
