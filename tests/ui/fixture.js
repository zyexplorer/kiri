// A Kiri Dev window with four containers, two of them shared: what the
// tests start from. `sent` records what the page asks the app to do.
document.body.classList.add("dev");ui.status={dev:true};
kiri.on({type:"state",state:"running"});
kiri.on({type:"lists",containers:[
 {Id:"a1",Names:["/web"],Image:"nginx:alpine",State:"running",Status:"Up 2 hours",Created:Date.now()/1000-7200,Ports:[{PublicPort:8080,PrivatePort:80,Type:"tcp"}],Labels:{}},
 {Id:"a2",Names:["/api"],Image:"node:22",State:"running",Status:"Up 1 hour",Created:Date.now()/1000-3600,Ports:[{PublicPort:3000,PrivatePort:3000,Type:"tcp"}],Labels:{}},
 {Id:"a3",Names:["/postgres"],Image:"postgres:17",State:"running",Status:"Up 1 hour",Created:Date.now()/1000-3600,Ports:[{PublicPort:5432,PrivatePort:5432,Type:"tcp"}],Labels:{}},
 {Id:"a4",Names:["/worker"],Image:"python:3.13",State:"exited",Status:"Exited (0)",Created:Date.now()/1000-9000,Ports:[],Labels:{}}],images:[],volumes:[],networks:[]});
kiri.on({type:"shareaccount",signedIn:true,login:"octo",session:"ks_7f3a9c21e04b"});
kiri.on({type:"shareusage",login:"octo",used:1.9e9,cap:5e9,tunnels:3,rate:2e6});
kiri.on({type:"share",id:1,source:"web",target:"web (localhost:8080)",state:"connecting"});
kiri.on({type:"share",id:1,state:"live",url:"https://web-k3x9.runkiri.cloud"});
kiri.on({type:"share",id:2,source:"api",target:"api (localhost:3000)",state:"connecting"});
kiri.on({type:"share",id:2,state:"live",url:"https://api-7qp2.runkiri.cloud"});

kiri.on({type:"share",id:3,source:"postgres",target:"postgres (localhost:5432)",state:"connecting"});

const sent = [];
window.webkit = { messageHandlers: { kiri: { postMessage: (m) => { const o = JSON.parse(m); sent.push({ type: o.cmd, ...o }); } } } };
// UI tests assert English copy; pin the language so a Chinese system locale cannot fail them.
prefs.lang = "en";
if (typeof paintChrome === "function") paintChrome();


// For the tests in other languages (zh.js, ar.js, langswitch.js): English left on screen.
// Words that are fine in a Chinese window: products, systems, Kubernetes
// kinds, units and the fixture's own names.
const KEEP = new Set(("kiri dev share cloud docker hub desktop compose kubernetes kubectl macos windows linux wsl mac pc ubuntu " +
  "orbstack finder explorer pod pods service services deployment replicaset clusterip nodeport loadbalancer ingress configmap secret " +
  "canestack labs cpu cpus gpu ram id ids url dns tcp udp http https ip ipv6 ssh tls api cli json yaml sha k3s ok " +
  "web api postgres worker nginx alpine node python octo localhost runkiri default rancher latest").split(" "));
function english(root) {
  const out = [];
  const walk = (el) => {
    // An element in a language of its own (a language's name in its own words) is meant so.
    if (el.closest?.("[hidden], .xterm, pre, code, kbd, .mono, .p-mono, [data-copy], svg, script, style, [lang]:not(html)")) return;
    if (el.nodeType === 1 && getComputedStyle(el).display === "none") return;
    for (const n of el.childNodes) {
      if (n.nodeType === 3) {
        // Two or more ordinary English words in a row, none of them a name.
        for (const m of n.textContent.matchAll(/[A-Za-z][a-z']+(?:[ -][A-Za-z][a-z']+)+/g)) {
          const words = m[0].toLowerCase().split(/[ -]/);
          if (words.filter((w) => !KEEP.has(w)).length >= 2) out.push(m[0]);
        }
      } else if (n.nodeType === 1) walk(n);
    }
    for (const a of ["title", "aria-label", "placeholder"]) {
      const v = el.getAttribute?.(a);
      if (v && /[a-z]{2,} [a-z]{2,}/.test(v) && v.toLowerCase().split(/\W+/).filter((w) => w.length > 1 && !KEEP.has(w)).length >= 2 && !/[一-鿿\u0600-\u06FF]/.test(v)) out.push(`${a}="${v}"`);
    }
  };
  walk(root);
  return [...new Set(out)];
}
