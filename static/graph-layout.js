// Deterministic force-directed layout in a worker. No remote libraries, no vectors.
// Springs use real graph edges. Barnes-Hut repulsion avoids quadratic pair scans
// and artificial cell-boundary patterns, without shipping a third-party bundle.
self.onmessage = ({data}) => {
  const {nodes, edges} = data;
  let seed = 87231;
  const random = () => {seed = (Math.imul(seed,1664525)+1013904223)>>>0; return seed/4294967296;};
  const n = nodes.length, size = Math.max(300, Math.sqrt(n)*30);
  const points = nodes.map(() => {const a=random()*Math.PI*2,r=Math.sqrt(random())*size*.42;return {x:Math.cos(a)*r,y:Math.sin(a)*r,vx:0,vy:0};});
  const cell=(x,y,w)=>({x,y,w,m:0,sx:0,sy:0,children:null,items:[]});
  function insert(q,i,depth=0){const p=points[i];q.m++;q.sx+=p.x;q.sy+=p.y;
    if(!q.children&&(q.items.length===0||depth>20)){q.items.push(i);return;}
    if(!q.children){const w=q.w/2;q.children=[cell(q.x,q.y,w),cell(q.x+w,q.y,w),cell(q.x,q.y+w,w),cell(q.x+w,q.y+w,w)];
      for(const j of q.items){const a=points[j];insert(q.children[(a.x>=q.x+w?1:0)+(a.y>=q.y+w?2:0)],j,depth+1);}q.items=[];}
    insert(q.children[(p.x>=q.x+q.w/2?1:0)+(p.y>=q.y+q.w/2?2:0)],i,depth+1);
  }
  function repel(q,p,i,alpha){if(!q.m)return;const dx=p.x-q.sx/q.m,dy=p.y-q.sy/q.m,d2=dx*dx+dy*dy+.1;
    if(!q.children){for(const j of q.items){if(j===i)continue;const a=points[j],x=p.x-a.x,y=p.y-a.y,f=36*alpha/Math.max(16,x*x+y*y);p.vx+=x*f;p.vy+=y*f;}return;}
    if(q.w*q.w/d2<.7){const f=36*alpha*q.m/d2;p.vx+=dx*f;p.vy+=dy*f;}
    else for(const child of q.children)repel(child,p,i,alpha);
  }
  for(let step=0;step<480;step++) {
    const alpha = .8*(1-step/520);
    let loX=Infinity,loY=Infinity,hiX=-Infinity,hiY=-Infinity;
    for(const p of points){loX=Math.min(loX,p.x);loY=Math.min(loY,p.y);hiX=Math.max(hiX,p.x);hiY=Math.max(hiY,p.y);}
    const tree=cell(loX-1,loY-1,Math.max(hiX-loX,hiY-loY,1)+2);
    for(let i=0;i<n;i++)insert(tree,i);
    for(let i=0;i<n;i++) {
      const p=points[i];repel(tree,p,i,alpha);
      p.vx-=p.x*.0008*alpha;p.vy-=p.y*.0008*alpha;
    }
    for(const e of edges) {const p=points[e.source],q=points[e.target],dx=q.x-p.x,dy=q.y-p.y,d=Math.hypot(dx,dy)||1;
      const desired=22+(1-e.similarity)*90,f=(d-desired)/d*.022*alpha;
      p.vx+=dx*f;p.vy+=dy*f;q.vx-=dx*f;q.vy-=dy*f;}
    for(const p of points) {p.vx*=.7;p.vy*=.7;p.x+=Math.max(-7,Math.min(7,p.vx));p.y+=Math.max(-7,Math.min(7,p.vy));}
    if(step===0 || step%80===79) self.postMessage({points:points.map(p=>[p.x,p.y]),done:step===479});
  }
};
