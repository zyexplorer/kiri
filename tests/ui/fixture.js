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
