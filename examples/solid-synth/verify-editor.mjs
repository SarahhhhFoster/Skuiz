// Renders the editor in a headless DOM and checks that Solid's state really
// drives the parameter messages the plugin listens for.
//
// Optional developer tool: Skuiz itself builds and tests with cargo alone.
//   npm install jsdom && node examples/solid-synth/verify-editor.mjs
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { JSDOM } from "jsdom";

const here = dirname(fileURLToPath(import.meta.url));
// Exactly what `concat!` assembles in lib.rs.
const page = [
  "src/editor.head.html",
  "src/vendor/solid.js",
  "src/vendor/solid-knobs.js",
  "src/editor.tail.html",
]
  .map((f) => readFileSync(join(here, f), "utf8"))
  .join("");

const sent = [];
const dom = new JSDOM(page, {
  runScripts: "dangerously",
  pretendToBeVisual: true,
  // wry injects the bridge before page scripts run; mirror that, or the
  // scripts would execute twice and render the app twice.
  beforeParse(window) {
    window.ipc = { postMessage: (m) => sent.push(m) };
  },
});

const doc = dom.window.document;
const fail = (msg) => {
  console.error("FAIL:", msg);
  process.exit(1);
};

// 1. Solid rendered something. Three solid-knobs controls (frequency, level,
//    cutoff — each a role=slider div); waveform and note keys are buttons.
const knobs = doc.querySelectorAll('[role=slider]');
if (knobs.length !== 3) fail(`expected 3 knobs, rendered ${knobs.length}`);
for (const knob of knobs) {
  if (knob.querySelectorAll("svg path").length !== 2)
    fail("knob is missing its track/sweep arcs");
}
const waveButtons = doc.querySelectorAll(".waves button");
if (waveButtons.length !== 4) fail(`expected 4 waveform buttons, got ${waveButtons.length}`);
const keyButtons = doc.querySelectorAll(".keys button");
if (keyButtons.length !== 7) fail(`expected 7 note keys, got ${keyButtons.length}`);
if (!doc.body.textContent.includes("Solid Synth")) fail("heading did not render");

// 2. Mount effects pushed every parameter down to the DSP.
const ids = new Set(sent.map((m) => m.split(" ")[1]));
for (const id of ["0", "1", "2", "3"]) {
  if (!ids.has(id)) fail(`no initial value sent for parameter ${id}; sent: ${JSON.stringify(sent)}`);
}

// 3. Nudging a knob with the keyboard sends the new value. solid-knobs
//    only listens for arrow keys while the control is focused.
sent.length = 0;
knobs[1].dispatchEvent(new dom.window.Event("focus"));
knobs[1].dispatchEvent(new dom.window.KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }));
if (!sent.some((m) => m.startsWith("set_param 2 "))) {
  fail(`knob nudge did not reach the DSP; sent: ${JSON.stringify(sent)}`);
}

// 4. Clicking a waveform updates both state and the UI.
sent.length = 0;
waveButtons[2].dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
if (!sent.includes("set_param 1 2")) fail(`waveform click did not send; sent: ${JSON.stringify(sent)}`);
if (waveButtons[2].getAttribute("data-on") !== "true") fail("selected waveform not highlighted");

// 5. A value arriving from the plugin updates the knob and is NOT echoed
//    back, or two instances would drive each other in a loop.
sent.length = 0;
dom.window.skuizOnParam(0, 440);
if (sent.length !== 0) fail(`incoming value was echoed back: ${JSON.stringify(sent)}`);
if (!doc.body.textContent.includes("440.0 Hz")) fail("incoming value did not update the display");
if (knobs[0].getAttribute("aria-valuenow") !== "440")
  fail(`knob aria-valuenow did not follow the incoming value: ${knobs[0].getAttribute("aria-valuenow")}`);

console.log("editor OK:", [
  `${knobs.length} knobs`,
  `${waveButtons.length} waveforms`,
  `${keyButtons.length} note keys`,
  "effects push state",
  "no echo loop",
].join(", "));
