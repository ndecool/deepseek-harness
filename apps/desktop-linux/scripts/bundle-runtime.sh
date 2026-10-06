#!/usr/bin/env bash
# Stage the runtime the packaged app launches: upstream's pinned Node (scripts/primary-runtime/lock.json)
# plus an npm install of @deepseek-ai/dsh, into resources/runtime/{node,dsh}.
#
#   DSH_LINUX_DSH_SPEC  npm spec to install (default: @deepseek-ai/dsh@<apps/cli version>);
#                       a tarball from `pnpm run release:pack` ships fork-local changes.
set -euo pipefail

app_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
repo_dir=$(cd -- "$app_dir/../.." && pwd)
runtime_dir="$app_dir/resources/runtime"
cache_dir="$app_dir/.cache"
lock="$repo_dir/scripts/primary-runtime/lock.json"

case "$(uname -m)" in
  x86_64) arch=x64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "bundle-runtime: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

read_json() { node -e 'const v = process.argv[2].split(".").reduce((o, k) => o?.[k], JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))); if (v === undefined) process.exit(1); console.log(v)' "$@"; }

node_version=$(read_json "$lock" nodeVersion)
node_sha=$(read_json "$lock" "targets.linux-$arch.nodeSha256")
dsh_version=$(read_json "$repo_dir/apps/cli/package.json" version)
dsh_spec=${DSH_LINUX_DSH_SPEC:-@deepseek-ai/dsh@$dsh_version}

archive="node-v$node_version-linux-$arch.tar.gz"
mkdir -p "$cache_dir"
if ! { [ -f "$cache_dir/$archive" ] && echo "$node_sha  $cache_dir/$archive" | sha256sum --check --status; }; then
  echo "bundle-runtime: downloading Node $node_version ($arch)"
  curl --fail --location --silent --show-error --output "$cache_dir/$archive.part" \
    "https://nodejs.org/dist/v$node_version/$archive"
  mv "$cache_dir/$archive.part" "$cache_dir/$archive"
  echo "$node_sha  $cache_dir/$archive" | sha256sum --check --quiet
fi

rm -rf "$runtime_dir"
mkdir -p "$runtime_dir/node" "$runtime_dir/dsh"
tar -xzf "$cache_dir/$archive" -C "$runtime_dir/node" --strip-components=1
# Headers and man pages are build-time only; npm stays for plugin installs.
rm -rf "$runtime_dir/node/include" "$runtime_dir/node/share" "$runtime_dir/node/README.md" "$runtime_dir/node/CHANGELOG.md"

echo "bundle-runtime: installing $dsh_spec"
export PATH="$runtime_dir/node/bin:$PATH"
printf '{ "name": "dsh-desktop-linux-runtime", "private": true }\n' > "$runtime_dir/dsh/package.json"
npm install --prefix "$runtime_dir/dsh" --omit=dev --no-audit --no-fund --loglevel=error "$dsh_spec"

entry="$runtime_dir/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js"
installed=$("$runtime_dir/node/bin/node" "$entry" --version)
echo "bundle-runtime: staged dsh $installed on Node $("$runtime_dir/node/bin/node" --version) in $runtime_dir"
