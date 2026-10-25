#!/bin/bash
# HTTP/2 & HTTP/3 对照压测（curl 驱动）：每档 3 次取中位
#
# 前置：
#   - sweety 用 TLS 站点（示例 18443/TCP + 18443/UDP），nginx 用 18444（ssl + http2 + quic）
#   - 必须确认端口上只有一个服务进程（避免 SO_REUSEPORT 分流污染）
#
# 用法：./matrix_h2h3.sh <sweety_port> <nginx_port> <file> <procs> <reqs>
set -u
SP=${1:-18443}; NP=${2:-18444}; FILE=${3:-1k.bin}; PROCS=${4:-1}; REQS=${5:-200}

med() { printf "%s\n" "$@" | sort -n | sed -n 2p; }

run() { # port proto
  local port=$1 proto=$2
  local a="" i
  for ((i=0;i<REQS;i++)); do a="$a https://127.0.0.1:$port/$FILE -o /dev/null"; done
  local t0 t1
  t0=$(date +%s%N)
  seq $PROCS | xargs -P $PROCS -I@ sh -c \
    "curl -sk $proto -H 'Host: bench.local' $a > /dev/null 2>&1"
  t1=$(date +%s%N)
  echo $(( (t1 - t0) / 1000000 ))   # 总毫秒数，越小越好
}

for proto in --http2 --http3; do
  s1=$(run $SP $proto); s2=$(run $SP $proto); s3=$(run $SP $proto)
  n1=$(run $NP $proto); n2=$(run $NP $proto); n3=$(run $NP $proto)
  sm=$(med $s1 $s2 $s3); nm=$(med $n1 $n2 $n3)
  echo "$proto $FILE conns=$PROCS: sweety=${sm}ms nginx=${nm}ms ratio=$(awk "BEGIN{printf \"%.2fx\", $sm/$nm}") lower_is_better"
done
