// The USB view: devices listed whether the engine runs or not; Attach and
// Detach say what they're doing and come back with a toast; Linux rows say
// where containers find the device; Windows without usbipd-win offers it.
// A row opens its details (one at a time); the device just attached opens
// by itself; the Attached column says since when. A device that left and
// is waited for says so and can be let go.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const rows = () => [...document.querySelectorAll("#content tbody tr.row")];
const opened = () => [...document.querySelectorAll("#content tr.usb-more")].map((r) => r.dataset.more);
const mac = { type: "usb", can: "attach", note: "A device in Kiri is gone from the Mac until you detach it.", devices: [
  { key: "0x01100000", name: "Zigbee 3.0 USB Dongle Plus", id: "10c4:ea60", speed: "full", state: "host", path: "" },
  { key: "0x02200000", name: "USB Mouse", id: "046d:c077", speed: "low", state: "kiri", path: "" }] };

sent.length = 0;
show("usb");
t("opening the view asks for the devices", () => eq(sent.some((m) => m.type === "usb"), true, "sent"));
t("it says it's looking until they come", () => eq(/Looking for USB devices/.test($("content").textContent), true, "text"));
kiri.on(mac);
t("the title is USB, marked Beta", () => eq([$("title").firstChild.textContent, $("title").querySelector(".beta")?.textContent], ["USB", "Beta"], "title"));
t("a banner says it's in beta, what works, and links to feedback", () => {
  const b = document.querySelector(".usb-beta");
  eq([!!b, /in beta/.test(b?.textContent), /webcams and audio/.test(b?.textContent), /issues\/new/.test(b?.querySelector("[data-open-site]")?.dataset.openSite || "")], [true, true, true, true], "banner");
});
sent.length = 0;
document.querySelector("[data-usbbeta]").click();
t("closing it saves that, and it stays closed", () => {
  eq(sent.filter((m) => m.type === "setpref").map((m) => [m.name, m.value]), [["usbBetaClosed", true]], "pref");
  kiri.on(mac);
  eq(document.querySelector(".usb-beta"), null, "gone");
});
t("the sidebar marks it Beta too", () => eq(document.querySelector('nav [data-view="usb"] .beta')?.textContent, "Beta", "nav"));
t("one row per device", () => eq(rows().map((r) => r.dataset.key), ["0x01100000", "0x02200000"], "rows"));
t("a device on the Mac can be attached, one in Kiri detached", () => {
  eq(rows().map((r) => r.querySelector("[data-usbgo]")?.dataset.usbgo), ["attach", "detach"], "actions");
  eq(rows().map((r) => r.querySelector(".pill").textContent), ["On this Mac", "In Kiri"], "state");
});
t("the sidebar counts the devices in Kiri", () => eq($("n-usb").textContent, "1", "count"));
t("the subtitle says how many", () => eq($("subtitle").textContent, "1 in Kiri · 2 devices", "subtitle"));
t("the note says what attaching means", () => eq(/gone from the Mac/.test(document.querySelector(".usb-note")?.textContent || ""), true, "note"));

