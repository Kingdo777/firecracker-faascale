#kernel_bin="/home/kingdo/CLionProjects/firecracker/build/img/x86_64/ubuntu/kernel/vmlinux.bin"
#rootfs_path="/home/kingdo/CLionProjects/firecracker/build/img/x86_64/ubuntu/fsfiles/bionic.rootfs.ext4"

KERNEL_DIR=/home/kingdo/CLionProjects/linux_kernel_5_10
ARCH=x86_64
Release=ubuntu
Version=20

kernel_bin="$KERNEL_DIR/vmlinux"
rootfs_path="$KERNEL_DIR/zxm/rootfs/rootfs_${Release}-${Version}_${ARCH}.ext4"
#rootfs_path="/home/kingdo/CLionProjects/linux_kernel_5_14_2/ROOTFS-IMG/rootfs-2G.ext4"


# set the guest kernel
curl --unix-socket /tmp/firecracker.socket -i \
      -X PUT 'http://localhost/boot-source'   \
      -H 'Accept: application/json'           \
      -H 'Content-Type: application/json'     \
      -d "{
            \"kernel_image_path\": \"${kernel_bin}\",
            \"boot_args\": \"console=ttyS0 reboot=k panic=1 pci=off\"
       }"

# set the guest rootfs
curl --unix-socket /tmp/firecracker.socket -i \
  -X PUT 'http://localhost/drives/rootfs' \
  -H 'Accept: application/json'           \
  -H 'Content-Type: application/json'     \
  -d "{
        \"drive_id\": \"rootfs\",
        \"path_on_host\": \"${rootfs_path}\",
        \"is_root_device\": true,
        \"is_read_only\": false
   }"

# Configure resource
curl --unix-socket /tmp/firecracker.socket -i  \
  -X PUT 'http://localhost/machine-config' \
  -H 'Accept: application/json'            \
  -H 'Content-Type: application/json'      \
  -d '{
      "vcpu_count": 4,
      "mem_size_mib": 8192
  }'

# Installing the balloon device
amount_mib=0
deflate_on_oom=false
polling_interval=1
curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/balloon' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d "{
        \"amount_mib\": $amount_mib, \
        \"deflate_on_oom\": $deflate_on_oom, \
        \"stats_polling_interval_s\": $polling_interval \
    }"

# Installing the faascale_mem device
polling_interval=0
curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/faascale_mem' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d "{
        \"stats_polling_interval_s\": $polling_interval, \
        \"pre_alloc_mem\": true, \
        \"pre_tdp_fault\": true \
    }"

curl -X PUT \
  --unix-socket /tmp/firecracker.socket \
  http://localhost/network-interfaces/eth0 \
  -H accept:application/json \
  -H content-type:application/json \
  -d '{
    "iface_id": "eth0",
    "guest_mac": "AA:FC:00:00:00:01",
    "host_dev_name": "tap0"
}'

# start the guest machine
curl --unix-socket /tmp/firecracker.socket -i \
  -X PUT 'http://localhost/actions'       \
  -H  'Accept: application/json'          \
  -H  'Content-Type: application/json'    \
  -d '{
      "action_type": "InstanceStart"
   }'


