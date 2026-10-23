#!/bin/bash
# run_seat.sh NAME CMD... : start the seat, ask once from the caller, stop the seat
export PATH=$HOME/.omni2api/bin:/usr/games:$PATH TZ=Asia/Shanghai
cd "$(dirname "$0")"; n=$1; shift
setsid "$@" > $n.seat.log 2>&1 & pid=$!
sleep 1.5
./ask.sh > $n.caller.txt
sleep 1.5
kill -- -$pid 2>/dev/null
echo "== caller got:"; cat $n.caller.txt; echo "== seat log:"; cat $n.seat.log