sent.length = 0;
rows()[0].querySelector("[data-usbgo]").click();
t("Attach sends the device's key", () => eq(sent.filter((m) => m.type === "usbattach").map((m) => m.name), ["0x01100000"], "sent"));
t("and the row says it's attaching, not clickable twice", () => {
  const b = rows()[0].querySelector(".actions button");
  eq([b.textContent, b.disabled, !!b.querySelector(".spin")], ["Attaching…", true, true], "busy");
});
t("a refresh while it works keeps it busy", () => { kiri.on(mac); eq(rows()[0].querySelector(".actions button").textContent, "Attaching…", "busy"); });
kiri.on({ type: "usbdone", key: "0x01100000", text: "Zigbee 3.0 USB Dongle Plus is in Kiri" });
kiri.on({ ...mac, devices: mac.devices.map((d) => ({ ...d, state: "kiri" })) });
t("done: the toast says so and the row can be detached", () => {
  eq(/is in Kiri/.test(document.body.textContent), true, "toast");
  eq(rows()[0].querySelector("[data-usbgo]")?.dataset.usbgo, "detach", "action");
  eq($("n-usb").textContent, "2", "count");
});
t("the device just attached opens its details, and only it", () => eq(opened(), ["0x01100000"], "open"));
sent.length = 0;
rows()[1].querySelector("[data-usbgo]").click();
kiri.on({ type: "usbdone", key: "0x02200000", error: "USB Mouse was not attached: the device is busy" });
t("Detach sends its key; an error is a toast and the row is free again", () => {
  eq(sent.filter((m) => m.type === "usbdetach").map((m) => m.name), ["0x02200000"], "sent");
  eq(/device is busy/.test(document.body.textContent), true, "toast");
  eq(rows()[1].querySelector("[data-usbgo]")?.dataset.usbgo, "detach", "free");
});
kiri.on({ type: "usbdone", key: "0x01100000" });
t("a cancelled password prompt says nothing", () => eq(rows()[0].querySelector(".actions button").disabled, false, "free"));

t("each device's icon says what it is", () => {
  kiri.on({ type: "usb", can: "attach", devices: [
    { key: "m", name: "Razer Basilisk V3 Pro 35K", id: "1532:00d6", state: "host", kind: "mouse" },
    { key: "k", name: "ROG AZOTH", id: "0b05:1a83", state: "host", kind: "keyboard" },
    { key: "z", name: "Sonoff Zigbee 3.0 USB Dongle Plus", id: "10c4:ea60", state: "host" },
    { key: "s", name: "USB-SERIAL CH340 (COM4)", id: "1a86:7523", state: "host" },
    { key: "h", name: "USB2.0 HID", id: "0bda:1100", state: "host", kind: "" }] });
  const d = (k) => document.querySelector(`tr[data-key="${k}"] .tile path`)?.getAttribute("d");
  eq([d("m") === PH.mouse, d("k") === PH.keyboard, d("z") === PH.broadcast, d("s") === PH.circuitry, d("h") === PH.usb], [true, true, true, true, true], "icons");
  // In Kiri the engine can't read its interfaces: the kind it had stays.
  kiri.on({ type: "usb", can: "attach", devices: [{ key: "m", name: "Razer Basilisk V3 Pro 35K", id: "1532:00d6", state: "kiri", kind: "" }] });
  eq(d("m") === PH.mouse, true, "kept");
  kiri.on(mac);
});
t("the Attached column says since when; a device on the computer has a dash", () => {
  const now = Math.floor(Date.now() / 1000);
  kiri.on({ type: "usb", can: "attach", devices: [
    { key: "a", name: "Sonoff Zigbee 3.0 USB Dongle Plus", id: "10c4:ea60", speed: "full", state: "kiri", kind: "radio", maker: "ITead", serial: "a4c1386d2b5ef011", bus: 1, dev: 2, since: now - 120 },
    { key: "b", name: "Razer Basilisk V3 Pro 35K", id: "1532:00d6", speed: "full", state: "kiri", kind: "mouse", maker: "Razer", bus: 1, dev: 4, since: now - 30 },
    { key: "c", name: "DM30 USB Microphone", id: "352f:0101", speed: "full", state: "host", kind: "audio", maker: "KTMicro", serial: "221018", bus: 2, dev: 12 }] });
  eq(rows().map((r) => r.cells[4].textContent), ["2 min ago", "just now", "—"], "attached");
});
document.querySelector('[data-usbmore="b"]').click();
t("the caret opens one device's details: what it is, where, since when", () => {
  eq(opened(), ["b"], "open");
  const m = document.querySelector("tr.usb-more").textContent;
  eq([/Razer/.test(m), /Mouse/.test(m), /1532:00d6/.test(m), /USB 1\.1 · 12 Mbps/.test(m), /Bus 1 · Device 4/.test(m), /just now/.test(m)], [true, true, true, true, true, true], "facts");
  eq(/Serial number/.test(m), false, "no serial when it has none");
});
t("a mouse in Kiri warns that this computer can't use it meanwhile", () => eq(/can't use it while it's in Kiri/.test(document.querySelector(".usb-warn")?.textContent || ""), true, "warn"));
rows()[2].cells[2].click();
t("a click on another row opens it and closes the first", () => {
  eq(opened(), ["c"], "open");
  const m = document.querySelector("tr.usb-more").textContent;
  eq([/221018/.test(m), /Bus 2 · Device 12/.test(m), /sound or video/.test(m)], [true, true, true], "facts");
});
rows()[2].cells[2].click();
t("a second click closes it", () => eq(opened(), [], "closed"));
kiri.on({ type: "usb", can: "none", devices: [{ key: "/dev/bus/usb/001/003", name: "CP2102", id: "10c4:ea60", speed: "full", state: "shared", path: "/dev/ttyUSB0", kind: "serial", serial: "0001", bus: 1, dev: 3 }] });
document.querySelector('[data-usbmore="/dev/bus/usb/001/003"]').click();
t("Linux: the details say how a container gets it, with a copy button", () => {
  eq(document.querySelector("tr.usb-more .usb-use [data-copy]")?.dataset.copy, "--device /dev/ttyUSB0", "copy");
  eq(rows()[0].cells[4].textContent, "—", "no attach time: containers already see it");
});
kiri.on({ type: "usb", can: "attach", devices: [{ key: "1-3", name: "CP210x USB to UART Bridge (COM3)", id: "10c4:ea60", state: "kiri", kind: "serial", serial: "0001", busid: "1-3", since: Math.floor(Date.now() / 1000) - 7200 }] });
document.querySelector('[data-usbmore="1-3"]').click();
t("Windows: the bus id is its location", () => eq(/Location1-3/.test(document.querySelector("tr.usb-more").textContent.replace(/\s+/g, "")), true, "busid"));
document.querySelector('[data-usbmore="1-3"]').click();

