#!/bin/bash
# HTTP/2 over TLS 对照压测（curl 驱动，单连接多请求，多进程并发）
#
# 用法：
#   ./h2_tls.sh <sweety_port> <nginx_port> <file> <procs> <reqs>
# 例：
#   ./h2_tls.sh 18443 18444 1k.bin 32 800
#
# 说明：curl 每个进程复用一条 h2 连接连续发 N 个请求，多进程并行模拟并发连接。
# 客户端本身是瓶颈（约 5 万 rps/32 进程），因此绝对值仅供参考，
# 但两台服务器用同一客户端、同一时刻测量，相对差值有效。
set -u
SP=${1:-18443}; NP=${2:-18444}; FILE=${3:-1k.bin}; PROCS=${4:-32}; REQS=${5:-800}

bench() {
  local port=$1 proto=$2
  local args="" i
  for ((i=0; i<REQS; i++)); do args="$args https://127.0.0.1:$port/$FILE -o /dev/null"; done
  local t0 t1 ok ms
  t0=$(date +%s%N)
  seq "$PROCS" | xargs -P "$PROCS" -I@ sh -c \
    "curl -sk $proto -H 'Host: bench.local' -w '%{http_code}\n' $args > /tmp/h2bench.@.txt 2>/dev/null"
  t1=$(date +%s%N)
  ok=$(cat /tmp/h2bench.*.txt 2>/dev/null | grep -c '^200$')
  rm -f /tmp/h2bench.*.txt
  ms=$(( (t1 - t0) / 1000000 ))
  echo "port=$port $proto $FILE ok=$ok ms=$ms rps=$(( ok * 1000 / (ms>0?ms:1) ))"
}

bench "$SP" "--http2"
bench "$NP" "--http2"
bench "$SP" "--http1.1"
bench "$NP" "--http1.1"
