#!/usr/bin/env bash
# Stamp, update and check plugins against templates/plugin. Needs git and
# kickstart 0.6.0 (`cargo install --locked kickstart@0.6.0 --features cli`);
# `check` needs only git.
#
#   scripts/plugin_template.sh new <name> --description "..."
#       [--capabilities records,http,model,memory,jobs,sources]
#       [--tools a,b] [--records a,b]
#     Stamp plugins/<name> in this repo, compile it into the app behind a
#     `plugin-<name>` feature, then format, lint and test it. Leaves
#     everything unstaged.
#
#   scripts/plugin_template.sh new <name> --description "..." --out <dir>
#       [--crates workspace|vX.Y.Z] [...]
#     Stamp <dir>/<name> for another repo, on the runtime's crates from a
#     workspace that declares them, or by git at a release tag (this
#     checkout's version by default). Registering it there is yours.
#
#   scripts/plugin_template.sh update <plugin dir>
#     Bring the template's changes since the plugin's recorded version into
#     it, three-way per file: the template as recorded is the base, the
#     plugin is ours, the template now is theirs (git merge-file). A
#     conflict is left marked in the file. Works for a plugin in another
#     repo too, run from this checkout.
#
#   scripts/plugin_template.sh link <plugin dir> <answers.json>
#     Put a plugin written before the template on it: record the answers
#     that describe it, and the template as it is now. Changes nothing else.
#
#   scripts/plugin_template.sh check
#     Fail when a plugin in plugins/ has no .template.json, or was stamped
#     from an older template than templates/plugin is now. CI runs this.
#
#   scripts/plugin_template.sh selftest
#     Render the template with every option, then lint and test what it
#     made. CI runs this, so the template can't drift from the contract.
#
# .template.json records the template's version as the git tree of
# templates/plugin, not a commit: a tree is the template's content alone, so
# a template change and the plugins' updates go in one commit.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

TEMPLATE=templates/plugin
# The plugin's own once written: an update never patches these.
SKIP=README.md
NAME_RULE='^[a-z][a-z0-9]{0,31}$'
LIST_RULE='^([a-z][a-z0-9_]{0,30}(,[a-z][a-z0-9_]{0,30})*)?$'
CAPS_RULE='^((records|http|model|memory|jobs|sources)(,(records|http|model|memory|jobs|sources))*)?$'
DEP_RULE='^(path|workspace|v[0-9]+\.[0-9]+\.[0-9]+)$'

die() { echo "plugin_template: $*" >&2; exit 1; }

SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT

# The git tree of templates/plugin as it is in the working tree, written to
# the object store through a scratch index (the real index isn't touched).
template_tree() {
  local index="$SCRATCH/index"
  rm -f "$index"
  GIT_INDEX_FILE=$index git read-tree HEAD
  GIT_INDEX_FILE=$index git add -A -- "$TEMPLATE"
  GIT_INDEX_FILE=$index git write-tree --prefix="$TEMPLATE/"
  rm -f "$index"
}

# render <tree> <answers.json> <dest>: the template at <tree>, rendered with
# the answers, as <dest>/<name>/.
render() {
  local tree=$1 answers=$2 dest=$3 work
  command -v kickstart >/dev/null ||
    die "kickstart isn't installed: cargo install --locked kickstart@0.6.0 --features cli"
  git cat-file -e "$tree^{tree}" 2>/dev/null || die "template version ${tree:0:12} isn't in this clone"
  work=$(mktemp -d "$SCRATCH/render.XXXXXX")
  mkdir -p "$work/template" "$work/out"
  git archive "$tree" | tar -x -C "$work/template"
  if ! kickstart "$work/template" -o "$work/out" -i "$answers" >"$work/log" 2>&1; then
    cat "$work/log" >&2
    die "the template refused the answers or didn't render"
  fi
  # Formatted as the repo formats, so a stamped plugin and the renders an
  # update merges against differ only where someone changed them.
  find "$work/out" -name '*.rs' -exec rustfmt --edition 2024 {} +
  mkdir -p "$dest"
  cp -r "$work/out/." "$dest/"
}

# record <plugin dir> <tree> <answers.json>: one field per line, so this
# script reads it back without a JSON tool.
record() {
  {
    echo "{"
    echo "  \"template\": \"$TEMPLATE\","
    echo "  \"tree\": \"$2\","
    echo "  \"answers\": $(tr -d '\n' <"$3"),"
    echo "  \"skip\": [\"$SKIP\"]"
    echo "}"
  } >"$1/.template.json"
}

recorded() { # recorded <plugin dir> tree|answers
  local file="$1/.template.json"
  [ -f "$file" ] || die "$1 has no .template.json"
  case "$2" in
    tree) sed -n 's/^  "tree": "\([0-9a-f]*\)",$/\1/p' "$file" ;;
    answers) sed -n 's/^  "answers": \({.*}\),$/\1/p' "$file" ;;
  esac
}

answers_json() { # name description capabilities tools records crates
  printf '{"name": "%s", "description": "%s", "capabilities": "%s", "tools": "%s", "records": "%s", "crates": "%s"}\n' "$@"
}