const zig = { key: "z", name: "Sonoff Zigbee 3.0 USB Dongle Plus", id: "10c4:ea60", speed: "full", state: "kiri", kind: "radio", since: Math.floor(Date.now() / 1000) - 60,
  paths: [{ path: "/dev/ttyUSB0", what: "serial port" }, { path: "/dev/bus/usb/003/002", what: "raw USB" }] };
kiri.on({ type: "usb", can: "attach", devices: [zig,
  { key: "h", name: "Zigbee on the Mac", id: "1111:2222", state: "host" },
  { key: "n", name: "USB 2.5G Ethernet", id: "0bda:8156", state: "kiri", kind: "network", paths: [{ path: "eth1", what: "network interface" }, { path: "/dev/bus/usb/003/003", what: "raw USB" }] },
  { key: "w", name: "Waiting", id: "3333:4444", state: "kiri", paths: [] }] });
document.querySelector('[data-usbmore="z"]').click();
const side = () => document.querySelector("tr.usb-more .usb-side");
t("in Kiri: the docker run flag for its device, copyable, and every path inside Linux", () => {
  eq(side().querySelector("[data-copy]")?.dataset.copy, "--device /dev/ttyUSB0", "flag");
  eq([...side().querySelectorAll(".usb-paths dt")].map((x) => x.textContent), ["/dev/ttyUSB0", "/dev/bus/usb/003/002"], "paths");
  eq([...side().querySelectorAll(".usb-paths dd")].map((x) => x.textContent), ["serial port", "raw USB"], "what");
});
side().querySelector('[data-usbtab="compose"]').click();
t("the compose tab gives the compose lines, and stays picked", () => {
  eq(side().querySelector("[data-copy]")?.dataset.copy, 'devices: ["/dev/ttyUSB0:/dev/ttyUSB0"]', "compose");
  eq(opened(), ["z"], "still open");
  kiri.on({ type: "usb", can: "attach", devices: [zig] });
  eq(side().querySelector(".usb-seg .on")?.dataset.usbtab, "compose", "kept");
});
side().querySelector('[data-usbtab="run"]').click();
kiri.on({ type: "usb", can: "attach", devices: [zig, { key: "h", name: "Zigbee on the Mac", id: "1111:2222", state: "host" },
  { key: "n", name: "USB 2.5G Ethernet", id: "0bda:8156", state: "kiri", kind: "network", paths: [{ path: "eth1", what: "network interface" }] },
  { key: "w", name: "Waiting", id: "3333:4444", state: "kiri", paths: [] }] });
