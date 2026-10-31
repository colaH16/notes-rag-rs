/* Run before styles paint; storage failure must never hide content. */
(()=>{let theme='auto';try{theme=localStorage.getItem('cola-theme')||'auto'}catch{}if(!['auto','light','dark'].includes(theme))theme='auto';const root=document.documentElement;root.dataset.colaPreference=theme;root.dataset.colaTheme=theme==='auto'?(matchMedia('(prefers-color-scheme:dark)').matches?'dark':'light'):theme})();
