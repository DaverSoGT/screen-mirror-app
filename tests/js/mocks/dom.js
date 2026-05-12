// Idempotent installer: removes any prior fragment, then installs the
// elements the SUTs query at module-parse time.
// Phase 9 adds #retry and #cancel (hidden by default) for the dead-session UI.
// hw-encoder-backend-disclosure adds #encoder-backend (hidden by default).
export function installDom() {
  removeDom();
  document.body.innerHTML = `
    <video id="player"></video>
    <div id="status"></div>
    <span id="encoder-backend" hidden></span>
    <button id="start">Start streaming</button>
    <div id="error"></div>
    <a id="change-mode" href="#"></a>
    <button id="retry" style="display:none">Retry</button>
    <button id="cancel" style="display:none">Cancel</button>
  `;
}

export function removeDom() {
  document.body.innerHTML = '';
}
