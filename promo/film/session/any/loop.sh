while :; do
  oh-seat next >/dev/null || continue
  oh-seat reply "$(date)"
done
