// Idempotent installer: removes any prior fragment, then installs the
// 5 elements the SUTs query at module-parse time.
export function installDom() {
  removeDom();
  document.body.innerHTML = `
    <video id="player"></video>
    <div id="status"></div>
    <button id="start">Start streaming</button>
    <div id="error"></div>
    <a id="change-mode" href="#"></a>
  `;
}

export function removeDom() {
  document.body.innerHTML = '';
}
