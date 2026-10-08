#!/usr/bin/env bash
# Checks every shell command in the runbooks against the current-source `topup` CLI and the
# committed OpenAPI documents, every invocation of a deploy script in the documentation against
# the script's arguments, then proves the checker rejects a deliberately wrong fixture.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

# Reuse the workspace build cache so repeated runs only rebuild what changed.
target_dir=${CARGO_TARGET_DIR:-$root/target}
cargo build --locked -q -p topup --manifest-path "$root/Cargo.toml" --target-dir "$target_dir"
topup="$target_dir/debug/topup"

# Prints the subcommand names listed in the "Commands:" section of one help text.
help_commands() {
    awk '
        /^Commands:/ { in_commands = 1; next }
        /^[^ ]/ { in_commands = 0 }
        in_commands && /^  [a-z]/ { print $1 }
    ' "$1" | grep -vx help || true
}

# Prints the long flags listed in the "Options:" section of one help text.
help_flags() {
    awk '
        /^Options:/ { in_options = 1; next }
        /^[^ ]/ { in_options = 0 }
        in_options { print }
    ' "$1" | grep -Eo -- '--[a-z0-9][a-z0-9-]*' | sort -u
}

# Records help, flags, and nested subcommands for every command path, keyed by "a.b".
describe_command() {
    local key=$1 sub
    shift
    "$topup" "$@" --help > "$tmp/help.$key"
    help_flags "$tmp/help.$key" > "$tmp/flags.$key"
    help_commands "$tmp/help.$key" > "$tmp/commands.$key"
    while IFS= read -r sub; do
        describe_command "${key:+$key.}$sub" "$@" "$sub"
    done < "$tmp/commands.$key"
}
describe_command ""
mv "$tmp/commands." "$tmp/commands.root"

# The merchant API's and the operator's admin API's operations.
jq -r '
  .paths | to_entries[] | .key as $path
  | .value | keys[] | select(. != "parameters")
  | ascii_upcase + " " + $path
' "$root/crates/topup/openapi.json" "$root/crates/topup/openapi.admin.json" \
    | sort -u > "$tmp/openapi.operations"

