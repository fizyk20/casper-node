#!/usr/bin/env bash

unset OFFSET
unset SLEEP_INTERVAL
unset TIME_OUT

for ARGUMENT in "$@"
do
    KEY=$(echo "$ARGUMENT" | cut -f1 -d=)
    VALUE=$(echo "$ARGUMENT" | cut -f2 -d=)
    case "$KEY" in
        offset) OFFSET=${VALUE} ;;
        sleep_interval) SLEEP_INTERVAL=${VALUE} ;;
        timeout) TIME_OUT=${VALUE} ;;
        *)
    esac
done

OFFSET=${OFFSET:-1}

# ----------------------------------------------------------------
# MAIN
# ----------------------------------------------------------------

source "$NCTL"/sh/utils/main.sh

await_n_blocks "$OFFSET" true '' $SLEEP_INTERVAL $TIME_OUT
