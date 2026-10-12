// Answers the content script's requests through the extension APIs the
// browser has to implement: tabs (query/get/create/update/remove) and storage.
const handlers = {
  async hello(_request, sender) {
    const stamp = String(Date.now());
    await chrome.storage.local.set({ stamp });
    const stored = (await chrome.storage.local.get('stamp')).stamp;
    const tab = await chrome.tabs.get(sender.tab.id);
    const all = await chrome.tabs.query({});
    return { tabId: tab.id, url: tab.url, title: tab.title, tabCount: all.length, storage: stored === stamp };
  },
  async open(request) {
    const tab = await chrome.tabs.create({ url: request.url, active: false });
    return { tabId: tab.id };
  },
  async update(request) {
    await chrome.tabs.update(request.tabId, { url: request.url });
    return { ok: true };
  },
  async close(request) {
    await chrome.tabs.remove(request.tabId);
    return { ok: true };
  },
  async popup() {
    await chrome.action.openPopup();
    return { ok: true };
  },
};

chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
  const handler = handlers[request.type];
  if (!handler) return false;
  handler(request, sender).then(sendResponse, (error) => sendResponse({ error: String(error) }));
  return true;
});

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({ id: 'octoweb-test', title: 'Octoweb Test Item', contexts: ['all'] });
});

chrome.action.setBadgeText({ text: 'ok' });
