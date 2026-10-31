'use strict';
(() => {
const $ = id => document.getElementById(id), canvas=$('map'), ctx=canvas.getContext('2d');
const palette=['#83e4bb','#79b9f3','#d1a3ee','#efb881','#f08f9e','#91cbb7','#accd78','#8e9fef','#e4cf89','#7dd4dc','#c9aac0','#b7c3cc','#d7ac85','#a7c7e7','#d0dca4','#efaec9','#b5a4ed'];
const token=name=>getComputedStyle(document.documentElement).getPropertyValue('--cola-'+name).trim();
function recolor(){colors=new Map(groups.map((g,i)=>[g,document.documentElement.dataset.colaTheme==='light'?['#7B4C68','#366F94','#875A9B','#956027','#A44255','#317665','#617537','#5263A0','#86702C','#287A82','#8A577A','#687683','#8B5F3B','#436A91','#617140','#A64E77','#74629D'][i%17]:palette[i%palette.length]]));renderLegend();dirty=true;}
addEventListener('cola-theme-change',()=>{if(graph)recolor();else dirty=true;});
let graph=null, points=[], shown=new Set(), groups=[], colors=new Map(), adjacency=[], selected=null, hovered=null;
let notebook='', query='', owner=false, ready=false, worker=null, epoch=0, loading=false, dirty=true;
let width=1,height=1,dpr=1,scale=1,panX=0,panY=0,autoFit=true,frame=0;
let sessionBusy=false;
const el=(tag,text,className)=>{const x=document.createElement(tag);if(text!==undefined)x.textContent=text;if(className)x.className=className;return x;};
const format=n=>n.toLocaleString('ko-KR');
const degree=id=>adjacency[id]?.length||0;
const title=n=>n.title||n.path.split('/').at(-1).replace(/\.md$/,'');
function resetView() {
  epoch++;worker?.terminate();worker=null;graph=null;points=[];adjacency=[];shown.clear();groups=[];colors.clear();selected=hovered=null;ready=false;
  $('legend').replaceChildren();$('matches').replaceChildren();$('neighbors').replaceChildren();
  $('node-count').textContent=$('edge-count').textContent=$('dimension').textContent='—';
  $('detail-title').textContent='어디서 시작할까요?';$('detail-path').textContent='점을 누르면 관련 문서를 보여드립니다.';
  $('detail-meta').textContent=$('neighbor-count').textContent='';$('document-actions').hidden=true;$('clear').hidden=true;
  $('read').removeAttribute('href');$('edit').removeAttribute('href');$('updated').textContent='색인 기준 확인 중';
  $('map-scope').textContent='전체 지도';$('layout-status').textContent='연결을 불러오는 중';dirty=true;
}
async function load() {
  resetView();const generation=epoch;loading=true;$('retry').hidden=true;$('message').hidden=false;
  $('message').textContent='저장된 벡터로 지도를 준비합니다. 처음에는 잠시 걸릴 수 있어요.';
  try {
    const response=await fetch('/api/graph',{cache:'no-store'});
    if(!response.ok)throw Error('graph');const data=await response.json();
    if(generation!==epoch)return;
    if(data.audience!==(owner?'owner':'public')){await refreshSession(true);return;}
    graph=data;ready=true;notebook='';query='';$('find').value='';
    groups=[...new Set(data.nodes.map(n=>n.notebook))].sort((a,b)=>a.localeCompare(b,'ko'));
    recolor();
    adjacency=data.nodes.map(()=>[]);
    for(const e of data.edges){adjacency[e.source].push({id:e.target,similarity:e.similarity});adjacency[e.target].push({id:e.source,similarity:e.similarity});}
    adjacency.forEach(list=>list.sort((a,b)=>b.similarity-a.similarity));
    $('dimension').textContent=format(data.dimensions);
    $('updated').textContent='색인 '+new Date(data.index.updated_at).toLocaleString('ko-KR')+' · '+(owner?'본인 전체 범위':'공개 문서만');
    $('message').hidden=data.nodes.length>0;if(!data.nodes.length)$('message').textContent='현재 권한으로 표시할 문서가 없습니다.';
    renderLegend();filter();select(null);autoFit=true;
    worker=new Worker('/graph-layout.js');
    worker.onmessage=({data:layout})=>{if(generation!==epoch)return;if(matchMedia('(prefers-reduced-motion:reduce)').matches&&!layout.done)return;points=layout.points;if(autoFit)fit();dirty=true;$('layout-status').textContent=layout.done?'의미 기반 연결 · 배치 완료':'문서 연결을 배치하는 중';};
    worker.onerror=()=>{if(generation!==epoch)return;$('layout-status').textContent='배치 실패 · 목록으로 탐색 가능';};
    worker.postMessage({nodes:data.nodes.map(n=>({id:n.id})),edges:data.edges});
  } catch(error) {if(generation===epoch){$('message').textContent='지도를 불러오지 못했습니다. 잠시 후 다시 시도해 주세요.';$('retry').hidden=false;$('layout-status').textContent='연결 대기';}}
  finally{if(generation===epoch)loading=false;}
}
async function refreshSession(force=false) {
  if(sessionBusy)return;sessionBusy=true;
  try {const response=await fetch('/api/session',{cache:'no-store'});if(!response.ok)throw Error('session');const state=await response.json();
    const changed=owner!==state.owner;owner=state.owner;
    $('audience').textContent=owner?'OWNER LIBRARY':'PUBLIC LIBRARY';$('login').hidden=!state.login_available||owner;$('logout').hidden=!owner;
    if(changed||force||(!ready&&!loading)){sessionBusy=false;await load();}
  } catch(error){resetView();loading=false;$('message').hidden=false;$('message').textContent='접근 권한을 확인하지 못했습니다. 다시 시도해 주세요.';$('retry').hidden=false;}
  finally{sessionBusy=false;}
}
function renderLegend(){
  $('legend').replaceChildren();
  for(const g of groups){const count=graph.nodes.filter(n=>n.notebook===g).length,b=el('button',undefined,'legend-row');b.setAttribute('aria-pressed',String(g===notebook));
    const sw=el('span',undefined,'swatch');sw.style.backgroundColor=colors.get(g);
    b.append(sw,el('span',g,'legend-name'),el('span',format(count),'legend-count'));
    b.addEventListener('click',()=>{notebook=notebook===g?'':g;selected=null;filter();select(null);renderLegend();fit();});$('legend').append(b);}
}
function filter(){
  if(!graph)return;shown=new Set(graph.nodes.filter(n=>(!notebook||n.notebook===notebook)&&(!query||(title(n)+' '+n.path).toLocaleLowerCase().includes(query))).map(n=>n.id));
  $('node-count').textContent=format(shown.size);$('edge-count').textContent=format(graph.edges.filter(e=>shown.has(e.source)&&shown.has(e.target)).length);
  $('map-scope').textContent=(notebook||'전체 지도')+(query?' · 제목 필터':'');
  $('matches').replaceChildren();$('list-title').textContent=query?'제목 검색 결과':'연결이 많은 문서';$('match-count').textContent=format(shown.size);
  // Start with meaningful current titles, not empty separator/untitled hubs.
  const rank=id=>{const n=graph.nodes[id],t=title(n);return degree(id)-(n.history?10000:0)-(!/[\p{L}\p{N}]/u.test(t)||/^untitled$/i.test(t)?20000:0);};
  const sorted=[...shown].sort((a,b)=>rank(b)-rank(a)||a-b);
  for(const id of sorted.slice(0,query?50:8)){$('matches').append(noteButton(id,null));}
  if(!shown.size)$('matches').append(el('p','일치하는 문서가 없습니다.','empty-hint'));
  if(sorted.length>(query?50:8))$('matches').append(el('p',query?'상위 50개 표시 · 검색어를 좁혀 보세요.':'노드 또는 제목 검색으로 더 탐색하세요.','micro'));
  dirty=true;
}
function noteButton(id,similarity){const n=graph.nodes[id],b=el('button',title(n),'note-row'+(id===selected?' selected':''));b.title=n.path;
  b.append(el('small',similarity===null?n.notebook+' · '+degree(id)+'개 연결':'cosine '+similarity.toFixed(3)+' · '+n.notebook));
  b.addEventListener('click',()=>{if(!shown.has(id)){query='';notebook='';$('find').value='';filter();renderLegend();}select(id);focusNode(id);});return b;}
function select(id){selected=id;hovered=null;$('neighbors').replaceChildren();$('document-actions').hidden=id===null;$('clear').hidden=id===null;
  if(id===null){$('detail-title').textContent='어디서 시작할까요?';$('detail-path').textContent='점을 누르면 그 노트와 의미가 가까운 문서들이 드러납니다.';$('detail-meta').textContent=$('neighbor-count').textContent='';$('neighbors').append(el('p','선은 실제 임베딩 유사도입니다. 노트북 경계를 넘어 관련 기록을 찾아보세요.','empty-hint'));}
  else {const n=graph.nodes[id];$('detail-title').textContent=title(n);$('detail-path').textContent=n.path;$('detail-meta').textContent=format(n.chunks)+'개 색인 조각 · '+(n.history?'과거 기록':'현행 문서');
    $('read').href='/read?path='+encodeURIComponent(n.path);$('edit').href='https://notes.cola16.app/OneNote/markdown/'+n.path.replace(/\.md$/,'').split('/').map(encodeURIComponent).join('/');
    $('neighbor-count').textContent=format(degree(id));for(const other of adjacency[id].slice(0,12))$('neighbors').append(noteButton(other.id,other.similarity));
    if(!degree(id))$('neighbors').append(el('p','기준 유사도 이상의 연결이 없습니다.','empty-hint'));
    if(degree(id)>12)$('neighbors').append(el('p','유사도가 높은 12개 표시','micro'));
  }document.querySelectorAll('#matches .note-row').forEach(b=>{const active=b.title===graph?.nodes[id]?.path;b.classList.toggle('selected',active);b.setAttribute('aria-pressed',String(active));});dirty=true;
}
function fit(){if(!points.length||!shown.size)return;const ps=[...shown].map(id=>points[id]).filter(Boolean);if(!ps.length)return;
  let minX=Infinity,maxX=-Infinity,minY=Infinity,maxY=-Infinity;for(const [x,y]of ps){minX=Math.min(minX,x);maxX=Math.max(maxX,x);minY=Math.min(minY,y);maxY=Math.max(maxY,y);}
  scale=Math.min((width-90)/Math.max(100,maxX-minX),(height-130)/Math.max(100,maxY-minY));scale=Math.max(.05,Math.min(4,scale));panX=width/2-(minX+maxX)/2*scale;panY=height/2-(minY+maxY)/2*scale;dirty=true;}
function focusNode(id){if(!points[id])return;autoFit=false;
  const ps=[id,...adjacency[id].slice(0,12).map(e=>e.id)].filter(i=>shown.has(i)).map(i=>points[i]);
  const xs=ps.map(p=>p[0]),ys=ps.map(p=>p[1]),loX=Math.min(...xs),hiX=Math.max(...xs),loY=Math.min(...ys),hiY=Math.max(...ys);
  scale=Math.min(2,(width-100)/Math.max(120,hiX-loX),(height-150)/Math.max(120,hiY-loY));
  panX=width/2-(loX+hiX)/2*scale;panY=height/2-(loY+hiY)/2*scale;dirty=true;}
const position=id=>[points[id][0]*scale+panX,points[id][1]*scale+panY];
function draw(){
  ctx.setTransform(dpr,0,0,dpr,0,0);ctx.clearRect(0,0,width,height);
  ctx.fillStyle=token('bg');ctx.fillRect(0,0,width,height);
  const glow=ctx.createRadialGradient(width*.5,height*.45,0,width*.5,height*.45,Math.max(width,height)*.65);glow.addColorStop(0,token('surface'));glow.addColorStop(1,token('bg'));ctx.fillStyle=glow;ctx.fillRect(0,0,width,height);
  ctx.fillStyle=token('divider');for(let x=20;x<width;x+=32)for(let y=20;y<height;y+=32){ctx.globalAlpha=.4;ctx.fillRect(x,y,1,1);}ctx.globalAlpha=1;
  if(!graph||!points.length)return;
  const active=selected??hovered, neighborhood=active===null?null:new Set([active,...adjacency[active].map(e=>e.id)]);
  ctx.lineWidth=.65;ctx.strokeStyle=token('muted');ctx.globalAlpha=neighborhood?.07:.16;ctx.beginPath();
  for(const e of graph.edges){if(!shown.has(e.source)||!shown.has(e.target))continue;const a=position(e.source),b=position(e.target);ctx.moveTo(...a);ctx.lineTo(...b);}ctx.stroke();
  if(active!==null&&shown.has(active)){ctx.globalAlpha=.75;ctx.lineWidth=1.3;const a=position(active);for(const e of adjacency[active]){if(!shown.has(e.id))continue;ctx.strokeStyle=colors.get(graph.nodes[e.id].notebook);ctx.beginPath();ctx.moveTo(...a);ctx.lineTo(...position(e.id));ctx.stroke();}}
  for(const id of shown){const [x,y]=position(id);if(x<-20||y<-20||x>width+20||y>height+20)continue;const radius=Math.max(1.7,Math.min(5.5,1.5+Math.sqrt(degree(id))*.45))*Math.min(1.5,Math.max(.8,scale));
    ctx.globalAlpha=!neighborhood||neighborhood.has(id)?.95:.14;ctx.fillStyle=colors.get(graph.nodes[id].notebook);ctx.beginPath();ctx.arc(x,y,radius,0,Math.PI*2);ctx.fill();
    if(id===active){ctx.globalAlpha=1;ctx.lineWidth=1.5;ctx.strokeStyle=token('text');ctx.beginPath();ctx.arc(x,y,radius+5,0,Math.PI*2);ctx.stroke();ctx.globalAlpha=.1;ctx.fillStyle=token('accent');ctx.beginPath();ctx.arc(x,y,22,0,Math.PI*2);ctx.fill();}}
  ctx.globalAlpha=1;
  const candidates=active!==null?[active,...adjacency[active].slice(0,6).map(e=>e.id)]:[...shown].sort((a,b)=>degree(b)-degree(a)).slice(0,20);
  const boxes=[];ctx.font='11px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif';
  for(const id of candidates){if(!shown.has(id))continue;const [x,y]=position(id),label=title(graph.nodes[id]).slice(0,32),w=ctx.measureText(label).width+12,box=[x+9,y-8,w,21];
    if(x<5||y<58||x+w+12>width||y>height-65||boxes.some(b=>box[0]<b[0]+b[2]&&box[0]+w>b[0]&&box[1]<b[1]+b[3]&&box[1]+21>b[1]))continue;
    boxes.push(box);ctx.fillStyle=token('surface');ctx.fillRect(...box);ctx.fillStyle=id===active?token('text'):token('muted');ctx.fillText(label,x+15,y+6);}
}
function loop(){if(dirty){draw();dirty=false;}frame=requestAnimationFrame(loop);}loop();
function resize(){const rect=canvas.getBoundingClientRect();width=rect.width;height=rect.height;dpr=Math.min(2,devicePixelRatio||1);canvas.width=Math.round(width*dpr);canvas.height=Math.round(height*dpr);if(autoFit)fit();dirty=true;}new ResizeObserver(resize).observe(canvas);
function hit(x,y){if(!graph||!points.length)return null;let best=null,distance=13;for(const id of shown){const p=position(id),d=Math.hypot(p[0]-x,p[1]-y);if(d<distance){best=id;distance=d;}}return best;}
function zoom(mult,x=width/2,y=height/2){autoFit=false;const next=Math.min(12,Math.max(.05,scale*mult));panX=x-(x-panX)*next/scale;panY=y-(y-panY)*next/scale;scale=next;dirty=true;}
const pointers=new Map();let moved=false,lastDistance=0;
function local(event){const r=canvas.getBoundingClientRect();return [event.clientX-r.left,event.clientY-r.top];}
canvas.addEventListener('pointerdown',e=>{const [x,y]=local(e);pointers.set(e.pointerId,{x,y});canvas.setPointerCapture(e.pointerId);moved=false;lastDistance=0;autoFit=false;});
canvas.addEventListener('pointermove',e=>{const [x,y]=local(e),old=pointers.get(e.pointerId);if(old){pointers.set(e.pointerId,{x,y});if(pointers.size===2){moved=true;const [a,b]=[...pointers.values()],d=Math.hypot(a.x-b.x,a.y-b.y);if(lastDistance)zoom(d/lastDistance,(a.x+b.x)/2,(a.y+b.y)/2);lastDistance=d;}else{if(Math.hypot(x-old.x,y-old.y)>1)moved=true;panX+=x-old.x;panY+=y-old.y;dirty=true;}}else{const next=hit(x,y);if(next!==hovered){hovered=next;canvas.style.cursor=next===null?'grab':'pointer';dirty=true;}}});
canvas.addEventListener('pointerup',e=>{const [x,y]=local(e);pointers.delete(e.pointerId);lastDistance=0;if(!moved){select(hit(x,y));}if(canvas.hasPointerCapture(e.pointerId))canvas.releasePointerCapture(e.pointerId);});
canvas.addEventListener('pointercancel',e=>{pointers.delete(e.pointerId);moved=true;lastDistance=0;});
canvas.addEventListener('pointerleave',()=>{hovered=null;dirty=true;});
canvas.addEventListener('wheel',e=>{e.preventDefault();const [x,y]=local(e);zoom(Math.exp(-e.deltaY*.0015),x,y);},{passive:false});
canvas.addEventListener('keydown',e=>{if(e.key==='Escape')select(null);else if(e.key==='+'||e.key==='=')zoom(1.25);else if(e.key==='-')zoom(.8);else if(e.key==='Home'){fit();e.preventDefault();}});
$('zoom-in').onclick=()=>zoom(1.3);$('zoom-out').onclick=()=>zoom(1/1.3);$('fit').onclick=()=>{autoFit=true;fit();};$('clear').onclick=()=>select(null);
$('find').addEventListener('input',()=>{query=$('find').value.trim().toLocaleLowerCase();select(null);filter();});
$('all').onclick=()=>{notebook='';query='';$('find').value='';select(null);renderLegend();filter();autoFit=true;fit();};
$('retry').onclick=()=>refreshSession(true);
$('export').onclick=()=>{if(!graph||!points.length)return;if(owner&&!confirm('비공개 노트 제목이 포함될 수 있습니다. 본인용 PNG를 저장할까요?'))return;
  draw();const out=document.createElement('canvas');out.width=canvas.width;out.height=canvas.height+100*dpr;const c=out.getContext('2d');c.fillStyle=token('bg');c.fillRect(0,0,out.width,out.height);c.drawImage(canvas,0,0);c.scale(dpr,dpr);
  c.fillStyle=token('accent');c.font='20px sans-serif';c.fillText('cola16 / 기록 지도',20,height+32);c.fillStyle=token('muted');c.font='11px sans-serif';c.fillText(`${owner?'OWNER / PRIVATE':'PUBLIC'} · ${shown.size} documents · ${$('edge-count').textContent} links · ${graph.dimensions}D embeddings`,20,height+55);
  c.fillText('Document cosine similarity · Force-directed layout · rag.cola16.app/graph',20,height+76);
  out.toBlob(blob=>{if(!blob)return;const url=URL.createObjectURL(blob),a=el('a');a.href=url;a.download='notes-atlas-'+(owner?'private':'public')+'.png';a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);},'image/png');};
$('logout').onclick=async()=>{resetView();loading=false;try{const r=await fetch('/auth/logout',{method:'POST'});if(!r.ok)throw Error('logout');location.replace('/graph');}catch(e){$('message').hidden=false;$('message').textContent='로그아웃에 실패했습니다. 다시 시도해 주세요.';}};
window.addEventListener('pageshow',()=>refreshSession());
window.addEventListener('pagehide',()=>{if(owner){resetView();loading=false;}});
document.addEventListener('visibilitychange',()=>{if(document.visibilityState==='hidden'&&owner){resetView();loading=false;}if(document.visibilityState==='visible')refreshSession();});
setInterval(()=>{if(document.visibilityState==='visible')refreshSession();},60000);
if(matchMedia('(max-width:760px)').matches)$('explorer-details').open=false;
})();
