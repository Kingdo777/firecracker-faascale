#!/bin/bash
sudo setfacl -m u:${USER}:rw /dev/kvm
sudo rm -f /tmp/firecracker.socket
sudo ./network-ifup tap0
/home/kingdo/CLionProjects/firecracker/build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker --api-sock /tmp/firecracker.socket
sudo ./network-ifdown tap0
