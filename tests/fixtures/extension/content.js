// Marks the page so the suite can see the content script ran, and relays
// requests the page posts to the background worker. Results land on <html>
// data attributes, the one place both the page and this isolated world see.
document.documentElement.dataset.octowebExtension = 'injected';

window.addEventListener('message', (event) => {
  if (event.source !== window || !event.data || !event.data.octowebTest) return;
  const request = event.data.octowebTest;
  chrome.runtime.sendMessage(request, (reply) => {
    const result = reply || { error: chrome.runtime.lastError ? chrome.runtime.lastError.message : 'no reply' };
    document.documentElement.setAttribute('data-octoweb-result-' + request.id, JSON.stringify(result));
  });
});
