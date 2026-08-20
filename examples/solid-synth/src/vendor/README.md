# Vendored SolidJS + solid-knobs

`solid.js` is SolidJS 1.9.15 — all of `solid-js` and `solid-js/web`, plus
the `solid-js/html` tagged-template helper — bundled into one IIFE that
assigns `globalThis.Solid`, built with esbuild:

```sh
npm install solid-js@1.9.15 esbuild
# entry-solid.js: globalThis.Solid = { ...core, ...web, html }
npx esbuild entry-solid.js --bundle --format=iife --minify \
    --target=safari15 --outfile=solid.js
```

`solid-knobs.js` is solid-knobs 0.5.2 (`Control`, `Arc`, `ValueInput`,
ranges) bundled the same way into `globalThis.SolidKnobs`, with one twist:
a shim aliases `solid-js` to `globalThis.Solid` so the knobs share the
page's single reactive core instead of bundling a second one:

```js
// solid-shim.js
module.exports = globalThis.Solid;
```

```sh
npm install solid-knobs
# entry-knobs.js: globalThis.SolidKnobs = knobs
npx esbuild entry-knobs.js --bundle --format=iife --minify \
    --target=safari15 \
    --alias:solid-js/web=./solid-shim.js --alias:solid-js=./solid-shim.js \
    --outfile=solid-knobs.js
```

They are committed rather than built because a plugin editor is a single
inlined document with no module resolution, and because building Skuiz
should need cargo and nothing else. Regenerate them with the commands
above when upgrading. Both are MIT licensed, like Skuiz.
