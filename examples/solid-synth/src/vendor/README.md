# Vendored SolidJS

`solid.js` is SolidJS 1.9.15 (`solid-js`, `solid-js/web`, `solid-js/html`)
bundled into one IIFE that assigns `globalThis.Solid`, built with esbuild:

```sh
npm install solid-js esbuild
npx esbuild entry.js --bundle --format=iife --minify --target=safari15 \
    --outfile=solid.js
```

where `entry.js` imports the pieces and assigns them to `globalThis.Solid`.

It is committed rather than built because a plugin editor is a single
inlined document with no module resolution, and because building Skuiz
should need cargo and nothing else. Regenerate it with the command above
when upgrading Solid.

MIT licensed, like Skuiz.