check_answers() { # name description capabilities tools records crates
  local name=$1 description=$2 caps=$3 tools=$4 records=$5 dep=$6 reserved tool
  [[ $name =~ $NAME_RULE ]] || die "name '$name': lowercase letters and digits, starting with a letter, at most 32"
  reserved=$(sed -n '/pub const RESERVED_NAMES/,/];/p' crates/plugin-api/src/manifest.rs)
  ! grep -q "\"$name\"" <<<"$reserved" || die "name '$name' is reserved"
  [[ -n $description && ${#description} -le 300 && $description != *[\"\\]* ]] ||
    die "description: one line, at most 300 characters, no \" or \\"
  [[ $caps =~ $CAPS_RULE ]] || die "capabilities '$caps': from records, http, model, memory, jobs, sources"
  [[ $tools =~ $LIST_RULE ]] || die "tools '$tools': lowercase words joined by _, comma-separated"
  [[ $records =~ $LIST_RULE ]] || die "records '$records': lowercase words joined by _, comma-separated"
  [[ $dep =~ $DEP_RULE ]] || die "crates '$dep': path, workspace, or a tag such as v0.6.0"
  [[ -z $records || ,$caps, == *,records,* ]] || die "record types need the records capability"
  for tool in ${tools//,/ }; do
    case "$tool" in run | definition | effect_of) die "a tool can't be named '$tool': the Tool trait has that method" ;; esac
    [ "$(tr ',' '\n' <<<"$tools" | grep -cx "$tool")" = 1 ] || die "tool '$tool' is given twice"
    ! grep -qx "  - name: ${name}_$tool" plugins/*/plugin.yaml 2>/dev/null ||
      die "tool ${name}_$tool already belongs to a plugin"
  done
}

# Insert <text> after the last line matching <pattern> in <file>; fail if
# nothing matches, so a changed file is noticed rather than skipped.
after_last() {
  local file=$1 pattern=$2 text=$3
  awk -v pat="$pattern" -v add="$text" '
    { lines[NR] = $0; if ($0 ~ pat) last = NR }
    END {
      if (!last) exit 1
      for (i = 1; i <= NR; i++) { print lines[i]; if (i == last) print add }
    }' "$file" >"$file.new" || { rm -f "$file.new"; die "$file: no line matches $pattern"; }
  mv "$file.new" "$file"
}

register() { # name
  local name=$1 kind
  kind="$(tr '[:lower:]' '[:upper:]' <<<"${name:0:1}")${name:1}"
  after_last crates/app/Cargo.toml '^plugin-[a-z0-9]+ = \["dep:plugin-' "plugin-$name = [\"dep:plugin-$name\"]"
  after_last crates/app/Cargo.toml '^plugin-[a-z0-9]+ = \{ path = "\.\./\.\./plugins/' \
    "plugin-$name = { path = \"../../plugins/$name\", optional = true }"
  after_last crates/app/src/main.rs 'plugins\.push\(Arc::new\(plugin_' \
    "    #[cfg(feature = \"plugin-$name\")]
    plugins.push(Arc::new(plugin_$name::${kind}Plugin));"
  after_last crates/app/src/main.rs '^            expected \+= 1;$' "        }
        #[cfg(feature = \"plugin-$name\")]
        {
            expected += 1;"
  echo "compiled in behind the plugin-$name feature: crates/app/Cargo.toml, crates/app/src/main.rs"
}

cmd_new() {
  local name=${1:-} description="" caps="records" tools="lookup" records="" out="" dep=""
  [ -n "$name" ] && [ "${name:0:1}" != - ] || die "usage: new <name> --description \"...\" [--capabilities a,b] [--tools a,b] [--records a,b] [--out dir [--crates workspace|vX.Y.Z]]"
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
      --description) description=$2; shift 2 ;;
      --capabilities) caps=$2; shift 2 ;;
      --tools) tools=$2; shift 2 ;;
      --records) records=$2; shift 2 ;;
      --out) out=$2; shift 2 ;;
      --crates) dep=$2; shift 2 ;;
      *) die "unknown option $1" ;;
    esac
  done
  if [ -z "$out" ]; then
    [ -z "$dep" ] || [ "$dep" = path ] || die "--crates is for --out; in this repo it's path"
    dep=path
    out=plugins
  else
    [ "$dep" != path ] || die "--crates path is for this repo's own plugins/"
    [ -n "$dep" ] || dep="v$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
  fi
  check_answers "$name" "$description" "$caps" "$tools" "$records" "$dep"
  [ ! -e "$out/$name" ] || die "$out/$name already exists"
  answers_json "$name" "$description" "$caps" "$tools" "$records" "$dep" >"$SCRATCH/answers.json"
  local tree
  tree=$(template_tree)
  render "$tree" "$SCRATCH/answers.json" "$out"
  record "$out/$name" "$tree" "$SCRATCH/answers.json"
  echo "stamped $out/$name from template ${tree:0:12}"
  if [ "$dep" = path ]; then
    register "$name"
    cargo fmt --all
    cargo clippy -p "plugin-$name" -p app --all-targets --features "app/plugin-$name" -- -D warnings
    cargo test -p "plugin-$name"
  else
    echo "next: add it to your workspace and binary, grant it in plugins.yaml, and run its tests"
  fi
}

cmd_update() {
  local dir=${1:?usage: update <plugin dir>} old new name file merged=0 conflicted=0 added=0
  old=$(recorded "$dir" tree)
  new=$(template_tree)
  [ "$old" != "$new" ] || { echo "$dir is on the template as it is"; return; }
  recorded "$dir" answers >"$SCRATCH/answers.json"
  name=$(sed -n 's/.*"name": "\([a-z0-9]*\)".*/\1/p' "$SCRATCH/answers.json")
  render "$old" "$SCRATCH/answers.json" "$SCRATCH/base"
  render "$new" "$SCRATCH/answers.json" "$SCRATCH/theirs"
  while IFS= read -r file; do
    [ "$file" != "$SKIP" ] || continue
    if [ ! -e "$dir/$file" ]; then
      mkdir -p "$(dirname "$dir/$file")"
      cp "$SCRATCH/theirs/$name/$file" "$dir/$file"
      added=$((added + 1))
      continue
    fi
    local base="$SCRATCH/base/$name/$file"
    [ -e "$base" ] || base=/dev/null
    if git merge-file -L plugin -L "template ${old:0:12}" -L "template ${new:0:12}" \
      "$dir/$file" "$base" "$SCRATCH/theirs/$name/$file"; then
      merged=$((merged + 1))
    else
      conflicted=$((conflicted + 1))
      echo "conflict, marked in $dir/$file"
    fi
  done < <(cd "$SCRATCH/theirs/$name" && find . -type f | sed 's|^\./||' | sort)
  record "$dir" "$new" "$SCRATCH/answers.json"
  echo "$dir: template ${old:0:12} -> ${new:0:12}: $merged merged, $added added, $conflicted with conflicts"
  [ "$conflicted" = 0 ]
}

cmd_link() {
  local dir=${1:?usage: link <plugin dir> <answers.json>} answers=${2:?usage: link <plugin dir> <answers.json>}
  [ ! -e "$dir/.template.json" ] || die "$dir is already on the template"
  [ -f "$answers" ] || die "no file $answers"
  record "$dir" "$(template_tree)" "$answers"
  echo "linked $dir to the template"
}

cmd_check() {
  local tree bad=0 dir
  tree=$(template_tree)
  for dir in plugins/*/; do
    dir=${dir%/}
    # The worked example is written by hand, to show what the template's
    # stubs become; merging the template into it would only conflict.
    [ "$dir" != plugins/example ] || continue
    if [ ! -f "$dir/.template.json" ]; then
      echo "$dir: not on the template (scripts/plugin_template.sh link)"
      bad=1
    elif [ "$(recorded "$dir" tree)" != "$tree" ]; then
      echo "$dir: behind the template (scripts/plugin_template.sh update $dir)"
      bad=1
    fi
  done
  [ "$bad" = 0 ] && echo "every plugin is on the template as it is"
  return "$bad"
}

cmd_selftest() {
  # name, capabilities, tools, records: every option, and the sparse cases
  # where a template's conditionals break.
  local cases=(
    "templateall records,http,model,memory,jobs,sources look_up,list_all item,other_item"
    "templatebare - lookup -"
    "templatesource sources - -"
    "templaterecords records,http lookup item"
  )
  local one name caps tools records packages=() tree
  for one in "${cases[@]}"; do
    [ ! -e "plugins/${one%% *}" ] || die "plugins/${one%% *} exists; remove it first"
  done
  cp Cargo.lock "$SCRATCH/Cargo.lock"
  # shellcheck disable=SC2064 # the paths are fixed now, on purpose
  trap "rm -rf plugins/template{all,bare,source,records}; cp '$SCRATCH/Cargo.lock' Cargo.lock; rm -rf '$SCRATCH'" EXIT
  tree=$(template_tree)
  for one in "${cases[@]}"; do
    read -r name caps tools records <<<"$one"
    [ "$caps" != - ] || caps=""
    [ "$tools" != - ] || tools=""
    [ "$records" != - ] || records=""
    answers_json "$name" "A case the template must render." "$caps" "$tools" "$records" path >"$SCRATCH/$name.json"
    render "$tree" "$SCRATCH/$name.json" plugins
    packages+=(-p "plugin-$name")
  done
  # Formatted as rendered, and clean under the repo's lints.
  cargo fmt "${packages[@]}" -- --check
  cargo clippy "${packages[@]}" --all-targets -- -D warnings
  cargo test "${packages[@]}"
  echo "the template renders plugins that lint clean and pass their tests"
}

case "${1:-}" in
  new) shift; cmd_new "$@" ;;
  update) shift; cmd_update "$@" ;;
  link) shift; cmd_link "$@" ;;
  check) cmd_check ;;
  selftest) cmd_selftest ;;
  *) die "usage: new | update | link | check | selftest (see the top of this script)" ;;
esac
