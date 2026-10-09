#!/usr/bin/env bash
set -euo pipefail

binary="$(realpath "$1")"
target="$2"
package="git-yard-$target"
mkdir -p "dist/$package"
install -m 755 "$binary" "dist/$package/git-yard"
cp LICENSE README.md "dist/$package/"
"dist/$package/git-yard" --version
tar -czf "dist/$package.tar.gz" -C "dist/$package" .
if command -v sha256sum >/dev/null; then
  (cd dist && sha256sum "$package.tar.gz" > "$package.tar.gz.sha256")
else
  (cd dist && shasum -a 256 "$package.tar.gz" > "$package.tar.gz.sha256")
fi
