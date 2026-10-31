"use strict";
const form = document.querySelector("#search-form");
const query = document.querySelector("#query");
const status = document.querySelector("#status");
const results = document.querySelector("#results");
const button = document.querySelector("#search-button");
const defaultQuery = query.dataset.defaultQuery;
const folderColumns = document.querySelector("#folder-columns");
const folderUp = document.querySelector("#folder-up");
const folderAll = document.querySelector("#folder-all");
const folderReload = document.querySelector("#folder-reload");
let selectedSource = 'all';
let selectedFolder = '';
let selectedCategory = null;
let folders = [];
let blogCategories = [];
let blogPosts = 0;
let foldersReady = false;
let foldersLoading = false;
let foldersBackground = false;
let foldersAttempted = false;
let foldersReloadPending = false;
let foldersReloadBackground = true;
let ownerMode = false;
let viewGeneration = 0;
let catalogGeneration = 0;
let searchSequence = 0;
let activeSearch = null;
let running = false;
let sessionCheck = null;
const folderScroll = new Map();
function element(tag, text, className) {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (className) el.className = className;
  return el;
}
function invalidateSearch(message) {
  viewGeneration++; searchSequence++;
  activeSearch?.abort(); activeSearch = null;
  running = false; button.disabled = false; button.textContent = '검색 ↗';
  results.replaceChildren(); status.className = '';
  status.textContent = message || '선택한 범위에서 검색하려면 검색 버튼을 눌러 주세요.';
  document.querySelector('.search-again').hidden = true;
}
function resetCatalog() {
  catalogGeneration++; folders = []; blogCategories = []; blogPosts = 0;
  foldersReady = false; foldersAttempted = false; selectedFolder = ''; selectedCategory = null;
  invalidateSearch();
}
function showAccess(owner) {
  ownerMode = owner;
  document.querySelector('#access-badge').textContent = owner ? 'OWNER · 비공개 포함' : 'PUBLIC LIBRARY';
  document.querySelector('#logout').hidden = !owner;
}
function checkSession() {
  if (!sessionCheck) sessionCheck = updateSession().finally(() => {sessionCheck = null;});
  return sessionCheck;
}
async function updateSession() {
  try {
    const response = await fetch('/api/session', {cache: 'no-store'});
    if (!response.ok) throw new Error('session');
    const session = await response.json();
    if (ownerMode !== session.owner) {resetCatalog(); showAccess(session.owner); refreshFolderControls();}
    showAccess(session.owner);
    document.querySelector('#login').hidden = session.owner || !session.login_available;
    if (!foldersReady) await loadFolders();
  } catch (_) {
    if (ownerMode) {resetCatalog(); refreshFolderControls();}
  }
}
document.querySelector('#logout').addEventListener('click', async () => {
  resetCatalog(); refreshFolderControls();
  const response = await fetch('/auth/logout', {method: 'POST'});
  if (response.ok) location.replace('/');
  else status.textContent = '로그아웃에 실패했습니다. 다시 시도해 주세요.';
});
async function refreshVisibleCatalog() {
  if (document.hidden) return;
  const hadCatalog = foldersReady; const generation = catalogGeneration;
  await checkSession();
  // Initial/session-change loads already refresh the catalog themselves.
  if (!document.hidden && hadCatalog && generation === catalogGeneration && foldersReady) await loadFolders(true);
}
window.addEventListener('pageshow', () => refreshVisibleCatalog());
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'hidden' && ownerMode) {resetCatalog(); refreshFolderControls();}
  if (document.visibilityState === 'visible') void refreshVisibleCatalog();
});
setInterval(refreshVisibleCatalog, 60000);
function categoryTrail(id) {
  const trail = []; const seen = new Set();
  while (id !== null && !seen.has(id)) {
    seen.add(id);
    const item = blogCategories.find(c => c.id === id);
    if (!item) break;
    trail.unshift(item); id = item.parent;
  }
  return trail;
}
function chooseSource(source, keyboard = false) {
  if (!['all','wordpress','notes'].includes(source)) return;
  if (selectedSource !== source) {
    selectedSource = source; selectedFolder = ''; selectedCategory = null;
    invalidateSearch(); refreshFolderControls();
  }
  if (keyboard) folderColumns.querySelector(`[data-source="${source}"]`)?.focus();
  flushCatalog();
}
function chooseFolder(path, keyboard = false) {
  if (running || catalogBlocking() || path === selectedFolder) return;
  selectedFolder = path; invalidateSearch(); refreshFolderControls();
  if (keyboard) focusCurrentColumn();
}
function chooseCategory(id, keyboard = false) {
  if (running || catalogBlocking() || id === selectedCategory) return;
  selectedCategory = id; invalidateSearch(); refreshFolderControls();
  if (keyboard) focusCurrentColumn();
}
function focusCurrentColumn() {
  const current = folderColumns.querySelector('.folder-column-current');
  (current?.querySelector('.folder-row') || current?.querySelector('h3'))?.focus({preventScroll:true});
}
function addColumn(title, children, current, scrollKey, makeRow, empty) {
  const column = element('section',undefined,'folder-column'+(current?' folder-column-current':''));
  column.setAttribute('aria-label',title);
  const heading = element('h3',title,'folder-column-title');heading.title = title;heading.tabIndex = -1;
  const list = element('ul',undefined,'folder-list');list.dataset.scrollKey=scrollKey;
  children.forEach(item => {const li=element('li');li.append(makeRow(item));list.append(li);});
  if (!children.length) list.append(element('li',foldersReady?empty:(foldersLoading?'목록 불러오는 중…':foldersAttempted?'목록을 새로고침해 주세요.':'자료 목록을 준비하고 있습니다.'),'folder-empty'));
  column.append(heading,list);folderColumns.append(column);list.scrollTop=folderScroll.get(scrollKey)||0;
}
function rowParts(row, name, count, hasChildren, blog) {
  row.type='button'; row.className='folder-row'; row.disabled=running||catalogBlocking();
  const icon=element('span',blog?'#':undefined,blog?'category-icon':'folder-icon');icon.setAttribute('aria-hidden','true');
  const amount=element('span',count.toLocaleString(),'folder-count');amount.setAttribute('aria-label',`${count.toLocaleString()}${blog?'편':'개 문서'}`);
  const arrow=element('span',hasChildren?'›':'','folder-arrow');arrow.setAttribute('aria-hidden','true');
  row.append(icon,element('span',name,'folder-name'),amount,arrow);
}
function noteRow(item) {
  const row=element('button');row.dataset.folder=item.path;row.title=item.path;rowParts(row,item.name,item.pages,folders.some(f=>f.parent===item.path),false);
  row.setAttribute('aria-pressed',String(selectedFolder===item.path||selectedFolder.startsWith(item.path+'/')));if(item.path===selectedFolder)row.setAttribute('aria-current','true');
  row.addEventListener('click',event=>chooseFolder(item.path,event.detail===0));return row;
}
function categoryRow(item) {
  const row=element('button');row.dataset.category=String(item.id);row.title=categoryTrail(item.id).map(c=>c.name).join(' › ');
  rowParts(row,item.name,item.posts,blogCategories.some(c=>c.parent===item.id),true);
  row.setAttribute('aria-pressed',String(categoryTrail(selectedCategory).some(c=>c.id===item.id)));if(item.id===selectedCategory)row.setAttribute('aria-current','true');
  row.addEventListener('click',event=>chooseCategory(item.id,event.detail===0));return row;
}
function addSourceRoot(depth) {
  const root=element('section',undefined,'folder-column source-root'+(!depth?' folder-column-current':' source-root-with-descendants'));
  root.setAttribute('aria-label','검색할 자료');
  const notes=folders.filter(f=>f.parent==='').reduce((sum,f)=>sum+f.pages,0);
  [['wordpress','블로그(기록)',blogPosts,'편'],['notes','노트북',notes,'개']].forEach(([source,title,total,unit])=>{
    const expanded=selectedSource===source;
    const heading=element('button',undefined,'source-heading');heading.type='button';heading.dataset.source=source;
    heading.setAttribute('aria-expanded',String(expanded));heading.setAttribute('aria-controls',source+'-root-list');
    const arrow=element('span',undefined,'source-chevron');arrow.setAttribute('aria-hidden','true');
    heading.append(arrow,element('span',title,'source-title'),element('span',foldersReady?total.toLocaleString()+unit:'','source-count'));
    heading.addEventListener('click',event=>chooseSource(selectedSource===source?'all':source,event.detail===0));
    const list=element('ul',undefined,'folder-list source-root-list');list.id=source+'-root-list';list.dataset.scrollKey=source+':root';list.hidden=!expanded;
    const children=source==='wordpress'?blogCategories.filter(c=>c.parent===null):folders.filter(f=>f.parent==='');
    children.forEach(item=>{const li=element('li');li.append(source==='wordpress'?categoryRow(item):noteRow(item));list.append(li);});
    if(!children.length)list.append(element('li',foldersReady?(source==='wordpress'?'공개 블로그 카테고리가 없습니다. 블로그 전체를 검색할 수 있어요.':'검색할 수 있는 노트북이 없습니다.'):(foldersLoading?'목록 불러오는 중…':foldersAttempted?'목록을 새로고침해 주세요.':'자료 목록을 준비하고 있습니다.'),'folder-empty'));
    root.append(heading,list);list.scrollTop=folderScroll.get(list.dataset.scrollKey)||0;
  });
  folderColumns.append(root);
  root.querySelectorAll('.folder-list').forEach(list=>list.scrollTop=folderScroll.get(list.dataset.scrollKey)||0);
}
function refreshFolderControls() {
  const active = folderColumns.contains(document.activeElement) ? document.activeElement : null;
  const focusedFolder=active?.dataset.folder;const focusedCategory=active?.dataset.category;const focusedSource=active?.dataset.source;
  folderColumns.querySelectorAll('.folder-list').forEach(list => {if(list.getClientRects().length)folderScroll.set(list.dataset.scrollKey,list.scrollTop);});
  folderColumns.replaceChildren();folderColumns.setAttribute('aria-busy',String(catalogBlocking()));
  const blog=selectedSource==='wordpress';
  const trail=blog?categoryTrail(selectedCategory):[];
  const noteParents=[];if(selectedSource==='notes'&&selectedFolder)selectedFolder.split('/').forEach((part,index,parts)=>noteParents.push(parts.slice(0,index+1).join('/')));
  const depth=blog?trail.length:noteParents.length;
  document.querySelector('#scope-heading').textContent='검색할 자료';
  addSourceRoot(depth);
  if(blog)trail.slice(-2).forEach(item=>addColumn(item.name,blogCategories.filter(c=>c.parent===item.id),item.id===selectedCategory,'blog:'+item.id,categoryRow,'하위 카테고리가 없습니다. 이 분류에서 검색할 수 있어요.'));
  if(selectedSource==='notes')noteParents.slice(-2).forEach(parent=>addColumn(parent.split('/').at(-1),folders.filter(f=>f.parent===parent),parent===selectedFolder,'note:'+parent,noteRow,'하위 폴더가 없습니다. 이 폴더에서 검색할 수 있어요.'));
  for(let i=Math.min(depth+1,3);i<3;i++) {
    const message=selectedSource==='all'?'블로그나 노트북을 펼쳐 검색 범위를 좁힐 수 있어요.':`${blog?'카테고리':'폴더'}를 선택하면 하위 ${blog?'분류':'폴더'}가 여기에 표시됩니다.`;
    const placeholder=element('div',message,'folder-column folder-placeholder');placeholder.setAttribute('aria-hidden','true');folderColumns.append(placeholder);
  }
  if(focusedFolder!==undefined)(Array.from(folderColumns.querySelectorAll('[data-folder]')).find(row=>row.dataset.folder===focusedFolder&&row.getClientRects().length)||folderColumns.querySelector('[data-source=notes]'))?.focus({preventScroll:true});
  if(focusedCategory!==undefined)(Array.from(folderColumns.querySelectorAll('[data-category]')).find(row=>row.dataset.category===focusedCategory&&row.getClientRects().length)||folderColumns.querySelector('[data-source=wordpress]'))?.focus({preventScroll:true});
  if(focusedSource!==undefined)folderColumns.querySelector(`[data-source="${focusedSource}"]`)?.focus({preventScroll:true});
  folderUp.disabled=running||catalogBlocking()||!depth;
  folderAll.disabled=running||selectedSource==='all';folderAll.textContent='전체';folderReload.disabled=running||catalogBlocking();
  document.querySelector('#folder-current').textContent=selectedSource==='all'?'선택 범위: 전체 자료 · 블로그와 노트':blog?(selectedCategory===null?'선택 범위: 블로그(기록) 전체':`선택 범위: 블로그(기록) › ${trail.map(c=>c.name).join(' › ')} · 하위 카테고리 포함`):(selectedFolder?`선택 범위: 노트북 › ${selectedFolder} · 하위 폴더 포함`:'선택 범위: 모든 노트북');
  document.querySelector('#folder-status').textContent=catalogBlocking()?'목록 불러오는 중…':!foldersReady?(foldersAttempted?'목록을 불러오지 못했습니다. 전체 범위 검색은 사용할 수 있어요.':'자료 목록을 준비하고 있습니다.'):selectedSource==='all'?'블로그나 노트북을 눌러 펼치세요. 전체 검색은 두 자료를 함께 찾습니다.':blog?'카테고리를 누르면 해당 분류와 하위 분류를 함께 검색합니다.':'폴더를 누르면 해당 폴더 아래를 모두 검색합니다.';
}
function catalogBlocking() {return foldersLoading && !foldersBackground;}
function queueCatalog(background) {
  foldersReloadBackground=foldersReloadPending?foldersReloadBackground&&background:background;
  foldersReloadPending=true;
}
function flushCatalog() {
  if(foldersReloadPending&&!running&&!foldersLoading&&!document.hidden) {
    const background=foldersReloadBackground;foldersReloadPending=false;void loadFolders(background);
  }
}
async function loadFolders(background=false) {
  const generation=catalogGeneration;
  if(foldersLoading||running){queueCatalog(background);return;}
  foldersReloadPending=false;foldersReloadBackground=true;foldersLoading=true;foldersBackground=background&&foldersReady;
  const quiet=foldersBackground;let render=!quiet;foldersAttempted=true;
  if(!quiet)refreshFolderControls();
  const controller=new AbortController();const timeout=setTimeout(()=>controller.abort(),15000);
  try {
    const response=await fetch('/api/folders',{signal:controller.signal,cache:'no-store'});const body=await response.json();
    if(generation!==catalogGeneration){queueCatalog(false);return;}
    if(!response.ok||!Array.isArray(body.folders))throw new Error('catalog');
    if(body.audience)showAccess(body.audience==='owner');
    const nextFolders=body.folders;const nextPosts=Number.isSafeInteger(body.blog_posts)?body.blog_posts:0;const nextCategories=Array.isArray(body.blog_categories)?body.blog_categories:[];
    const changed=!foldersReady||JSON.stringify([folders,blogPosts,blogCategories])!==JSON.stringify([nextFolders,nextPosts,nextCategories]);
    if(changed){folders=nextFolders;blogPosts=nextPosts;blogCategories=nextCategories;render=true;}
    foldersReady=true;
    if(selectedFolder&&!folders.some(f=>f.path===selectedFolder)){selectedFolder='';invalidateSearch('이전 폴더가 색인에 없어 모든 노트북으로 돌아왔습니다.');render=true;}
    if(selectedCategory!==null&&!blogCategories.some(c=>c.id===selectedCategory)){selectedCategory=null;invalidateSearch('이전 카테고리가 없어 블로그 전체 범위로 돌아왔습니다.');render=true;}
  } catch (_) {
    // A background outage retains the last usable catalog without interrupting
    // keyboard navigation or announcing an unsolicited loading error.
    if(generation===catalogGeneration&&!quiet){foldersReady=false;render=true;}
  } finally {
    clearTimeout(timeout);foldersLoading=false;foldersBackground=false;
    if(render)refreshFolderControls();flushCatalog();
  }
}
folderColumns.addEventListener('keydown',event=>{
  if(running||catalogBlocking())return;const row=event.target.closest('.folder-row');
  if(event.key==='ArrowLeft'&&(selectedFolder||selectedCategory!==null)){event.preventDefault();folderUp.click();return;}
  if(!row)return;const rows=Array.from(row.closest('.folder-list').querySelectorAll('.folder-row'));const index=rows.indexOf(row);let next;
  if(event.key==='ArrowDown')next=rows[Math.min(rows.length-1,index+1)];if(event.key==='ArrowUp')next=rows[Math.max(0,index-1)];if(event.key==='Home')next=rows[0];if(event.key==='End')next=rows.at(-1);
  if(next){event.preventDefault();next.focus();}
  if(event.key==='ArrowRight'){event.preventDefault();if(selectedSource==='wordpress')chooseCategory(Number(row.dataset.category),true);else chooseFolder(row.dataset.folder,true);}
});
folderUp.addEventListener('click',event=>{if(selectedSource==='wordpress'){const trail=categoryTrail(selectedCategory);chooseCategory(trail.length>1?trail.at(-2).id:null,event.detail===0);}else chooseFolder(selectedFolder.includes('/')?selectedFolder.slice(0,selectedFolder.lastIndexOf('/')):'',event.detail===0);});
folderAll.addEventListener('click',()=>chooseSource('all'));
folderReload.addEventListener('click',()=>loadFolders(false));
refreshFolderControls();checkSession();
form.addEventListener("submit", async event => {
  event.preventDefault();
  if (running) return;
  if (!query.value.trim()) query.value = defaultQuery;
  const requestSequence = ++searchSequence;
  running = true; button.disabled = true; button.textContent = "검색 중…";
  refreshFolderControls();
  const searchFolder = selectedSource === 'notes' ? selectedFolder : '';
  const searchSource = selectedSource;
  const searchCategory = selectedSource === 'wordpress' ? selectedCategory : null;
  const generation = viewGeneration;
  results.replaceChildren(); status.className = "loading";
  status.textContent = "자료의 의미를 비교하고 있습니다…";
  document.querySelector('.search-again').hidden = false;
  // Keep the desktop refinement loop at the query. Only phones need to dismiss
  // the keyboard and bring the below-fold results into view.
  const compactSearch = matchMedia('(max-width:760px)').matches;
  if (compactSearch) {
    status.tabIndex = -1; status.focus({preventScroll: true});
    status.scrollIntoView({block: 'start', behavior: 'instant'});
  }
  const controller = new AbortController();
  activeSearch = controller;
  const timeout = setTimeout(() => controller.abort(), 55000);
  try {
    const response = await fetch("/api/search", {method: "POST", headers: {"Content-Type": "application/json"},
      body: JSON.stringify({query: query.value.trim(), include_history: document.querySelector("#history").checked, folder: searchFolder, source: searchSource, blog_category: searchCategory}), signal: controller.signal});
    const body = await response.json();
    if (generation !== viewGeneration || requestSequence !== searchSequence) return;
    if (!response.ok) throw new Error(body.error || "검색에 실패했습니다.");
    if (body.audience) showAccess(body.audience === 'owner');
    status.className = "";
    status.textContent = body.results.length ? `${body.results.length}개의 관련 자료 · ${body.scoped_pages.toLocaleString()}개 문서에서 검색${body.cached ? " · 캐시 결과" : ""}` : "선택한 범위에서 일치하는 자료를 찾지 못했습니다. 범위를 넓히거나 다른 표현으로 검색해 보세요.";
    if (body.blog_unavailable) status.textContent += ' · 블로그 연결을 확인하지 못해 블로그 결과는 제외했습니다.';
    body.results.forEach((item, index) => {
      const card = element("article", undefined, "result-card");
      const head = element("div", undefined, "result-heading");
      head.append(element("span", String(index + 1).padStart(2, "0"), "number"));
      const title = element("h2");
      const link = element("a", item.title || "제목 없는 자료");
      // Defense in depth: text/URLs are never inserted as HTML.
      const url = new URL(item.url);
      const blog = item.source_kind === 'wordpress';
      let readUrl;
      if (blog) {
        if (url.origin !== 'https://cola16.app' || url.username || url.password || url.search || url.hash || !Number.isSafeInteger(item.post_id) || item.post_id < 1) throw new Error('잘못된 블로그 출처입니다.');
        readUrl = '/read-blog?' + new URLSearchParams({id: String(item.post_id)});
      } else {
        if (url.origin !== "https://notes.cola16.app" || !url.pathname.startsWith("/OneNote/markdown/")) throw new Error("잘못된 출처 주소입니다.");
        const pagePath = decodeURIComponent(url.pathname.slice('/OneNote/markdown/'.length));
        readUrl = '/read?' + new URLSearchParams({path: pagePath.endsWith('.md') ? pagePath : pagePath + '.md'});
      }
      link.href = readUrl; link.target = "_blank"; link.rel = "noopener noreferrer";
      title.append(link); head.append(title); card.append(head);
      const tags = element("div", undefined, "result-tags");
      tags.append(element('span', blog ? 'WordPress 블로그' : '노트'));
      if (blog && Array.isArray(item.category_path) && item.category_path.length) tags.append(element('span',item.category_path.map(c=>c.name).join(' › ')));
      if (blog && typeof item.published === 'string') tags.append(element('span', '작성 ' + item.published.slice(0,10)));
      tags.append(element("span", item.history ? "과거 기록 · 현재 방법과 다를 수 있음" : "현재 자료", item.history ? "old" : "current"));
      if (item.visual_review) tags.append(element("span", "원본 배치·필기 확인 권장"));
      card.append(tags);
      const excerpt = element("pre", item.excerpt, "excerpt"); card.append(excerpt);
      const actions = element("div", undefined, "result-actions");
      const expand = element("button", "발췌 펼치기"); expand.type = "button"; expand.setAttribute("aria-expanded", "false");
      expand.addEventListener("click", () => {const open = excerpt.classList.toggle("expanded"); expand.textContent = open ? "접기" : "발췌 펼치기"; expand.setAttribute("aria-expanded", String(open));});
      actions.append(expand);
      const read = element("a", blog ? "미리보기 ↗" : "원문 보기 ↗"); read.href = readUrl; read.target = "_blank"; read.rel = "noopener noreferrer"; actions.append(read);
      const source = element("a", blog ? "WordPress에서 전문 보기 ↗" : "SilverBullet에서 편집 ↗"); source.href = url.href; source.target = "_blank"; source.rel = "noopener noreferrer"; actions.append(source);
      card.append(actions);
      if (item.truncated) card.append(element("p", "긴 문서의 일부입니다. 전체 내용은 원문에서 확인하세요.", "truncated"));
      results.append(card);
    });
  } catch (error) {
    if (generation !== viewGeneration || requestSequence !== searchSequence) return;
    results.replaceChildren(); status.className = "error";
    status.textContent = error.name === "AbortError" ? "검색이 지연되고 있습니다. 잠시 후 다시 시도해 주세요." : error.message;
  } finally {
    clearTimeout(timeout);
    if (requestSequence !== searchSequence) return;
    activeSearch = null; running = false; button.disabled = false; button.textContent = "검색 ↗";
    refreshFolderControls();
    // Rendering adds page height. Scroll now so results really reach the viewport;
    // leave the user alone if they have already moved focus elsewhere.
    if (compactSearch && document.activeElement === status) {
      status.scrollIntoView({block: 'start', behavior: 'instant'});
    }
    flushCatalog();
  }
});
document.querySelectorAll("[data-query]").forEach(el => el.addEventListener("click", () => {
  if (el.dataset.querySource) {
    chooseSource(el.dataset.querySource);
    if (selectedSource === 'wordpress' && selectedCategory !== null) {selectedCategory = null; invalidateSearch(); refreshFolderControls();}
  }
  query.value = el.dataset.query; form.requestSubmit();
}));

// Do not summon an on-screen keyboard on touch devices or steal an existing focus.
if (matchMedia('(hover:hover) and (pointer:fine)').matches &&
    !location.hash && document.activeElement === document.body) {
  query.focus({preventScroll: true});
}
