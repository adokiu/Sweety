#!/bin/bash
# 3 次取中位；先确认端口只有一个实例
cd /tmp/sweety-bench || exit 1
n=$(pgrep -c -x sweety-new); [ "$n" != "1" ] && { echo "ABORT: $n sweety-new instances"; exit 1; }
med() { printf "%s\n" "$@" | sort -n | sed -n 2p; }
one() { timeout 20 ./loadgen3 127.0.0.1:$1 /$2 $3 3 2>/dev/null | sed -E 's/rps=([0-9]+).*/\1/'; }
for spec in "$@"; do
  set -- $spec
  size=$1; conns=$2
  s1=$(one 18080 $size $conns); s2=$(one 18080 $size $conns); s3=$(one 18080 $size $conns)
  n1=$(one 18090 $size $conns); n2=$(one 18090 $size $conns); n3=$(one 18090 $size $conns)
  sm=$(med $s1 $s2 $s3); nm=$(med $n1 $n2 $n3)
  echo "$size sweety=$sm ($s1/$s2/$s3) nginx=$nm ($n1/$n2/$n3) delta=$(awk "BEGIN{printf \"%+.1f%%\", ($sm-$nm)*100/$nm}")"
done
