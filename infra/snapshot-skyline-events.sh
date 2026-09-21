#!/usr/bin/env bash
set -euo pipefail

ACTION=${1:-count}
EVENTS_PATH=${SKYLINE_EVENTS_PATH:-/run/skyline-speeder/events.jsonl}
# skyline-speederd renames the log to this once it reaches
# runtime.events_max_mib and starts a fresh one; only one old file is kept.
ROTATED_PATH="$EVENTS_PATH.1"

inode_of() {
    stat -c %i "$1" 2>/dev/null || echo 0
}

lines_of() {
    if [ -f "$1" ]; then
        wc -l < "$1"
    else
        echo 0
    fi
}

# Whether FILE is the one a cursor INODE:SKIP was taken on. tmpfs never hands
# a freed inode number out again, but ext4 and others do, and a rotated-in file
# can then carry the old number; one shorter than SKIP lines cannot be it.
is_cursor_file() {
    [ "$2" != 0 ] && [ "$(inode_of "$1")" = "$2" ] && [ "$(lines_of "$1")" -ge "$3" ]
}

# Print FILE from line SKIP+1 on, if it exists.
print_after() {
    if [ -f "$1" ]; then
        tail -n "+$(($2 + 1))" "$1"
    fi
}

case "$ACTION" in
    count)
        lines_of "$EVENTS_PATH"
        ;;
    from)
        START_LINE=${2:-1}
        if [ ! -f "$EVENTS_PATH" ]; then
            exit 0
        fi
        if ! [[ "$START_LINE" =~ ^[0-9]+$ ]] || [ "$START_LINE" -lt 1 ]; then
            echo "START_LINE must be a positive integer" >&2
            exit 2
        fi
        sed -n "${START_LINE},\$p" "$EVENTS_PATH"
        ;;
    cursor)
        # A bare line count stops meaning anything once the daemon rotates the
        # log, so the cursor also records which file (inode) it counted in.
        echo "$(inode_of "$EVENTS_PATH"):$(lines_of "$EVENTS_PATH")"
        ;;
    since)
        CURSOR=${2:-0:0}
        if ! [[ "$CURSOR" =~ ^([0-9]+):([0-9]+)$ ]]; then
            echo "CURSOR must be the INODE:LINES printed by '$0 cursor'" >&2
            exit 2
        fi
        INODE=${BASH_REMATCH[1]}
        SKIP=${BASH_REMATCH[2]}
        if is_cursor_file "$EVENTS_PATH" "$INODE" "$SKIP"; then
            print_after "$EVENTS_PATH" "$SKIP"
        elif is_cursor_file "$ROTATED_PATH" "$INODE" "$SKIP"; then
            print_after "$ROTATED_PATH" "$SKIP"
            print_after "$EVENTS_PATH" 0
        else
            # INODE 0: there was no log when the cursor was taken, so all of
            # it is new. Anything else: rotated at least twice, and the
            # events between the cursor and the older file are gone.
            if [ "$INODE" != 0 ]; then
                echo "event log rotated more than once since cursor $CURSOR;" \
                    "events before $ROTATED_PATH are lost" >&2
            fi
            print_after "$ROTATED_PATH" 0
            print_after "$EVENTS_PATH" 0
        fi
        ;;
    *)
        echo "Usage: $0 count|from [START_LINE]|cursor|since CURSOR" >&2
        exit 2
        ;;
esac