# Joins backslash continuations and prints one line per shell command, skipping heredoc bodies.
# Fences may be indented (a code block in a list item); the indentation is removed.
extract_commands() {
    awk '
        /^[[:space:]]*```(sh|bash|shell)[[:space:]]*$/ && !in_shell {
            in_shell = 1; continued = ""; heredoc = ""
            match($0, /^[[:space:]]*/)
            indent = RLENGTH
            next
        }
        in_shell && /^[[:space:]]*```[[:space:]]*$/ {
            if (continued != "") print FILENAME ": " continued
            in_shell = 0
            next
        }
        !in_shell { next }
        { $0 = substr($0, 1, indent) ~ /^[[:space:]]*$/ ? substr($0, indent + 1) : $0 }
        heredoc != "" {
            if ($0 == heredoc) heredoc = ""
            next
        }
        {
            line = $0
            if (continued != "") line = continued " " line
            if (line ~ /\\[[:space:]]*$/) {
                sub(/\\[[:space:]]*$/, "", line)
                continued = line
                next
            }
            continued = ""
            redirects = line
            gsub(/<<</, "", redirects)
            if (match(redirects, /<<-?[[:space:]]*['\''"]?[A-Za-z_][A-Za-z0-9_]*/)) {
                heredoc = substr(redirects, RSTART, RLENGTH)
                sub(/^<<-?[[:space:]]*['\''"]?/, "", heredoc)
            }
            print FILENAME ": " line
        }
    ' "$@"
}

# Splits one shell line into words, one simple command per output line. Quotes are removed, and
# pipes, lists, and command substitutions start a new command.
split_commands() {
    local line=$1 char quote="" word="" index next
    local -a words=()
    flush_word() {
        [[ -n "$word" ]] && words+=("$word")
        word=""
    }
    flush_command() {
        flush_word
        (( ${#words[@]} > 0 )) && printf '%s\n' "${words[*]}"
        words=()
    }
    for ((index = 0; index < ${#line}; index++)); do
        char=${line:index:1}
        next=${line:index+1:1}
        if [[ "$char" == '$' && "$next" == '(' && "$quote" != "'" ]]; then
            flush_command
            index=$((index + 1))
            continue
        fi
        if [[ -n "$quote" ]]; then
            if [[ "$char" == "$quote" ]]; then
                quote=""
            elif [[ "$char" == ')' && "$quote" == '"' ]]; then
                flush_command
            else
                word+=$char
            fi
            continue
        fi
        case "$char" in
            "'" | '"') quote=$char ;;
            ' ' | $'\t') flush_word ;;
            '|' | ';' | '&' | '(' | ')' | '`') flush_command ;;
            '#')
                if [[ -z "$word" ]]; then
                    break
                fi
                word+=$char
                ;;
            *) word+=$char ;;
        esac
    done
    flush_command
}

# Prints "topup" and its arguments for one simple command, or nothing if it does not run topup.
topup_arguments() {
    local -a words
    local index=0 count
    read -r -a words <<< "$1"
    count=${#words[@]}
    while (( index < count )) && [[ "${words[index]}" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; do
        index=$((index + 1))
    done
    (( index < count )) || return 0
    if [[ "${words[index]}" == docker && "${words[index + 1]:-}" == compose ]]; then
        index=$((index + 2))
        while (( index < count )) && [[ "${words[index]}" != exec && "${words[index]}" != run ]]; do
            index=$((index + 1))
        done
        (( index < count )) || return 0
        index=$((index + 1))
        while (( index < count )) && [[ "${words[index]}" == -* ]]; do
            case "${words[index]}" in
                -e | --env | -u | --user | -w | --workdir | --index | --entrypoint | --name)
                    index=$((index + 2))
                    ;;
                *) index=$((index + 1)) ;;
            esac
        done
        # The restore-check service's entrypoint is `topup restore-check --config FILE`, so its
        # run arguments are restore-check flags.
        if [[ "${words[index]:-}" == restore-check && "${words[index + 1]:-}" != topup ]]; then
            printf 'topup restore-check %s\n' "${words[*]:index+1}"
            return 0
        fi
        index=$((index + 1))
    elif [[ "${words[index]}" == cargo && "${words[index + 1]:-}" == run ]]; then
        [[ " ${words[*]} " == *" -p topup "* ]] || return 0
        while (( index < count )) && [[ "${words[index]}" != -- ]]; do
            index=$((index + 1))
        done
        (( index < count )) || return 0
        printf 'topup %s\n' "${words[*]:index+1}"
        return 0
    fi
    (( index < count )) || return 0
    [[ "${words[index]}" == topup || "${words[index]}" == */topup ]] || return 0
    printf 'topup %s\n' "${words[*]:index+1}"
}

validate_topup() {
    local arguments=$1 source=$2 key="" command index=0 word flag fail=0
    local -a words
    read -r -a words <<< "$arguments"
    if (( ${#words[@]} == 0 )); then
        echo "$source: incomplete topup invocation" >&2
        return 1
    fi
    while (( index < ${#words[@]} )); do
        word=${words[index]}
        [[ "$word" == -* ]] && break
        [[ -s "$tmp/commands.${key:-root}" ]] || break
        if ! grep -Fxq -- "$word" "$tmp/commands.${key:-root}"; then
            command="${key//./ } $word"
            echo "$source: unknown topup subcommand: ${command# }" >&2
            return 1
        fi
        key=${key:+$key.}$word
        index=$((index + 1))
    done
    if [[ -s "$tmp/commands.${key:-root}" ]]; then
        echo "$source: missing nested subcommand for topup ${key//./ }" >&2
        return 1
    fi
    for ((; index < ${#words[@]}; index++)); do
        word=${words[index]}
        [[ "$word" == --* && "$word" != -- ]] || continue
        flag=${word%%=*}
        if ! grep -Fxq -- "$flag" "$tmp/flags.$key"; then
            echo "$source: unknown flag $flag for topup ${key//./ }" >&2
            fail=1
        fi
    done
    return "$fail"
}

path_matches() {
    local actual=$1 spec=$2 index
    local -a actual_parts spec_parts
    IFS=/ read -r -a actual_parts <<< "$actual"
    IFS=/ read -r -a spec_parts <<< "$spec"
    (( ${#actual_parts[@]} == ${#spec_parts[@]} )) || return 1
    for index in "${!spec_parts[@]}"; do
        if [[ "${spec_parts[index]}" == \{*\} ]]; then
            [[ -n "${actual_parts[index]}" ]] || return 1
        elif [[ "${actual_parts[index]}" != "${spec_parts[index]}" ]]; then
            return 1
        fi
    done
}

validate_operation() {
    local method=${1^^} url=$2 source=$3 path spec_method spec_path
    [[ "$url" == *'/v1/'* ]] || return 0
    path="/v1/${url#*/v1/}"
    path=${path%%\?*}
    while read -r spec_method spec_path; do
        if [[ "$method" == "$spec_method" ]] && path_matches "$path" "$spec_path"; then
            return 0
        fi
    done < "$tmp/openapi.operations"
    echo "$source: unknown API operation: $method $path" >&2
    return 1
}

# Checks curl, signing-helper, and `admin` invocations against OpenAPI method and path pairs.
validate_api() {
    local -a words
    local index method=GET fail=0
    read -r -a words <<< "$1"
    (( ${#words[@]} > 0 )) || return 0
    case "${words[0]}" in
        */sign-admin-request.sh)
            validate_operation "${words[1]:-}" "${words[2]:-}" "$2" || fail=1
            ;;
        admin)
            # The runbooks' `admin METHOD PATH [BODY]` helper (runbooks/README.md).
            validate_operation "${words[1]:-}" "${words[2]:-}" "$2" || fail=1
            ;;
        curl)
            for ((index = 1; index < ${#words[@]}; index++)); do
                case "${words[index]}" in
                    -X | --request) method=${words[index + 1]:-} ;;
                    --request=*) method=${words[index]#--request=} ;;
                esac
            done
            for ((index = 1; index < ${#words[@]}; index++)); do
                validate_operation "$method" "${words[index]}" "$2" || fail=1
            done
            ;;
    esac
    return "$fail"
}

# The arguments of the deploy scripts the documentation runs, by path under deploy/: its flags,
# `--flag=` for one taking a value, and its number of positional arguments.
declare -A script_flags=(
    [render.sh]="--restore-check --template --images= --gateway-domain= --origin= --project-name= --no-download"
    [preflight.sh]="--env= --compose= --environment-dir= --workspace= --os-image= --restore-check --offline --unsealed --require-sentry"
    [product/preflight.sh]="--env= --compose= --environment-dir= --workspace= --os-image= --offline --unsealed"
    [pinned-compose.sh]="--no-download"
)
declare -A script_positionals=(
    [render.sh]=1 [preflight.sh]=0 [product/preflight.sh]=0 [pinned-compose.sh]=0
    [verify-attestation.sh]=5 [verify-ingress-evidence.sh]=2 [check-route-modes.sh]=2
    [validate-compose.sh]=0 [runbooks/sign-admin-request.sh]=5
)

# Checks one simple command that runs a deploy script of the table above: known flags, each
# value present, and exactly the script's number of positional arguments.
validate_script() {
    local -a words
    local index=0 name word flag positionals=0 fail=0
    read -r -a words <<< "$1"
    while (( index < ${#words[@]} )) && [[ "${words[index]}" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; do
        index=$((index + 1))
    done
    [[ "${words[index]:-}" =~ (^|/)deploy/(.+\.sh)$ ]] || return 0
    name=${BASH_REMATCH[2]}
    [[ -v "script_positionals[$name]" ]] || return 0
    for ((index = index + 1; index < ${#words[@]}; index++)); do
        word=${words[index]}
        case "$word" in
            '>' | '>>' | '<' | '2>') index=$((index + 1)); continue ;;
            '>'* | '<'* | '2>'*) continue ;;
            --*)
                flag=${word%%=*}
                if [[ " ${script_flags[$name]:-} " == *" $flag= "* ]]; then
                    [[ "$word" == *=* ]] || index=$((index + 1))
                    if (( index >= ${#words[@]} )) && [[ "$word" != *=* ]]; then
                        echo "$2: $flag of deploy/$name needs a value" >&2
                        fail=1
                    fi
                elif [[ " ${script_flags[$name]:-} " != *" $flag "* ]]; then
                    echo "$2: unknown flag $flag for deploy/$name" >&2
                    fail=1
                fi
                ;;
            *) positionals=$((positionals + 1)) ;;
        esac
    done
    if (( positionals != script_positionals[$name] )); then
        echo "$2: deploy/$name takes ${script_positionals[$name]} arguments, not $positionals" >&2
        fail=1
    fi
    return "$fail"
}

validate_commands() {
    local commands=$1 fail=0 source line command arguments
    while IFS= read -r line; do
        source=${line%%: *}
        source=${source#"$root/"}
        line=${line#*: }
        while IFS= read -r command; do
            arguments=$(topup_arguments "$command")
            if [[ -n "$arguments" ]]; then
                validate_topup "${arguments#topup}" "$source" || fail=1
            fi
            validate_api "$command" "$source" || fail=1
            validate_script "$command" "$source" || fail=1
        done < <(split_commands "$line")
    done < "$commands"
    return "$fail"
}

# Only the deploy scripts' arguments, for documentation outside the runbooks.
validate_script_commands() {
    local commands=$1 fail=0 source line command
    while IFS= read -r line; do
        source=${line%%: *}
        source=${source#"$root/"}
        line=${line#*: }
        while IFS= read -r command; do
            validate_script "$command" "$source" || fail=1
        done < <(split_commands "$line")
    done < "$commands"
    return "$fail"
}

mapfile -t runbook_files < <(
    find "$root/deploy/runbooks" -maxdepth 1 -name '*.md' | sort
)
extract_commands "${runbook_files[@]}" > "$tmp/runbook.commands"
validate_commands "$tmp/runbook.commands"
mapfile -t document_files < <(
    find "$root/deploy" "$root/docs" "$root/CONTRIBUTING.md" -name '*.md' \
        -not -path '*/node_modules/*' -not -path "$root/deploy/runbooks/*" | sort
)
extract_commands "${document_files[@]}" > "$tmp/document.commands"
validate_script_commands "$tmp/document.commands"

extract_commands "$root/deploy/runbooks/tests/valid.md" > "$tmp/valid.commands"
if ! validate_commands "$tmp/valid.commands"; then
    echo "valid fixture unexpectedly failed" >&2
    exit 1
fi

extract_commands "$root/deploy/runbooks/tests/invalid.md" > "$tmp/invalid.commands"
if validate_commands "$tmp/invalid.commands" 2> "$tmp/negative.out"; then
    echo "negative fixture unexpectedly passed" >&2
    exit 1
fi
expected=(
    'unknown topup subcommand: bogus'
    'unknown topup subcommand: route bogus'
    'missing nested subcommand for topup route'
    'unknown flag --bogus for topup attest'
    'unknown flag --no-such-flag for topup reconcile'
    'unknown flag --once for topup restore-check'
    'unknown topup subcommand: restore'
    'unknown API operation: POST /v1/admin/not-a-route'
    'unknown API operation: GET /v1/admin/routes/r/pause'
    'unknown API operation: DELETE /v1/admin/reports/daily'
    'unknown API operation: PUT /v1/admin/products'
    'deploy/verify-attestation.sh takes 5 arguments, not 4'
    'unknown flag --bogus for deploy/render.sh'
    'deploy/render.sh takes 1 arguments, not 2'
)
for message in "${expected[@]}"; do
    if ! grep -Fq -- "$message" "$tmp/negative.out"; then
        echo "negative fixture did not report: $message" >&2
        cat "$tmp/negative.out" >&2
        exit 1
    fi
done
if (( $(wc -l < "$tmp/negative.out") != ${#expected[@]} )); then
    echo "negative fixture reported unexpected errors:" >&2
    cat "$tmp/negative.out" >&2
    exit 1
fi

echo "runbook CLI references match current-source topup --help"
echo "runbook API references match crates/topup/openapi.json and openapi.admin.json"
echo "documented deploy script invocations match their arguments"
echo "negative fixture failed with exactly ${#expected[@]} expected errors"
