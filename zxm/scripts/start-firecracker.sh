#!/bin/bash
sudo setfacl -m u:${USER}:rw /dev/kvm
sudo rm -f /tmp/firecracker.socket
echo "" > /tmp/firecracker.log
sudo /home/kingdo/CLionProjects/firecracker/zxm/scripts/network-ifup tap0
/home/kingdo/CLionProjects/firecracker/build/cargo_target/x86_64-unknown-linux-musl/release/firecracker --api-sock /tmp/firecracker.socket --no-seccomp --log-path /tmp/firecracker.log --level debug
sudo /home/kingdo/CLionProjects/firecracker/zxm/scripts/network-ifdown tap0
