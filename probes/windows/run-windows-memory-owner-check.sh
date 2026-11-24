#!/usr/bin/env bash
# Run from the directory containing the compiled probe, in an MSYS2 shell.
set -euo pipefail
probe=${1:-./owner.exe}
coproc HOLDER { timeout 30 "$probe" holder; }
holder_pid=$HOLDER_PID
exec {owner_out}<&"${HOLDER[0]}"
exec {owner_in}>&"${HOLDER[1]}"
for phase in before held released; do
    read -rt 12 tag usage budget <&"$owner_out"
    [[ $tag == "$phase" ]]
    printf '%s %s %s\n' "$tag" "$usage" "$budget"
    "$probe"
    if [[ $phase != released ]]; then printf A >&"$owner_in"; fi
done
wait "$holder_pid"
