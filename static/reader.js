"use strict";
document.querySelectorAll('article pre > code').forEach(code => {
  const button = document.createElement('button');
  button.type = 'button'; button.className = 'copy-code'; button.textContent = '복사';
  button.addEventListener('click', async () => {
    try {await navigator.clipboard.writeText(code.textContent); button.textContent = '복사됨';}
    catch (_) {button.textContent = '선택해서 복사해주세요';}
    setTimeout(() => {button.textContent = '복사';}, 1800);
  });
  code.parentElement.append(button);
});
// Match the search UI's privacy behavior for restored/hidden owner pages.
if (document.body.dataset.audience === 'owner') {
  async function checkOwner() {
    if (document.hidden) return;
    try {
      const response = await fetch('/api/session', {cache: 'no-store'});
      if (!response.ok || !(await response.json()).owner) {
        document.querySelector('main').hidden = true;
        location.reload();
      }
    } catch (_) {document.querySelector('main').hidden = true;}
  }
  setInterval(checkOwner, 60000);
  let hidden = false;
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) {document.querySelector('main').hidden = true; hidden = true;}
    else if (hidden) location.reload();
  });
  window.addEventListener('pageshow', event => {if (event.persisted) location.reload();});
}
