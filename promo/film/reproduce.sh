#!/bin/sh
# Rebuild both cuts of the omni2api launch film from this folder.
#   sh reproduce.sh          → deliver/omni2api-promo-v2-{en,zh}.rebuild.mp4
#   LANGS=zh sh reproduce.sh → one cut only
#   RE3D=1 sh reproduce.sh   → also re-render the 3D open/reprise frames (film/frames/{open,end}) from film/o3d.html
# Every input is committed: real terminal captures (session/, film/data/), fonts (film/fonts/), three.js r170
# (film/node_modules/three), the 3D frames, and the outline wordmark (film/lockup.svg). Audio uses fixed seeds.
set -e
cd "$(dirname "$0")"
pip install -q -r requirements.txt
python3 -m playwright install --with-deps chromium
[ -d repos/earshot ] || { git clone -q https://github.com/Leonxlnx/earshot repos/earshot; git -C repos/earshot checkout -q 925ebe56a1b755e5d354ab642bb8f344440cfa9c; }
pip install -q --no-deps -e repos/earshot
(python3 -m http.server 8777 --directory film >/tmp/http-omni-film.log 2>&1 &)
sleep 1
if [ "${RE3D:-0}" = 1 ]; then
  rm -rf film/frames/open film/frames/end
  python3 r3d_seq.py open 6.0 film/frames/open --sub 3    # 180 frames
  python3 r3d_seq.py end 3.0 film/frames/end --sub 3      # 90 frames
fi
python3 music.py
for L in ${LANGS:-en zh}; do
  python3 sound.py "$L"                                   # writes audio/mix_$L.wav from the comp's __events()
  python3 render_film.py --comp "comp.html?lang=$L" --out "film/out/frames_$L" --workers 2
  ./encode.sh "$L" "deliver/omni2api-promo-v2-$L.rebuild.mp4"
done
