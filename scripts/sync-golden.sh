#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
usage: scripts/sync-golden.sh [--check] <notes-core checkout>

Copies the Lumbridge Notes golden format files from a checkout of the
repository that holds the notes-core crate (crates/notes-core/tests/golden/)
to tests/golden/ in this repo.

The files pin the note format that three implementations must agree on byte
for byte: lumbridge-notes' store.rs, Lumbridge TUI's notebook drawer
(src/client/notebook.rs in lumbridge-tui) and the notes-core crate. They
are blessed on the notes-core side from the drawer's original functions; this repo
never edits them. After a sync, run

  cargo test --test golden

and, in lumbridge-tui with this repo as its crates/lumbridge-notes submodule,

  cargo test --bin lumbridge-tui client::notebook::golden

An equal MANIFEST.sha256 on both sides means both test the same files.
The bless tools (bless/) stay with notes-core.

  --check  copy nothing; fail if the files here differ from the checkout's.
USAGE
}

check=0
case "${1:-}" in
  -h|--help)
    usage
    exit 0
    ;;
  --check)
    check=1
    shift
    ;;
esac
if [[ $# -ne 1 ]]; then
  usage >&2
  exit 2
fi

source_dir=$(cd "$1" && pwd)/crates/notes-core/tests/golden
[[ -f "$source_dir/MANIFEST.sha256" ]] || {
  echo "no golden files at $source_dir" >&2
  exit 1
}
repo_root=$(cd "$(dirname "$0")/.." && pwd)
target=$repo_root/tests/golden

# Refuse a source whose files do not match its own manifest: a half-blessed
# checkout would otherwise spread here.
(cd "$source_dir" && sha256sum --quiet -c MANIFEST.sha256) || {
  echo "$source_dir does not match its MANIFEST.sha256; re-bless it there first" >&2
  exit 1
}

if [[ "$check" == 1 ]]; then
  diff -r --exclude=bless "$source_dir" "$target"
  echo "golden files match $source_dir"
  exit 0
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/notes-golden-sync.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
(cd "$source_dir" && tar -cf - --exclude=./bless .) | tar -xf - -C "$scratch"
rm -rf "$target"
mkdir -p "$(dirname "$target")"
mv "$scratch" "$target"
trap - EXIT
chmod -R u+rwX,go+rX "$target"
echo "synced $target from $source_dir"
echo "manifest $(sha256sum "$target/MANIFEST.sha256" | cut -d' ' -f1)"
