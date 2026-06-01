// Idempotent installer: removes any prior fragment, then installs the
// elements the SUTs query at module-parse time.
// Phase 9 adds #retry and #cancel (hidden by default) for the dead-session UI.
// hw-encoder-backend-disclosure adds #encoder-backend (hidden by default).
// REQ-B2: adds #dead-modal, #dead-reason, #receiver-retry, #receiver-cancel
//         for the receiver dead-modal UI (mse-client.js:145-180).
// receiver-retry-on-exhaustion (D-RRE-6): adds #dead-role-change inside
//         .dead-buttons for the role-change affordance (backward-compatible).
// staged-silent-reconnect: adds #reconnecting-overlay (hidden by default) for
//         the 3-stage timer-gate overlay (REQ-SSR-3, REQ-SSR-6).
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
    <div id="reconnecting-overlay" hidden></div>
    <div id="dead-modal" hidden>
      <p id="dead-reason"></p>
      <div class="dead-buttons">
        <button id="receiver-retry">Retry</button>
        <a id="dead-role-change" href="./sender.html"
           role="button" aria-label="Switch to sender mode and leave receiver">
          Be the sender instead
        </a>
        <button id="receiver-cancel">Cancel</button>
      </div>
    </div>
  `;
}

export function removeDom() {
  document.body.innerHTML = '';
}