const sideOf = (k) => { document.querySelector(`[data-usbmore="${k}"]`).click(); return side().textContent; };
t("on the computer: attach it to see where it goes", () => eq(/Attach it to see where/.test(sideOf("h")), true, "host"));
t("a network adapter: no device file, its interface name instead", () => {
  const x = sideOf("n");
  eq([/network interface inside Linux/.test(x), /eth1/.test(x), !!side().querySelector("[data-copy]")], [true, true, false], "net");
});
t("no paths yet: Linux hasn't made a device", () => eq(/hasn't made a device/.test(sideOf("w")), true, "waiting"));
document.querySelector('[data-usbmore="w"]').click();
kiri.on(mac);
$("gq").value = "zig"; $("gq").dispatchEvent(new Event("input"));
t("the search narrows the devices", () => eq(rows().length, 1, "rows"));
$("gq").value = ""; $("gq").dispatchEvent(new Event("input"));

kiri.on({ type: "state", state: "stopped" });
t("with the engine stopped the devices are still listed", () => eq(rows().length, 2, "rows"));
kiri.on({ type: "state", state: "running" });

kiri.on({ type: "usb", can: "none", note: "Containers see them.", devices: [{ key: "/dev/bus/usb/001/003", name: "CP2102", id: "10c4:ea60", speed: "full", state: "shared", path: "/dev/ttyUSB0" }] });
t("Linux: no attach; the row says where containers find it, with a copy button", () => {
  eq(rows()[0].querySelector("[data-usbgo]"), null, "no attach");
  eq(rows()[0].querySelector("[data-copy]")?.dataset.copy, "--device /dev/ttyUSB0", "copy");
  eq(rows()[0].querySelector(".pill").textContent, "Containers see it", "state");
});

// A board that left the Mac's bus while in Kiri: waited for, and can be let go.
kiri.on({ type: "usb", can: "attach", devices: [{ key: "0x01300000", name: "ESP32-S3", id: "", speed: "", state: "waiting", kind: "" }] });
t("a device waited for says so, and offers Stop waiting", () => {
  const r = rows()[0];
  eq([r.querySelector(".pill").textContent, r.querySelector("[data-usbgo]")?.dataset.usbgo, r.querySelector("[data-usbgo]")?.textContent], ["Waiting to come back", "detach", "Stop waiting"], "row");
  eq($("n-usb").textContent, "", "not counted as in Kiri");
});
sent.length = 0;
rows()[0].querySelector("[data-usbgo]").click();
t("Stop waiting sends a detach for it", () => eq(sent.filter((m) => m.type === "usbdetach").map((m) => m.name), ["0x01300000"], "sent"));
kiri.on({ type: "usbdone", key: "0x01300000", text: "Given back to the Mac" });

kiri.on({ type: "usb", devices: [], missing: true });
sent.length = 0;
t("Windows without usbipd-win: it offers to install it", () => eq(!!document.querySelector('[data-usbgo="install"]'), true, "button"));
document.querySelector('[data-usbgo="install"]').click();
t("Install sends usbinstall and waits", () => {
  eq(sent.map((m) => m.type).includes("usbinstall"), true, "sent");
  eq(document.querySelector('[data-usbgo="install"]').disabled, true, "busy");
});
kiri.on({ type: "usb", devices: [] });
t("no devices: it says to plug one in", () => eq(/No USB devices/.test($("content").textContent), true, "empty"));
show("containers");
report(R);
