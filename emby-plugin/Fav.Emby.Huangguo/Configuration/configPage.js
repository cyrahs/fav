define(['baseView', 'loading', 'emby-input', 'emby-button'], function (BaseView, loading) {
    'use strict';

    var pluginId = '4b8c8826-067f-4e5a-876f-6237f6861b05';

    function View(view, params) {
        BaseView.apply(this, arguments);
        view.querySelector('form').addEventListener('submit', function (e) {
            e.preventDefault();
            loading.show();
            ApiClient.getPluginConfiguration(pluginId).then(function (config) {
                config.FavUrl = view.querySelector('#favUrl').value;
                config.Token = view.querySelector('#token').value;
                ApiClient.updatePluginConfiguration(pluginId, config).then(function (result) {
                    loading.hide();
                    Dashboard.processPluginConfigurationUpdateResult(result);
                });
            });
            return false;
        });
    }

    Object.assign(View.prototype, BaseView.prototype);

    View.prototype.onResume = function (options) {
        BaseView.prototype.onResume.apply(this, arguments);
        var view = this.view;
        loading.show();
        ApiClient.getPluginConfiguration(pluginId).then(function (config) {
            view.querySelector('#favUrl').value = config.FavUrl || '';
            view.querySelector('#token').value = config.Token || '';
            loading.hide();
        });
    };

    return View;
});
