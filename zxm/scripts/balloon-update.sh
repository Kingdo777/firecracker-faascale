socket_location=/tmp/firecracker.socket
amount_mib=$1

curl --unix-socket $socket_location -i \
    -X PATCH 'http://localhost/balloon' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d "{
        \"amount_mib\": $amount_mib \
    }"
