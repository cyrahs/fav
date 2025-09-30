// ==UserScript==
// @name         Tangxin Helper
// @namespace    http://tampermonkey.net/
// @version      0.1
// @description  Combine favorite list export and m3u8 catcher functionality
// @author       You
// @include      http*://*.txh*.com/*
// @grant        GM_xmlhttpRequest
// @grant        unsafeWindow
// @run-at       document-start
// ==/UserScript==

(function () {
    'use strict';
    const apiKey = 'vgTJe8Oh5eoaV4eHFp-JTVzkDLXx12keDEpmcARo';
    const accountId = '8a4b8e8f2514c0cff6847adffb351661';
    const databaseId = '32e9ec47-279a-45cd-a4e0-c634048e96f8';

    // Check if we're on the favorite page
    if (window.location.pathname === '/mine/favorite') {
        // Add button to page after load
        window.addEventListener('load', function () {
            addButtons();
        });
    }
    // Check if we're on a movie detail page
    else if (window.location.pathname.match(/\/movie\/detail\/\d+/)) {
        setupM3U8Catcher();
    }

    function addButtons() {
        // Export JSON button
        const exportButton = document.createElement('button');
        exportButton.innerText = '导出组件 JSON 数据';
        exportButton.style.position = 'fixed';
        exportButton.style.top = '20px';
        exportButton.style.right = '20px';
        exportButton.style.zIndex = '9999';
        exportButton.style.padding = '15px 30px';
        exportButton.style.backgroundColor = '#fff';
        exportButton.style.color = '#333';
        exportButton.style.border = '1px solid #ddd';
        exportButton.style.borderRadius = '5px';
        exportButton.style.cursor = 'pointer';
        exportButton.style.boxShadow = '0 2px 5px rgba(0, 0, 0, 0.1)';
        exportButton.style.fontSize = '16px';

        // Auto process button
        const autoButton = document.createElement('button');
        autoButton.innerText = '自动处理';
        autoButton.style.position = 'fixed';
        autoButton.style.top = '80px';
        autoButton.style.right = '20px';
        autoButton.style.zIndex = '9999';
        autoButton.style.padding = '15px 30px';
        autoButton.style.backgroundColor = '#fff';
        autoButton.style.color = '#333';
        autoButton.style.border = '1px solid #ddd';
        autoButton.style.borderRadius = '5px';
        autoButton.style.cursor = 'pointer';
        autoButton.style.boxShadow = '0 2px 5px rgba(0, 0, 0, 0.1)';
        autoButton.style.fontSize = '16px';

        // Refresh M3U8 button
        const refreshButton = document.createElement('button');
        refreshButton.innerText = `刷新未下载的M3U8`;
        refreshButton.style.position = 'fixed';
        refreshButton.style.top = '140px';
        refreshButton.style.right = '20px';
        refreshButton.style.zIndex = '9999';
        refreshButton.style.padding = '15px 30px';
        refreshButton.style.backgroundColor = '#fff';
        refreshButton.style.color = '#333';
        refreshButton.style.border = '1px solid #ddd';
        refreshButton.style.borderRadius = '5px';
        refreshButton.style.cursor = 'pointer';
        refreshButton.style.boxShadow = '0 2px 5px rgba(0, 0, 0, 0.1)';
        refreshButton.style.fontSize = '16px';

        document.body.appendChild(exportButton);
        document.body.appendChild(autoButton);
        document.body.appendChild(refreshButton);

        exportButton.addEventListener('click', outputToJson);
        autoButton.addEventListener('click', autoProcess);
        refreshButton.addEventListener('click', () => refreshM3U8());
    }

    async function outputToJson() {
        const infos = await getAllItems();

        if (infos.length > 0) {
            const jsonOutput = JSON.stringify(infos, null, 2);
            const newWindow = window.open();
            newWindow.document.write(jsonOutput);
            newWindow.document.close();
        } else {
            alert('没有找到包含 data.id 的组件。');
        }
    }

    async function autoProcess() {
        const items = (await getNewItems()).reverse();
        if (items.length === 0) {
            notify('没有找到新视频');
            return;
        }
        notify(`找到${items.length}个新视频`);
        const num = items.length;
        const maxConcurrentWindows = 5;
        const tabs = [];


        while (true) {
            const openTabs = tabs.filter(tab => !tab.closed);
            for (const tab of openTabs) {
                if (Date.now() - tab.createTime > 10000) {
                    tab.location.reload();
                    tab.createTime = Date.now(); // Reset timer after refresh
                }
            }
            if (openTabs.length >= maxConcurrentWindows) {
                await new Promise(resolve => setTimeout(resolve, 100));
                continue
            }
            if (items.length === 0 && openTabs.length === 0) {
                break;
            }
            else if (items.length === 0) {
                await new Promise(resolve => setTimeout(resolve, 100));
                continue;
            }
            const item = items.shift();
            const url = window.location.origin + '/movie/detail/' + item.id + '?data=' + encodeURIComponent(`{"id":"${item.id}","name":"${item.title}","nickname":"${item.upper}","canvas":"short"}`);
            const tab = window.open(url, '_blank');
            queryD1(`INSERT INTO tx (id, title, upper) VALUES (${item.id}, '${item.title}', '${item.upper}');`).catch(console.error);
            tab.createTime = Date.now();
            tabs.push(tab);
            if (!tab) {
                alert('无法打开新窗口，请检查浏览器设置。');
                return;
            }
        }
    }


    async function refreshM3U8() {
        let items;
        items = await queryD1('SELECT * FROM tx WHERE downloaded = 0 ORDER BY created_at ASC;');
        const num = items.length;
        if (num === 0) {
            notify('没有找到需要处理的视频');
            return;
        }
        else {
            alert(`找到${num}个需要处理的视频`);
        }
        const maxConcurrentWindows = 5;
        const tabs = [];

        while (true) {
            const openTabs = tabs.filter(tab => !tab.closed);
            // Check if any tab has been open for more than 10 seconds
            for (const tab of openTabs) {
                if (!tab.closed && Date.now() - tab.createTime > 10000) {
                    tab.location.reload();
                    tab.createTime = Date.now(); // Reset timer after refresh
                }
            }
            if (openTabs.length >= maxConcurrentWindows) {
                await new Promise(resolve => setTimeout(resolve, 100));
                continue
            }
            if (items.length === 0 && openTabs.length === 0) {
                break;
            }
            else if (items.length === 0) {
                await new Promise(resolve => setTimeout(resolve, 100));
                continue;
            }
            const item = items.shift();
            const url = window.location.origin + '/movie/detail/' + item.id + '?data=' + encodeURIComponent(`{"id":"${item.id}","name":"${item.title}","nickname":"${item.upper}","canvas":"short"}`);
            const tab = window.open(url, '_blank');
            tab.createTime = Date.now();
            tabs.push(tab);
            if (!tab) {
                alert('无法打开新窗口，请检查浏览器设置。');
                return;
            }
        }

    }

    function notify(message) {
        const div = document.createElement('div');
        div.style.cssText = `
            position: fixed;
            top: 20px;
            left: 20px;
            padding: 10px 20px;
            background: white;
            color: black;
            border-radius: 4px;
            z-index: 9999;
            box-shadow: 0 2px 5px rgba(0,0,0,0.2);
        `;
        div.textContent = message;
        document.body.appendChild(div);
        setTimeout(() => {
            div.style.opacity = '0';
            div.style.transition = 'opacity 0.5s';
            setTimeout(() => div.remove(), 500);
        }, 5000);
    }


    async function queryD1(query) {
        console.log('Querying D1:', query);
        const res = new Promise((resolve) => GM_xmlhttpRequest({
            method: 'POST',
            url: `https://api.cloudflare.com/client/v4/accounts/${accountId}/d1/database/${databaseId}/query`,
            headers: {
                'Authorization': `Bearer ${apiKey}`,
                'Content-Type': 'application/json'
            },
            data: JSON.stringify({ sql: query }),
            onload: function (response) {
                if (response.status === 200) {
                    const res = JSON.parse(response.responseText);
                    if (!res.success) {
                        console.error('Query failed');
                        console.error(response);
                        alert('Failed to send query');
                        throw new Error('Query failed');
                    }
                    resolve(res.result[0].results);
                    console.log('Query finished')
                } else {
                    console.error('Failed to query database');
                    console.error(response);
                    throw new Error('Failed to query database');
                }
            },
            onerror: function (error) {
                console.error('Error querying database:', error);
                alert('Failed to query database');
                throw new Error('Failed to query database');
            }
        }));
        return res;
    }

    function getItems() {
        if (!unsafeWindow.$nuxt) {
            alert('$nuxt 对象未找到，请确保该页面是 Nuxt 应用。');
            return;
        }

        let infos = [];

        function traverseChildren(children) {
            children.forEach((child) => {
                if (child.data && child.data.id !== undefined && child.data.id !== null) {
                    infos.push({
                        id: parseInt(child.data.id),
                        title: child.data.name || 'Unknown Name',
                        upper: child.data.nickname || 'Unknown Nickname',
                    });
                }
                if (child.$children && child.$children.length > 0) {
                    traverseChildren(child.$children);
                }
            });
        }

        traverseChildren(unsafeWindow.$nuxt.$children);

        return infos;
    }

    async function getAllItems() {
        let infos = [];
        let ids = new Set();
        let lastChecks = 10;
        while (true) {
            const newInfos = getItems().filter(info => !ids.has(info.id));
            if (newInfos.length === 0) {
                if (lastChecks === 0) {
                    break;
                }
                await new Promise(resolve => setTimeout(resolve, 100))
                lastChecks--;
                continue;
            };
            lastChecks = 10;
            newInfos.forEach(info => ids.add(info.id));
            infos.push(...newInfos);
            window.scrollTo(0, document.body.scrollHeight);
        }

        return infos;
    }

    async function getNewItems() {
        const d1Ids = (await queryD1('SELECT id FROM tx;')).map(item => item.id);

        let found = false;
        let attempts = 0;
        let lastItemsNum = 0;
        const maxAttempts = 20;

        while (!found && attempts < maxAttempts) {
            const items = getItems();
            const ids = items.map(item => item.id);
            if (items.length > lastItemsNum) {
                attempts = 0;
                lastItemsNum = items.length;
            }
            const matchedIds = ids.filter(id => d1Ids.includes(id));
            if (matchedIds.length < 25) {
                attempts++;
                window.scrollTo(0, document.body.scrollHeight);
                await new Promise(resolve => setTimeout(resolve, 100));
                continue;
            }
            found = true;
            const newItems = items.filter(item => !d1Ids.includes(item.id));
            console.log(`找到${newItems.length}个新视频`);
            return newItems;
        }
        if (!found) {
            alert(`没有匹配到D1中的ID, 共找到${lastItemsNum}个新视频`);
            throw new Error('没有匹配到D1中的ID');
        }
    }


    function setupM3U8Catcher() {
        const originalXHR = unsafeWindow.XMLHttpRequest;

        unsafeWindow.XMLHttpRequest = function () {
            const xhr = new originalXHR();
            const originalOpen = xhr.open;
            const originalSend = xhr.send;

            xhr.open = function () {
                const [method, url] = arguments;

                if (typeof url === 'string' && (url.includes('/m3u8/link/') || url.endsWith('.m3u8'))) {
                    console.log('M3U8 Link detected (XHR):', url);

                    xhr.addEventListener('load', async function () {
                        if (xhr.status === 200) {
                            const m3u8Content = xhr.responseText;

                            try {
                                const movieId = window.location.pathname.match(/\/movie\/detail\/(\d+)/)?.[1];
                                if (!movieId) {
                                    console.error('Could not extract movie ID from URL');
                                    alert('Could not extract movie ID from URL');
                                    return;
                                }

                                GM_xmlhttpRequest({
                                    method: 'PUT',
                                    url: 'https://api.cloudflare.com/client/v4/accounts/8a4b8e8f2514c0cff6847adffb351661/storage/kv/namespaces/f1eacd84bd4042369f843abb61d772a8/values/' + movieId + '?expiration_ttl=1800',
                                    headers: {
                                        'Authorization': `Bearer ${apiKey}`,
                                        'Content-Type': 'text/plain'
                                    },
                                    data: m3u8Content,
                                    onload: function (response) {
                                        if (response.status === 200) {
                                            console.log('Successfully uploaded m3u8 content to Cloudflare KV');
                                            window.close();
                                        } else {
                                            console.error('Failed to upload to Cloudflare KV');
                                            alert('Failed to upload to Cloudflare KV');
                                        }
                                    },
                                    onerror: function (error) {
                                        console.error('Error uploading to Cloudflare KV:', error);
                                        alert('Error uploading to Cloudflare KV');
                                    }
                                });
                            } catch (error) {
                                console.error('Error uploading to Cloudflare KV:', error);
                                alert('Error uploading to Cloudflare KV');
                            }
                        }
                    });
                }

                return originalOpen.apply(this, arguments);
            };

            xhr.send = function () {
                return originalSend.apply(this, arguments);
            };

            return xhr;
        };
    }
})();


