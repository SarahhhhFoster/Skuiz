#!/bin/sh
# Scaffold a new Skuiz plugin from an example: copy it out of the repo,
# rename it, and rewire its manifest so it builds standalone.
#
#   tools/new-plugin.sh <name> [template] [dest-dir]
#
#   name       crate name, lower-case-with-dashes (e.g. my-gain)
#   template   example to copy: shared-gain (default), trigger-note,
#              solid-synth, pd-tremolo
#   dest-dir   parent directory for the project (default: current dir)
#
# The copy's Cargo.toml is rewritten from workspace inheritance to path
# dependencies on this checkout, so the project builds on its own. If you
# move the project to another machine, switch those to
# `skuiz-core = { git = "https://github.com/sarahhhh/skuiz" }` etc.
set -eu

NAME=${1:-}
TEMPLATE=${2:-shared-gain}
DEST_PARENT=${3:-.}

SKUIZ_ROOT=$(cd "$(dirname "$0")/.." && pwd)

die() { echo "new-plugin: $*" >&2; exit 1; }

[ -n "$NAME" ] || die "usage: new-plugin.sh <name> [template] [dest-dir]"
echo "$NAME" | grep -qE '^[a-z][a-z0-9-]*$' \
    || die "name must be lower-case-with-dashes: $NAME"
[ -d "$SKUIZ_ROOT/examples/$TEMPLATE" ] \
    || die "no such example: $TEMPLATE (pick one of: $(ls "$SKUIZ_ROOT/examples" | tr '\n' ' '))"

PROJ="$DEST_PARENT/$NAME"
[ ! -e "$PROJ" ] || die "already exists: $PROJ"

NEW_LIB=$(echo "$NAME" | tr - _)
OLD_LIB=$(echo "$TEMPLATE" | tr - _)
# "trigger-note" -> "Trigger Note"
title() { echo "$1" | tr - ' ' | awk '{for(i=1;i<=NF;i++) $i=toupper(substr($i,1,1)) substr($i,2)}1'; }
OLD_TITLE=$(title "$TEMPLATE")
NEW_TITLE=$(title "$NAME")

# Build flags the template's bundle needs (pd-tremolo: --features libpd).
FEATURES=$(sed -n 's/.*cargo build -p [a-z0-9-]* \(--features [a-z0-9-]*\).*/\1/p' \
    "$SKUIZ_ROOT/examples/$TEMPLATE/bundle.sh")

# Portable in-place sed (GNU and BSD differ on -i).
sed_inplace() {
    _f=$1; shift
    sed -i.bak "$@" "$_f" && rm "$_f.bak"
}

cp -R "$SKUIZ_ROOT/examples/$TEMPLATE" "$PROJ"

# Rename: file contents first, then file names (src/bin/<template>-standalone.rs).
cd "$PROJ"
grep -rl "$TEMPLATE" . | while read -r f; do sed_inplace "$f" "s/$TEMPLATE/$NAME/g"; done
grep -rl "$OLD_LIB" . | while read -r f; do sed_inplace "$f" "s/$OLD_LIB/$NEW_LIB/g"; done
grep -rl "$OLD_TITLE" . | while read -r f; do sed_inplace "$f" "s/$OLD_TITLE/$NEW_TITLE/g"; done
find . -depth -name "*$TEMPLATE*" | while read -r f; do
    mv "$f" "$(dirname "$f")/$(basename "$f" | sed "s/$TEMPLATE/$NAME/g")"
done

# Rewire the manifest: workspace inheritance -> literals, workspace deps ->
# path deps on this checkout, and an empty [workspace] table so cargo does
# not absorb the project into an enclosing workspace.
sed_inplace Cargo.toml \
    -e 's/^version\.workspace = true/version = "0.1.0"/' \
    -e 's/^edition\.workspace = true/edition = "2021"/' \
    -e 's/^license\.workspace = true/license = "MIT"/' \
    -e '/^repository\.workspace = true/d' \
    -e "s|^skuiz-\([a-z0-9-]*\)\.workspace = true|skuiz-\1 = { path = \"$SKUIZ_ROOT/crates/skuiz-\1\" }|" \
    -e "s|^skuiz-\([a-z0-9-]*\) = { workspace = true, optional = true }|skuiz-\1 = { path = \"$SKUIZ_ROOT/crates/skuiz-\1\", optional = true }|"
printf '\n# Standalone project, not part of the Skuiz workspace.\n[workspace]\n' >> Cargo.toml

# The template's bundle scripts assume the repo layout; make them local.
for script in bundle.sh bundle-vst3.sh; do
    [ -f "$script" ] || continue
    sed_inplace "$script" \
        -e 's|cd "$(dirname "$0")/../.."|cd "$(dirname "$0")"|' \
        -e "s| -p $NAME||" \
        -e 's|# Crate versions are workspace-inherited; the root manifest is the source\.|# The package version doubles as the bundle version.|' \
        -e 's|BUNDLE=target/\(.*\)|BUNDLE="${CARGO_TARGET_DIR:-target}/\1"|' \
        -e 's|cp "target/$BUILD_TYPE/\([^"]*\)"|cp "${CARGO_TARGET_DIR:-target}/$BUILD_TYPE/\1"|'
done

cat > install.sh <<EOF
#!/bin/sh
# Build $NAME and install the CLAP plugin for the current user.
# Override the destination with CLAP_INSTALL_DIR=/some/dir.
set -eu
cd "\$(dirname "\$0")"
case "\$(uname -s)" in
  Darwin)
    ./bundle.sh
    DEST="\${CLAP_INSTALL_DIR:-\$HOME/Library/Audio/Plug-Ins/CLAP}"
    mkdir -p "\$DEST"
    rm -rf "\$DEST/$NAME.clap"
    cp -R "\${CARGO_TARGET_DIR:-target}/$NAME.clap" "\$DEST/"
    echo "installed \$DEST/$NAME.clap"
    ;;
  Linux)
    cargo build $FEATURES
    DEST="\${CLAP_INSTALL_DIR:-\$HOME/.clap}"
    mkdir -p "\$DEST"
    cp "\${CARGO_TARGET_DIR:-target}/\${BUILD_TYPE:-debug}/lib$NEW_LIB.so" "\$DEST/$NAME.clap"
    echo "installed \$DEST/$NAME.clap"
    ;;
  *)
    echo "unsupported OS; on Windows copy target/debug/$NEW_LIB.dll to %COMMONPROGRAMFILES%\\CLAP\\$NAME.clap" >&2
    exit 1
    ;;
esac
EOF
chmod +x install.sh

echo "created $PROJ"
echo "  cd $PROJ && ./install.sh   # build + install for the current user"
echo "  edit src/lib.rs first: set your own PluginInfo id and vendor"
