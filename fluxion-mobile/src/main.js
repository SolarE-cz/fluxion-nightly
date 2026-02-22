// FluxION Mobile — Tauri IPC Bridge
//
// This file bridges the Tauri Rust backend with the WebView.
// It handles:
// 1. PIN lock screen (if configured)
// 2. Loading the cached UI bundle into the DOM
// 3. Injecting data via window.updateState()
// 4. Sending control changes back via Tauri commands
// 5. Managing the loading/updating/offline screens

const { invoke } = window.__TAURI__.core;

const statusText = document.getElementById("status-text");
const loadingScreen = document.getElementById("loading-screen");
const pinScreen = document.getElementById("pin-screen");
const pinInput = document.getElementById("pin-input");
const pinError = document.getElementById("pin-error");
const pinSubmit = document.getElementById("pin-submit");
const appDiv = document.getElementById("app");

// State
let uiLoaded = false;
let refreshTimer = null;
let backgroundedAt = null;

/**
 * App entry point — called on page load.
 */
async function init() {
  // 1. Check if PIN is set — show lock screen if needed
  const pinSet = await invoke("is_pin_set");
  if (pinSet) {
    showPinScreen();
    return;
  }

  // No PIN — proceed directly
  await startApp();
}

/**
 * Show the PIN entry screen.
 */
function showPinScreen() {
  loadingScreen.style.display = "none";
  pinScreen.style.display = "flex";
  pinInput.value = "";
  pinError.textContent = "";
  pinInput.focus();
}

/**
 * Handle PIN submission.
 */
async function handlePinSubmit() {
  const pin = pinInput.value;
  if (!pin) return;

  const valid = await invoke("verify_pin", { pin });
  if (valid) {
    pinScreen.style.display = "none";
    loadingScreen.style.display = "flex";
    await startApp();
  } else {
    pinError.textContent = "Incorrect PIN";
    pinInput.value = "";
    pinInput.focus();
  }
}

// PIN submit button and Enter key
if (pinSubmit) {
  pinSubmit.addEventListener("click", handlePinSubmit);
}
if (pinInput) {
  pinInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") handlePinSubmit();
  });
}

/**
 * Main app startup — runs after PIN verification (or if no PIN).
 */
async function startApp() {
  statusText.textContent = "Checking connection...";

  // 1. Check if we have a stored connection
  const info = await invoke("get_connection_info");

  if (!info.connected && !info.instance_name) {
    statusText.textContent = "No connection configured";
    // TODO: Show QR scanner prompt
    return;
  }

  statusText.textContent = "Loading cached data...";

  // 2. Try to load cached UI bundle
  const cachedUi = await invoke("get_cached_ui");

  if (cachedUi) {
    // Inject cached UI into the app div
    loadUiBundle(cachedUi);
  }

  // 3. Check for UI bundle updates
  statusText.textContent = "Checking for updates...";
  try {
    const update = await invoke("check_ui_update");
    if (update.updated) {
      statusText.textContent = "Updating app...";
      const freshUi = await invoke("get_cached_ui");
      if (freshUi) loadUiBundle(freshUi);
    }
  } catch (e) {
    // Non-fatal — continue with cached UI
  }

  // 4. Fetch fresh state from server
  statusText.textContent = "Connecting via Tor...";
  const state = await invoke("get_state");

  if (state.data) {
    if (uiLoaded) {
      // UI already loaded from cache — inject data
      if (window.updateState) {
        window.updateState(JSON.parse(state.data));
      }
    }
    showApp();
  } else if (state.error) {
    statusText.textContent = state.error;
  }

  // 4. Start 5-minute refresh timer (foreground only)
  startRefreshTimer();
}

/**
 * Load the UI bundle HTML into the app container.
 */
function loadUiBundle(html) {
  appDiv.innerHTML = html;
  uiLoaded = true;

  // Execute any inline scripts in the bundle
  const scripts = appDiv.querySelectorAll("script");
  scripts.forEach((script) => {
    const newScript = document.createElement("script");
    newScript.textContent = script.textContent;
    script.parentNode.replaceChild(newScript, script);
  });
}

/**
 * Hide loading screen and show the app.
 */
function showApp() {
  loadingScreen.style.display = "none";
  pinScreen.style.display = "none";
  appDiv.style.display = "block";
}

/**
 * Start the 5-minute foreground refresh timer.
 */
function startRefreshTimer() {
  stopRefreshTimer();
  refreshTimer = setInterval(async () => {
    const state = await invoke("get_state");
    if (state.data && window.updateState) {
      window.updateState(JSON.parse(state.data));
    }
  }, 5 * 60 * 1000); // 5 minutes
}

/**
 * Stop the refresh timer (when app is backgrounded).
 */
function stopRefreshTimer() {
  if (refreshTimer) {
    clearInterval(refreshTimer);
    refreshTimer = null;
  }
}

/**
 * Save control changes — called by the mobile UI bundle's "Save" button.
 * This is exposed as a global function for the UI bundle to call.
 */
window.__fluxionSave = async function (controlsJson) {
  const result = await invoke("save_controls", {
    controlsJson: JSON.stringify(controlsJson),
  });

  if (result.ok && result.updated_state && window.updateState) {
    window.updateState(JSON.parse(result.updated_state));
  }

  return result;
};

// Handle app visibility changes (pause/resume timer, PIN lock on return)
document.addEventListener("visibilitychange", async () => {
  if (document.hidden) {
    stopRefreshTimer();
    backgroundedAt = Date.now();
  } else {
    // Check if PIN lock should re-engage after lock timeout
    const pinSet = await invoke("is_pin_set");
    if (pinSet && backgroundedAt) {
      const elapsed = (Date.now() - backgroundedAt) / 1000;
      // Re-lock after 5 minutes in background (matches AppSettings.lock_timeout_secs default)
      if (elapsed >= 300) {
        appDiv.style.display = "none";
        showPinScreen();
        backgroundedAt = null;
        return;
      }
    }
    backgroundedAt = null;

    // Fetch immediately on foreground, then resume timer
    invoke("get_state").then((state) => {
      if (state.data && window.updateState) {
        window.updateState(JSON.parse(state.data));
      }
    });
    startRefreshTimer();
  }
});

// Start the app
init();
