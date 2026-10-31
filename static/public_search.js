"use strict";
const form = document.querySelector("#search-form");
const query = document.querySelector("#query");
const status = document.querySelector("#status");
const results = document.querySelector("#results");
const button = document.querySelector("#search-button");
let running = false;
const folderColumns = document.querySelector("#folder-columns");
const folderUp = document.querySelector("#folder-up");
const folderAll = document.querySelector("#folder-all");
const folderReload = document.querySelector("#folder-reload");
let selectedFolder = "";
let folders = [];
let foldersReady = false;
let foldersLoading = false;
let foldersReloadPending = false;
let ownerMode = false;
let viewGeneration = 0;
let sessionCheck = null;
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
    if (ownerMode !== session.owner) {
      viewGeneration++; results.replaceChildren(); folders = []; foldersReady = false; selectedFolder = '';
      showAccess(session.owner); refreshFolderControls();
    }
    showAccess(session.owner);
    document.querySelector('#login').hidden = session.owner || !session.login_available;
    // Initial navigation, OIDC return and BFCache restore all load after session resolution.
    if (!foldersReady) await loadFolders();
  } catch (_) {
    // Do not retain private excerpts on screen after a failed session check.
    if (ownerMode) {viewGeneration++; results.replaceChildren(); folders = []; foldersReady = false; refreshFolderControls();}
  }
}
document.querySelector('#logout').addEventListener('click', async () => {
  viewGeneration++; results.replaceChildren(); folders = []; foldersReady = false; selectedFolder = ''; refreshFolderControls();
  const response = await fetch('/auth/logout', {method: 'POST'});
  if (response.ok) location.replace('/');
  else status.textContent = '로그아웃에 실패했습니다. 다시 시도해 주세요.';
});
window.addEventListener('pageshow', () => checkSession());
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'hidden' && ownerMode) {
    viewGeneration++; results.replaceChildren(); folders = []; foldersReady = false; refreshFolderControls();
  }
  if (document.visibilityState === 'visible') checkSession();
});
setInterval(checkSession, 60000);
const folderScroll = new Map();
function element(tag, text, className) {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (className) el.className = className;
  return el;
}
function refreshFolderControls() {
  const focused = folderColumns.contains(document.activeElement) ? document.activeElement.dataset.folder : undefined;
  // Hidden ancestor columns on mobile report scrollTop=0; do not erase their saved position.
  folderColumns.querySelectorAll(".folder-list").forEach(list => {
    if (list.getClientRects().length) folderScroll.set(list.dataset.parent, list.scrollTop);
  });
  const parents = [""];
  if (selectedFolder) selectedFolder.split("/").forEach((part, index, parts) => parents.push(parts.slice(0, index + 1).join("/")));
  const visible = parents.slice(-3);
  const hasChildren = new Set(folders.map(item => item.parent));
  folderColumns.replaceChildren();
  folderColumns.setAttribute("aria-busy", String(foldersLoading));
  visible.forEach(parent => {
    const current = parent === selectedFolder;
    const column = element("section", undefined, "folder-column" + (current ? " folder-column-current" : ""));
    column.setAttribute("aria-label", parent || "노트북");
    const heading = element("h3", parent ? parent.split("/").at(-1) : "노트북", "folder-column-title");
    heading.title = parent || "전체 노트북"; heading.tabIndex = -1;
    const list = element("ul", undefined, "folder-list"); list.dataset.parent = parent;
    const children = folders.filter(item => item.parent === parent);
    children.forEach(item => {
      const row = element("button", undefined, "folder-row"); row.type = "button";
      row.dataset.folder = item.path; row.title = item.path;
      const selected = selectedFolder === item.path || selectedFolder.startsWith(item.path + "/");
      row.setAttribute("aria-pressed", String(selected));
      if (item.path === selectedFolder) row.setAttribute("aria-current", "true");
      row.disabled = running || foldersLoading;
      const icon = element("span", undefined, "folder-icon"); icon.setAttribute("aria-hidden", "true");
      const count = element("span", item.pages.toLocaleString(), "folder-count");
      count.setAttribute("aria-label", `${item.pages.toLocaleString()}개 문서`);
      const arrow = element("span", hasChildren.has(item.path) ? "›" : "", "folder-arrow"); arrow.setAttribute("aria-hidden", "true");
      row.append(icon, element("span", item.name, "folder-name"), count, arrow);
      row.addEventListener("click", event => chooseFolder(item.path, event.detail === 0));
      const li = element("li"); li.append(row); list.append(li);
    });
    if (!children.length) list.append(element("li", !foldersReady ? (foldersLoading ? "폴더 불러오는 중…" : "폴더 목록을 새로고침해 주세요.") : "하위 폴더가 없습니다. 이 폴더에서 검색할 수 있어요.", "folder-empty"));
    column.append(heading, list); folderColumns.append(column);
    list.scrollTop = folderScroll.get(parent) || 0;
  });
  for (let i = visible.length; i < 3; i++) {
    const column = element("div", "폴더를 선택하면 하위 폴더가 여기에 표시됩니다.", "folder-column folder-placeholder");
    column.setAttribute("aria-hidden", "true"); folderColumns.append(column);
  }
  if (focused !== undefined) Array.from(folderColumns.querySelectorAll(".folder-row")).find(row => row.dataset.folder === focused)?.focus({preventScroll: true});
  folderUp.disabled = folderAll.disabled = running || foldersLoading || !selectedFolder;
  folderReload.disabled = running || foldersLoading;
  document.querySelector("#folder-current").textContent = selectedFolder ? `선택 범위: 노트 ${selectedFolder} · 하위 폴더 포함` : "선택 범위: 전체 자료 · 노트와 공개 블로그";
}
function chooseFolder(path, keyboard = false) {
  if (running || foldersLoading || path === selectedFolder) return;
  selectedFolder = path;
  results.replaceChildren(); status.className = "";
  status.textContent = "선택한 범위에서 검색하려면 검색 버튼을 눌러 주세요.";
  refreshFolderControls();
  if (keyboard) {
    const column = folderColumns.querySelector(".folder-column-current");
    (column.querySelector(".folder-row") || column.querySelector("h3")).focus({preventScroll: true});
  }
}
async function loadFolders() {
  const generation = viewGeneration;
  // Do not lose a permission-change refresh behind an older in-flight request.
  if (foldersLoading || running) {foldersReloadPending = true; return;}
  foldersReloadPending = false;
  foldersLoading = true; refreshFolderControls();
  const hint = document.querySelector("#folder-status");
  hint.textContent = "폴더 불러오는 중…";
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 15000);
  try {
    const response = await fetch("/api/folders", {signal: controller.signal, cache: 'no-store'});
    const body = await response.json();
    if (generation !== viewGeneration) return;
    if (!response.ok) throw new Error(body.error || "폴더 목록 오류");
    if (body.audience) showAccess(body.audience === 'owner');
    folders = body.folders; foldersReady = true;
    if (selectedFolder && !folders.some(item => item.path === selectedFolder)) {
      selectedFolder = ""; results.replaceChildren();
      status.textContent = "이전 폴더가 색인에 없어 전체 범위로 돌아왔습니다.";
    }
    hint.textContent = "폴더를 한 번 눌러 선택하세요. 선택한 폴더 아래를 모두 검색합니다.";
  } catch (error) {
    if (generation === viewGeneration) hint.textContent = "폴더 목록을 불러오지 못했습니다. 전체 검색은 그대로 사용할 수 있어요.";
  } finally {
    clearTimeout(timeout); foldersLoading = false; refreshFolderControls();
    if (foldersReloadPending && !running && !document.hidden) void loadFolders();
  }
}
folderColumns.addEventListener("keydown", event => {
  if (running || foldersLoading) return;
  const row = event.target.closest(".folder-row");
  if (event.key === "ArrowLeft" && selectedFolder) {event.preventDefault(); folderUp.click(); return;}
  if (!row) return;
  const rows = Array.from(row.closest(".folder-list").querySelectorAll(".folder-row"));
  const index = rows.indexOf(row);
  let next;
  if (event.key === "ArrowDown") next = rows[Math.min(rows.length - 1, index + 1)];
  if (event.key === "ArrowUp") next = rows[Math.max(0, index - 1)];
  if (event.key === "Home") next = rows[0];
  if (event.key === "End") next = rows.at(-1);
  if (next) {event.preventDefault(); next.focus();}
  if (event.key === "ArrowRight") {event.preventDefault(); chooseFolder(row.dataset.folder, true);}
});
folderUp.addEventListener("click", event => chooseFolder(selectedFolder.includes("/") ? selectedFolder.slice(0, selectedFolder.lastIndexOf("/")) : "", event.detail === 0));
folderAll.addEventListener("click", event => chooseFolder("", event.detail === 0));
folderReload.addEventListener("click", loadFolders);
checkSession();
form.addEventListener("submit", async event => {
  event.preventDefault();
  if (running || !query.value.trim()) return;
  running = true; button.disabled = true; button.textContent = "검색 중…";
  refreshFolderControls();
  const searchFolder = selectedFolder;
  const generation = viewGeneration;
  results.replaceChildren(); status.className = "loading";
  status.textContent = "노트의 의미를 비교하고 있습니다…";
  document.querySelector('.search-again').hidden = false;
  // Keep the desktop refinement loop at the query. Only phones need to dismiss
  // the keyboard and bring the below-fold results into view.
  const compactSearch = matchMedia('(max-width:760px)').matches;
  if (compactSearch) {
    status.tabIndex = -1; status.focus({preventScroll: true});
    status.scrollIntoView({block: 'start', behavior: 'instant'});
  }
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 55000);
  try {
    const response = await fetch("/api/search", {method: "POST", headers: {"Content-Type": "application/json"},
      body: JSON.stringify({query: query.value.trim(), include_history: document.querySelector("#history").checked, folder: searchFolder}), signal: controller.signal});
    const body = await response.json();
    if (generation !== viewGeneration) return;
    if (!response.ok) throw new Error(body.error || "검색에 실패했습니다.");
    if (body.audience) showAccess(body.audience === 'owner');
    status.className = "";
    status.textContent = body.results.length ? `${body.results.length}개의 관련 노트 · ${body.scoped_pages.toLocaleString()}개 문서에서 검색${body.cached ? " · 캐시 결과" : ""}` : "선택한 범위에서 일치하는 노트를 찾지 못했습니다. 범위를 넓히거나 다른 표현으로 검색해 보세요.";
    if (body.blog_unavailable) status.textContent += ' · 블로그 연결을 확인하지 못해 블로그 결과는 제외했습니다.';
    body.results.forEach((item, index) => {
      const card = element("article", undefined, "result-card");
      const head = element("div", undefined, "result-heading");
      head.append(element("span", String(index + 1).padStart(2, "0"), "number"));
      const title = element("h2");
      const link = element("a", item.title || "제목 없는 노트");
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
    results.replaceChildren(); status.className = "error";
    status.textContent = error.name === "AbortError" ? "검색이 지연되고 있습니다. 잠시 후 다시 시도해 주세요." : error.message;
  } finally {
    clearTimeout(timeout); running = false; button.disabled = false; button.textContent = "검색 ↗";
    refreshFolderControls();
    // Rendering adds page height. Scroll now so results really reach the viewport;
    // leave the user alone if they have already moved focus elsewhere.
    if (compactSearch && document.activeElement === status) {
      status.scrollIntoView({block: 'start', behavior: 'instant'});
    }
    if (foldersReloadPending && !document.hidden) void loadFolders();
  }
});
document.querySelectorAll("[data-query]").forEach(el => el.addEventListener("click", () => {query.value = el.dataset.query; form.requestSubmit();}));

// Do not summon an on-screen keyboard on touch devices or steal an existing focus.
if (matchMedia('(hover:hover) and (pointer:fine)').matches &&
    !location.hash && document.activeElement === document.body) {
  query.focus({preventScroll: true});
}
