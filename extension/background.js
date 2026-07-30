// background.js — Wessal Bridge service worker

chrome.runtime.onInstalled.addListener(() => {
  // Set default state to Manual for safety
  chrome.storage.local.get(['autoMode'], (res) => {
    if (res.autoMode === undefined) {
      chrome.storage.local.set({ autoMode: false });
    }
  });
  console.info('[Wessal] extension installed / updated');
});