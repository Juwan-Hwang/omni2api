#!/bin/sh
# frames + mix → H.264 CRF16 / AAC 256k, loudness-normalised to -14 LUFS (two-pass loudnorm)
set -e
OUT=${1:-deliver/omni2api-promo-v1.mp4}
mkdir -p deliver
J=$(ffmpeg -hide_banner -i audio/mix.wav -af loudnorm=I=-14:TP=-1.5:LRA=11:print_format=json -f null - 2>&1 | sed -n '/^{/,/^}/p')
mi=$(echo "$J" | python3 -c "import json,sys;d=json.load(sys.stdin);print(d['input_i'],d['input_tp'],d['input_lra'],d['input_thresh'],d['target_offset'])")
set -- $mi "$OUT"
ffmpeg -y -hide_banner -loglevel error -framerate 30 -i film/out/frames/%05d.jpg -i audio/mix.wav \
  -af "loudnorm=I=-14:TP=-1.5:LRA=11:measured_I=$1:measured_TP=$2:measured_LRA=$3:measured_thresh=$4:offset=$5:linear=true,aresample=48000" \
  -vf "scale=in_range=pc:out_range=tv,format=yuv420p" -c:v libx264 -preset slow -crf 16 -pix_fmt yuv420p -profile:v high -movflags +faststart -tune film \
  -c:a aac -b:a 256k -ar 48000 -shortest "$6"
echo done "$6"
