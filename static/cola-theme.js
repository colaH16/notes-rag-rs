(()=>{'use strict';
/* The theme button lives in the shared <cola-header>. This keeps auto mode following the system
   when the provider script is absent, and tells the graph canvas when the theme changes. */
const root=document.documentElement,media=matchMedia('(prefers-color-scheme:dark)');
const apply=()=>{root.dataset.colaTheme=root.dataset.colaPreference==='auto'?(media.matches?'dark':'light'):root.dataset.colaPreference;const meta=document.querySelector('meta[name="theme-color"]');if(meta)meta.content=root.dataset.colaTheme==='dark'?'#191719':'#F4F0F1'};
media.addEventListener('change',()=>{if(root.dataset.colaPreference==='auto')apply()});

new MutationObserver(()=>dispatchEvent(new Event('cola-theme-change'))).observe(root,{attributes:true,attributeFilter:['data-cola-theme']});
})();
