#!/bin/bash

# Working directory
WD="$(
  cd "$(dirname "$0")"
  pwd -P
)"

mkdir $WD/reports
REPORT_FILE="$WD/reports/$(date +"%Y-%m-%d_%H:%M:%S")_test.txt"

# Not a proxy anymore, is a dummy for this test
PROXY=http://app1.atrium.127.0.0.1.nip.io:8080
BENCH_CMD="rewrk -c 400 -t 8 -d 20s -h ${PROXY} --pct >> $REPORT_FILE"

test_proxy() {
  if [ "$(curl -s ${PROXY})" != "Hello World!" ]; then
    echo "Error: curl command did not return 'Hello World!'"
    exit 1
  fi
}

#####################################################################
#                            INSTALL RWRK                           #
#####################################################################

sudo apt install -y libssl-dev
sudo apt install -y pkg-config
cargo install rewrk --git https://github.com/ChillFish8/rewrk.git

#####################################################################
#                             AXUM 0.6.12                           #
#####################################################################

# Build for production
cd ${WD}
# Switch back to 0.6.12
sed -i 's/axum = { version="=0.6.15"/axum = { version="=0.6.12"/g' Cargo.toml
cat Cargo.toml | grep "axum = { version"
cargo build --release
# Start proxy
cp ${WD}/atrium.yaml ${WD}/target/release/
cd ${WD}/target/release/
./atrium &
ATRIUM_PROXY_PID=$!
sleep 2
# Test proxy
echo -e "####################\n### AXUM 0.6.12  ###\n####################\n" >>$REPORT_FILE
test_proxy
eval ${BENCH_CMD}
# Shutdown
kill $ATRIUM_PROXY_PID

#####################################################################
#                             AXUM 0.6.15                           #
#####################################################################

# Build for production
cd ${WD}
# Switch to axum 0.6.15
sed -i 's/axum = { version="=0.6.12"/axum = { version="=0.6.15"/g' Cargo.toml
cat Cargo.toml | grep "axum = { version"
cargo build --release
# Start proxy
cd ${WD}/target/release/
./atrium &
ATRIUM_PROXY_PID=$!
sleep 2
# Test proxy
echo -e "####################\n### AXUM 0.6.15  ###\n####################\n" >>$REPORT_FILE
test_proxy
eval ${BENCH_CMD}
# Shutdown
kill $ATRIUM_PROXY_PID

cat $REPORT_FILE
