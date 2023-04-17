#!/bin/bash

# Working directory
WD="$(
  cd "$(dirname "$0")"
  pwd -P
)"

mkdir $WD/reports
REPORT_FILE="$WD/reports/$(date +"%Y-%m-%d_%H:%M:%S")_merge.txt"

# Not a proxy anymore, is a dummy for this test
PROXY=http://app1.atrium.127.0.0.1.nip.io:8080
BENCH_CMD="rewrk -c 400 -t 8 -d 20s -h ${PROXY} --pct >> $REPORT_FILE"

axum_bench() {
  # Build for production
  cd ${WD}
  # Switch to target version
  sed -i "s/axum = { version=\"=[0-9]\{1,\}\.[0-9]\{1,\}\.[0-9]\{1,\}\"/axum = { version=\"=$1\"/g" Cargo.toml
  # Test that switch was made properly
  if grep -q $1 Cargo.toml; then
    echo "Changed Axum version in Cargo.toml successfully !"
  else
    echo "Error: Changing Axum version in Cargo.toml failed !"
    exit 1
  fi
  cargo build --release
  # Start proxy
  cp ${WD}/atrium.yaml ${WD}/target/release/
  cd ${WD}/target/release/
  ./atrium &
  TEST_PROXY_PID=$!
  sleep 2
  # Test proxy
  echo -e "####################\n### AXUM $1  ###\n####################\n" >>$REPORT_FILE
  if [ "$(curl -s ${PROXY})" != "Hello World!" ]; then
    echo "Error: curl command did not return 'Hello World!'"
    exit 1
  fi
  eval ${BENCH_CMD}
  # Shutdown
  kill $TEST_PROXY_PID
}

#####################################################################
#                            INSTALL RWRK                           #
#####################################################################

sudo apt install -y libssl-dev
sudo apt install -y pkg-config
cargo install rewrk --git https://github.com/ChillFish8/rewrk.git

####################################################################
#                            DO THE TESTS                          #
####################################################################

axum_bench 0.6.12
axum_bench 0.6.13
axum_bench 0.6.14
axum_bench 0.6.15
axum_bench 0.6.12

cat $REPORT_FILE | grep -E 'AXUM|Req/Sec'
