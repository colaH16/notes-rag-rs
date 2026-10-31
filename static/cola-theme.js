(()=>{'use strict';
const root=document.documentElement,media=matchMedia('(prefers-color-scheme:dark)');
const apply=()=>{root.dataset.colaTheme=root.dataset.colaPreference==='auto'?(media.matches?'dark':'light'):root.dataset.colaPreference;const meta=document.querySelector('meta[name="theme-color"]');if(meta)meta.content=root.dataset.colaTheme==='dark'?'#201B20':'#F4F0F1'};
document.querySelectorAll('[data-cola-theme-select]').forEach(select=>{select.hidden=false;select.value=root.dataset.colaPreference||'auto';select.addEventListener('change',()=>{root.dataset.colaPreference=select.value;apply();const status=document.querySelector('#cola-theme-status');try{localStorage.setItem('cola-theme',select.value);if(status)status.textContent=''}catch{if(status)status.textContent='테마는 이 화면에 적용했습니다. 브라우저에 선택을 저장하지 못했습니다.'}})});
media.addEventListener('change',()=>{if(root.dataset.colaPreference==='auto')apply()});apply();

new MutationObserver(()=>dispatchEvent(new Event('cola-theme-change'))).observe(root,{attributes:true,attributeFilter:['data-cola-theme']});
})();
